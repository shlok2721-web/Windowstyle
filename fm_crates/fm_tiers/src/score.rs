//! Decayed visit scores with hysteresis, so a directory does not flap between tiers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Promote to hot when the decayed score reaches this value.
pub const PROMOTE_AT: f32 = 8.0;
/// Demote from hot only when the decayed score falls below this value.
pub const DEMOTE_AT: f32 = 3.0;
/// Score granted to seeded directories (XDG dirs, `~/.local/bin`, `$PATH` entries).
pub const SEED_SCORE: f32 = 10.0;
/// Score halves every this many seconds without a visit.
pub const HALF_LIFE_SECS: f64 = 7.0 * 24.0 * 3600.0;
/// At most this many directories are tracked; the coldest are pruned.
pub const MAX_TRACKED: usize = 4096;
/// At most this many directories are exported to disk.
pub const MAX_EXPORTED: usize = 512;

#[derive(Clone, Copy, Debug)]
struct Rec {
    score: f32,
    /// Unix seconds at which `score` was last brought up to date.
    at: u64,
    hot: bool,
}

fn decayed(score: f32, from: u64, now: u64) -> f32 {
    let dt = now.saturating_sub(from) as f64;
    (f64::from(score) * 0.5_f64.powf(dt / HALF_LIFE_SECS)) as f32
}

/// One persisted row of the tier table.
#[derive(Clone, Debug, PartialEq)]
pub struct TierRow {
    /// Directory path.
    pub path: PathBuf,
    /// Decayed score at export time.
    pub score: f32,
    /// Whether the directory was hot.
    pub hot: bool,
}

/// One persisted row in **raw** form: the score as stored and the time it was stored at,
/// *not* decayed to "now". Serialising raw rows makes the output identical between two
/// flushes when nothing changed, which is what lets the persistence layer skip the write.
#[derive(Clone, Debug, PartialEq)]
pub struct TierRaw {
    /// Directory path.
    pub path: PathBuf,
    /// Score as of `at`.
    pub score: f32,
    /// Unix seconds at which `score` was last brought up to date.
    pub at: u64,
    /// Whether the directory was hot.
    pub hot: bool,
}

/// Visit scores, pins and the hot flag with hysteresis.
#[derive(Debug, Default)]
pub struct TierTable {
    recs: HashMap<PathBuf, Rec>,
    pinned: HashSet<PathBuf>,
}

impl TierTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of tracked directories.
    pub fn len(&self) -> usize {
        self.recs.len()
    }

    /// Whether nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.recs.is_empty()
    }

    /// Whether `dir` is tracked at all.
    pub fn contains(&self, dir: &Path) -> bool {
        self.recs.contains_key(dir)
    }

    /// Pin a directory: always hot.
    pub fn pin(&mut self, dir: impl Into<PathBuf>) {
        self.pinned.insert(dir.into());
    }

    /// Remove a pin.
    pub fn unpin(&mut self, dir: &Path) {
        self.pinned.remove(dir);
    }

    /// Whether `dir` is pinned.
    pub fn is_pinned(&self, dir: &Path) -> bool {
        self.pinned.contains(dir)
    }

    /// All pinned directories.
    pub fn pins(&self) -> impl Iterator<Item = &PathBuf> {
        self.pinned.iter()
    }

    /// Current decayed score (0 if untracked).
    pub fn score(&self, dir: &Path, now: u64) -> f32 {
        self.recs
            .get(dir)
            .map_or(0.0, |r| decayed(r.score, r.at, now))
    }

    /// Whether `dir` is hot right now. Pure: applies hysteresis to the stored flag without
    /// mutating anything, so demotion happens lazily even if the directory is never visited.
    pub fn is_hot(&self, dir: &Path, now: u64) -> bool {
        if self.pinned.contains(dir) {
            return true;
        }
        let Some(r) = self.recs.get(dir) else {
            return false;
        };
        let s = decayed(r.score, r.at, now);
        if r.hot {
            s >= DEMOTE_AT
        } else {
            s >= PROMOTE_AT
        }
    }

    /// Record a user visit: decay, add 1.0, and commit any promotion/demotion.
    pub fn visit(&mut self, dir: &Path, now: u64) {
        self.bump(dir, 1.0, now);
    }

    /// Add `amount` to the decayed score and commit tier transitions.
    pub fn bump(&mut self, dir: &Path, amount: f32, now: u64) {
        let rec = self.recs.entry(dir.to_path_buf()).or_insert(Rec {
            score: 0.0,
            at: now,
            hot: false,
        });
        let s = decayed(rec.score, rec.at, now) + amount;
        rec.score = s;
        rec.at = now;
        if rec.hot {
            rec.hot = s >= DEMOTE_AT;
        } else {
            rec.hot = s >= PROMOTE_AT;
        }
        if self.recs.len() > MAX_TRACKED {
            self.prune(now);
        }
    }

    /// Give `dir` the seed score unless it is already tracked (restored state wins).
    pub fn seed(&mut self, dir: &Path, now: u64) {
        self.recs.entry(dir.to_path_buf()).or_insert(Rec {
            score: SEED_SCORE,
            at: now,
            hot: true,
        });
    }

    /// Hot directories (pinned first, then by score), at most `limit`. Also commits the
    /// lazy demotions so that the hot flag matches what [`is_hot`](Self::is_hot) reports.
    pub fn hot_dirs(&mut self, now: u64, limit: usize) -> Vec<PathBuf> {
        for r in self.recs.values_mut() {
            let s = decayed(r.score, r.at, now);
            r.hot = if r.hot {
                s >= DEMOTE_AT
            } else {
                s >= PROMOTE_AT
            };
        }
        let mut rows: Vec<(bool, f32, &PathBuf)> = self
            .recs
            .iter()
            .filter_map(|(p, r)| {
                let pinned = self.pinned.contains(p);
                (pinned || r.hot).then(|| (pinned, decayed(r.score, r.at, now), p))
            })
            .collect();
        for p in &self.pinned {
            if !self.recs.contains_key(p) {
                rows.push((true, 0.0, p));
            }
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(b.2)));
        rows.into_iter()
            .take(limit)
            .map(|(_, _, p)| p.clone())
            .collect()
    }

    /// Drop the coldest unpinned, non-hot rows until the table is back under its bound.
    fn prune(&mut self, now: u64) {
        let target = MAX_TRACKED * 3 / 4;
        let mut rows: Vec<(f32, PathBuf)> = self
            .recs
            .iter()
            .filter(|(p, r)| !r.hot && !self.pinned.contains(*p))
            .map(|(p, r)| (decayed(r.score, r.at, now), p.clone()))
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        let excess = self.recs.len().saturating_sub(target);
        for (_, p) in rows.into_iter().take(excess) {
            self.recs.remove(&p);
        }
    }

    /// Bounded snapshot for persistence: top [`MAX_EXPORTED`] rows with score ≥ 1.
    pub fn export(&self, now: u64) -> Vec<TierRow> {
        let mut rows: Vec<TierRow> = self
            .recs
            .iter()
            .map(|(p, r)| TierRow {
                path: p.clone(),
                score: decayed(r.score, r.at, now),
                hot: r.hot,
            })
            .filter(|row| row.score >= 1.0)
            .collect();
        rows.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.path.cmp(&b.path)));
        rows.truncate(MAX_EXPORTED);
        rows
    }

    /// Bounded snapshot in raw (undecayed) form: the top [`MAX_EXPORTED`] rows whose decayed
    /// score at `now` is at least 1, sorted by path so the output is deterministic.
    pub fn export_raw(&self, now: u64) -> Vec<TierRaw> {
        let mut rows: Vec<(f32, TierRaw)> = self
            .recs
            .iter()
            .map(|(p, r)| {
                (
                    decayed(r.score, r.at, now),
                    TierRaw {
                        path: p.clone(),
                        score: r.score,
                        at: r.at,
                        hot: r.hot,
                    },
                )
            })
            .filter(|(d, _)| *d >= 1.0)
            .collect();
        rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.path.cmp(&b.1.path)));
        rows.truncate(MAX_EXPORTED);
        let mut out: Vec<TierRaw> = rows.into_iter().map(|(_, row)| row).collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Restore raw rows (replaces same-path rows). A timestamp in the future is clamped to
    /// `now`, and non-finite or non-positive scores are ignored.
    pub fn import_raw(&mut self, rows: impl IntoIterator<Item = TierRaw>, now: u64) {
        for row in rows {
            if row.score.is_finite() && row.score > 0.0 {
                self.recs.insert(
                    row.path,
                    Rec {
                        score: row.score,
                        at: row.at.min(now),
                        hot: row.hot,
                    },
                );
            }
        }
    }

    /// Restore rows from persistence (replaces same-path rows).
    pub fn import(&mut self, rows: impl IntoIterator<Item = TierRow>, now: u64) {
        for row in rows {
            if row.score.is_finite() && row.score > 0.0 {
                self.recs.insert(
                    row.path,
                    Rec {
                        score: row.score,
                        at: now,
                        hot: row.hot,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 3600;

    #[test]
    fn raw_export_is_stable_over_time_and_round_trips() {
        let mut t = TierTable::new();
        for _ in 0..5 {
            t.visit(Path::new("/b"), 1000);
        }
        t.visit(Path::new("/a"), 1000);
        t.visit(Path::new("/a"), 1000);
        let first = t.export_raw(1000);
        let later = t.export_raw(1000 + DAY);
        assert_eq!(
            first, later,
            "raw rows do not change just because time passes"
        );
        assert_eq!(first[0].path, PathBuf::from("/a"), "sorted by path");

        let mut u = TierTable::new();
        u.import_raw(first, 2000);
        assert!((u.score(Path::new("/b"), 1000) - 5.0).abs() < 0.01);
        assert!(!u.is_hot(Path::new("/b"), 1000));
    }

    #[test]
    fn promotes_at_eight_and_not_before() {
        let mut t = TierTable::new();
        let d = Path::new("/d");
        for _ in 0..7 {
            t.visit(d, 1000);
        }
        assert!(!t.is_hot(d, 1000));
        t.visit(d, 1000);
        assert!(t.is_hot(d, 1000));
    }

    #[test]
    fn hysteresis_keeps_hot_between_three_and_eight() {
        let mut t = TierTable::new();
        let d = Path::new("/d");
        for _ in 0..8 {
            t.visit(d, 0);
        }
        // After one half-life the score is 4: below the promote bar, above the demote bar.
        let later = HALF_LIFE_SECS as u64;
        assert!(t.is_hot(d, later), "stays hot while score >= 3");
        // A never-hot directory with the same score of 4 is not hot.
        let mut cold = TierTable::new();
        for _ in 0..4 {
            cold.visit(d, 0);
        }
        assert!(!cold.is_hot(d, 0));
        // After two half-lives the score is 2: demoted.
        assert!(!t.is_hot(d, 2 * later));
    }

    #[test]
    fn demotion_is_committed_and_no_flapping_on_revisit() {
        let mut t = TierTable::new();
        let d = Path::new("/d");
        for _ in 0..8 {
            t.visit(d, 0);
        }
        let far = 60 * DAY;
        assert!(!t.is_hot(d, far));
        t.visit(d, far); // one visit after a long gap must not re-promote
        assert!(!t.is_hot(d, far));
    }

    #[test]
    fn pins_are_always_hot_and_listed_first() {
        let mut t = TierTable::new();
        t.pin("/pinned");
        for _ in 0..9 {
            t.visit(Path::new("/busy"), 5);
        }
        assert!(t.is_hot(Path::new("/pinned"), 10 * 365 * DAY));
        let hot = t.hot_dirs(5, 10);
        assert_eq!(hot[0], PathBuf::from("/pinned"));
        assert_eq!(hot[1], PathBuf::from("/busy"));
        assert_eq!(t.hot_dirs(5, 1).len(), 1);
    }

    #[test]
    fn seed_does_not_override_restored_state() {
        let mut t = TierTable::new();
        t.import(
            [TierRow {
                path: "/a".into(),
                score: 1.5,
                hot: false,
            }],
            0,
        );
        t.seed(Path::new("/a"), 0);
        assert!(!t.is_hot(Path::new("/a"), 0));
        t.seed(Path::new("/b"), 0);
        assert!(t.is_hot(Path::new("/b"), 0));
    }

    #[test]
    fn tracking_is_bounded() {
        let mut t = TierTable::new();
        for i in 0..(MAX_TRACKED + 50) {
            t.visit(Path::new(&format!("/d{i}")), 0);
        }
        assert!(t.len() <= MAX_TRACKED, "{}", t.len());
    }

    #[test]
    fn export_import_roundtrip_is_bounded_and_decayed() {
        let mut t = TierTable::new();
        for i in 0..(MAX_EXPORTED + 100) {
            for _ in 0..2 {
                t.visit(Path::new(&format!("/d{i}")), 0);
            }
        }
        let rows = t.export(0);
        assert_eq!(rows.len(), MAX_EXPORTED);
        let mut u = TierTable::new();
        u.import(rows.clone(), 0);
        assert_eq!(u.len(), MAX_EXPORTED);
        // Rows that decayed below 1.0 are not exported.
        assert!(t.export(2 * HALF_LIFE_SECS as u64).is_empty());
    }
}
