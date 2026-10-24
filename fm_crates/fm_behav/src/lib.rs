//! Behaviour mapping, RAM first. **This crate performs no I/O.**
//!
//! ```text
//! events (RAM ring, ~1 MB, 16 B each) ──fold──> aggregates (RAM) ──rare flush──> small text file
//! ```
//!
//! * **Events** `{ts, dir_id, action}` live in a fixed-size ring and are never written out.
//! * **Aggregates** are what survives a fold, and all of them are bounded:
//!   * decayed *frecency* per path (14-day half-life),
//!   * a sparse `dir → next_dir` table keeping only the top [`MAX_SUCCESSORS`] successors,
//!   * pins,
//!   * the tier scores handed over by `fm_tiers`.
//! * **Pruning at fold time** keeps every aggregate under a hard cap, so the persisted file
//!   cannot grow without limit.
//! * [`Behavior::to_bytes`] is **deterministic**: raw (undecayed) scores plus the time they
//!   were stored, sorted by path. Two encodings of an unchanged state are byte-identical,
//!   which is what lets `fm_persist` skip the write.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod codec;
mod ring;

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub use fm_tiers::TierRaw;
pub use ring::{Action, EVENT_BYTES, Event, EventRing};

use ring::PathInterner;

/// Default ring capacity: 65 536 events × 16 bytes = 1 MiB.
pub const DEFAULT_RING_EVENTS: usize = 65_536;
/// Frecency score halves every this many seconds without activity.
pub const FRECENCY_HALF_LIFE_SECS: f64 = 14.0 * 24.0 * 3600.0;
/// Successors kept per directory in the next-folder table.
pub const MAX_SUCCESSORS: usize = 4;
/// Maximum number of paths with a frecency score.
pub const MAX_FRECENCY: usize = 384;
/// Maximum number of directories with successors.
pub const MAX_NEXT_ROWS: usize = 96;
/// Maximum number of pins.
pub const MAX_PINS: usize = 64;
/// Maximum number of tier rows kept for persistence.
pub const MAX_TIER_ROWS: usize = 256;
/// Estimated encoded size above which fold-time pruning removes the weakest rows.
pub const SIZE_BUDGET_BYTES: usize = 96 * 1024;

const HEADER: &str = "fmbehav 1";
/// Frecency entries below this decayed score are dropped at fold time.
const FORGET_BELOW: f32 = 0.05;
/// Older successors fade slightly with each new transition so recent habits win.
const SUCCESSOR_FADE: f32 = 0.98;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Frec {
    score: f32,
    at: u64,
}

fn decayed(score: f32, from: u64, now: u64) -> f32 {
    let dt = now.saturating_sub(from) as f64;
    (f64::from(score) * 0.5_f64.powf(dt / FRECENCY_HALF_LIFE_SECS)) as f32
}

#[derive(Clone, Debug, PartialEq)]
struct Succ {
    to: PathBuf,
    score: f32,
}

/// Why a state file could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// Not UTF-8 text.
    NotText,
    /// The header line is missing or names an unknown version.
    UnknownFormat,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotText => f.write_str("behaviour state is not valid text"),
            Self::UnknownFormat => f.write_str("behaviour state has an unknown format or version"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Counters describing the current state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BehavStats {
    /// Events recorded since start.
    pub recorded: u64,
    /// Events folded into aggregates since start.
    pub folded: u64,
    /// Events waiting in the ring.
    pub pending: usize,
    /// Events lost because the ring was overwritten.
    pub overwritten: u64,
    /// Paths with a frecency score.
    pub frecency_rows: usize,
    /// Directories with successors.
    pub next_rows: usize,
    /// Pinned directories.
    pub pins: usize,
    /// Tier rows held for persistence.
    pub tier_rows: usize,
}

/// The behaviour layer: event ring plus folded aggregates.
#[derive(Debug)]
pub struct Behavior {
    ring: EventRing,
    ids: PathInterner,
    frecency: HashMap<PathBuf, Frec>,
    next: HashMap<PathBuf, Vec<Succ>>,
    pins: BTreeSet<PathBuf>,
    tiers: Vec<TierRaw>,
    last_dir: Option<PathBuf>,
    dirty: f32,
    recorded: u64,
    folded: u64,
}

impl Default for Behavior {
    fn default() -> Self {
        Self::new()
    }
}

impl Behavior {
    /// An empty state with the default ring size.
    pub fn new() -> Self {
        Self::with_ring_capacity(DEFAULT_RING_EVENTS)
    }

    /// An empty state whose ring holds `events` events.
    pub fn with_ring_capacity(events: usize) -> Self {
        Self {
            ring: EventRing::with_capacity(events),
            ids: PathInterner::default(),
            frecency: HashMap::new(),
            next: HashMap::new(),
            pins: BTreeSet::new(),
            tiers: Vec::new(),
            last_dir: None,
            dirty: 0.0,
            recorded: 0,
            folded: 0,
        }
    }

    // ------------------------------------------------------------------ recording

    /// Record one action at unix time `ts`. Pin state changes are applied immediately; the
    /// corresponding events remain in the ring for accounting. When the ring is full it is
    /// folded first, so no event is lost in normal operation.
    pub fn record(&mut self, action: Action, path: &Path, ts: u64) {
        // Commit pin mutations before a possible fold. Those mutations must not depend on
        // historical event replay, because callers can observe pins immediately.
        match action {
            Action::Pin => {
                if !self.pins.contains(path) && self.pins.len() < MAX_PINS {
                    self.pins.insert(path.to_path_buf());
                    self.dirty += 2.0;
                }
            }
            Action::Unpin => {
                if self.pins.remove(path) {
                    self.dirty += 2.0;
                }
            }
            Action::OpenDir | Action::OpenFile | Action::Back => {}
        }

        if self.ring.is_full() {
            self.fold(ts);
        }
        let dir_id = match self.ids.intern(path) {
            Some(id) => id,
            None => {
                // This is only reachable for a ring containing more than u32::MAX distinct
                // paths. Folding releases those ids and lets this event start a new id cycle.
                self.fold(ts);
                let Some(id) = self.ids.intern(path) else {
                    // With an empty interner, even the first id must be representable.
                    return;
                };
                id
            }
        };
        self.ring.push(Event { ts, dir_id, action });
        self.recorded = self.recorded.saturating_add(1);
    }

    /// The user opened (navigated into) a directory.
    pub fn open_dir(&mut self, dir: &Path, ts: u64) {
        self.record(Action::OpenDir, dir, ts);
    }

    /// The user opened a file.
    pub fn open_file(&mut self, file: &Path, ts: u64) {
        self.record(Action::OpenFile, file, ts);
    }

    /// The user went back to a directory through history.
    pub fn back(&mut self, dir: &Path, ts: u64) {
        self.record(Action::Back, dir, ts);
    }

    /// Pin a directory. Takes effect immediately. Returns `false` when the pin limit is
    /// reached (an existing pin is a no-op that returns `true`).
    pub fn pin(&mut self, dir: &Path, ts: u64) -> bool {
        if self.pins.contains(dir) {
            return true;
        }
        if self.pins.len() >= MAX_PINS {
            return false;
        }
        self.record(Action::Pin, dir, ts);
        true
    }

    /// Remove a pin. Takes effect immediately. Returns whether a pin was removed.
    pub fn unpin(&mut self, dir: &Path, ts: u64) -> bool {
        if !self.pins.contains(dir) {
            return false;
        }
        self.record(Action::Unpin, dir, ts);
        true
    }

    // ------------------------------------------------------------------ folding

    /// Events waiting to be folded.
    pub fn pending(&self) -> usize {
        self.ring.len()
    }

    /// Fold every pending event into the aggregates, prune them to their caps and release the
    /// event paths. Returns the number of events folded.
    pub fn fold(&mut self, now: u64) -> usize {
        // Pop in place instead of collecting a second Vec of every pending event. In
        // particular, a full default ring otherwise caused an unnecessary ~1 MiB allocation
        // and copy on every fold.
        let count = self.ring.len();
        while let Some(event) = self.ring.pop_front() {
            let Some(path) = self.ids.get_shared(event.dir_id) else {
                continue;
            };
            self.apply(event.action, path.as_ref(), event.ts);
        }
        self.ids.clear();
        self.folded = self.folded.saturating_add(count as u64);
        self.prune(now);
        count
    }

    fn apply(&mut self, action: Action, path: &Path, ts: u64) {
        match action {
            Action::OpenDir => {
                self.bump(path, 1.0, ts);
                if let Some(prev) = self.last_dir.take() {
                    if prev.as_path() != path {
                        self.note_transition(prev, path);
                        self.last_dir = Some(path.to_path_buf());
                    } else {
                        // Reopening the current directory isn't a transition. Reuse the stored
                        // path only when its exact OS-string representation also matches;
                        // Path equality may treat alternate separator spellings as equivalent,
                        // while the previous implementation retained the latest spelling.
                        self.last_dir = if prev.as_os_str() == path.as_os_str() {
                            Some(prev)
                        } else {
                            Some(path.to_path_buf())
                        };
                    }
                } else {
                    self.last_dir = Some(path.to_path_buf());
                }
                self.dirty += 1.0;
            }
            Action::Back => {
                self.bump(path, 0.5, ts);
                if !self
                    .last_dir
                    .as_ref()
                    .is_some_and(|last| last.as_os_str() == path.as_os_str())
                {
                    self.last_dir = Some(path.to_path_buf());
                }
                self.dirty += 0.5;
            }
            Action::OpenFile => {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    self.bump(parent, 0.5, ts);
                }
                self.bump(path, 1.0, ts);
                self.dirty += 1.0;
            }
            // record() commits pin mutations before queuing events. Replaying their history
            // here is not idempotent when an unpin has already changed the current state and
            // the ring fills before the matching event is enqueued. Keep these events for
            // accounting, but never replay state that has already been committed.
            Action::Pin | Action::Unpin => {}
        }
    }

    fn bump(&mut self, path: &Path, weight: f32, ts: u64) {
        // HashMap::entry(path.to_path_buf()) allocates a new path key even for a path already
        // present in the table. The common repeat-navigation case can update in place.
        if let Some(entry) = self.frecency.get_mut(path) {
            let at = entry.at.max(ts);
            entry.score = decayed(entry.score, entry.at, at) + weight;
            entry.at = at;
        } else {
            self.frecency.insert(
                path.to_path_buf(),
                Frec {
                    score: weight,
                    at: ts,
                },
            );
        }
    }

    fn note_transition(&mut self, from: PathBuf, to: &Path) {
        let row = self.next.entry(from).or_default();
        for succ in row.iter_mut() {
            succ.score *= SUCCESSOR_FADE;
        }
        match row.iter_mut().find(|s| s.to == to) {
            Some(succ) => succ.score += 1.0,
            None => row.push(Succ {
                to: to.to_path_buf(),
                score: 1.0,
            }),
        }
        row.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.to.cmp(&b.to)));
        row.truncate(MAX_SUCCESSORS);
    }

    fn prune(&mut self, now: u64) {
        self.frecency
            .retain(|_, f| decayed(f.score, f.at, now) >= FORGET_BELOW);
        if self.frecency.len() > MAX_FRECENCY {
            self.trim_frecency(MAX_FRECENCY * 3 / 4, now);
        }
        if self.next.len() > MAX_NEXT_ROWS {
            self.trim_next(MAX_NEXT_ROWS * 3 / 4);
        }
        // Keep the encoded size within budget even with very long paths.
        while self.estimated_bytes() > SIZE_BUDGET_BYTES
            && (self.frecency.len() > 16 || self.next.len() > 8)
        {
            let frecency_target = self.frecency.len() * 7 / 8;
            let next_target = self.next.len() * 7 / 8;
            self.trim_frecency(frecency_target, now);
            self.trim_next(next_target);
        }
    }

    fn trim_frecency(&mut self, keep: usize, now: u64) {
        if self.frecency.len() <= keep {
            return;
        }
        let mut rows: Vec<(f32, PathBuf)> = self
            .frecency
            .iter()
            .map(|(p, f)| (decayed(f.score, f.at, now), p.clone()))
            .collect();
        // Keep the same path tie-break used by top(): lower paths rank ahead on equal scores,
        // so trim equal-score rows in reverse path order.
        rows.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
        let excess = self.frecency.len() - keep;
        for (_, path) in rows.into_iter().take(excess) {
            self.frecency.remove(&path);
        }
    }

    fn trim_next(&mut self, keep: usize) {
        if self.next.len() <= keep {
            return;
        }
        let mut rows: Vec<(f32, PathBuf)> = self
            .next
            .iter()
            .map(|(p, row)| (row.iter().map(|s| s.score).sum::<f32>(), p.clone()))
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
        let excess = self.next.len() - keep;
        for (_, path) in rows.into_iter().take(excess) {
            self.next.remove(&path);
        }
    }

    /// Cheap estimate of the encoded size (path lengths plus fixed per-row overhead).
    fn estimated_bytes(&self) -> usize {
        let path_len = |p: &Path| p.as_os_str().len();
        let pins: usize = self.pins.iter().map(|p| path_len(p) + 4).sum();
        let frecency: usize = self.frecency.keys().map(|p| path_len(p) + 32).sum();
        let next: usize = self
            .next
            .iter()
            .map(|(from, row)| {
                row.iter()
                    .map(|s| path_len(from) + path_len(&s.to) + 24)
                    .sum::<usize>()
            })
            .sum();
        let tiers: usize = self.tiers.iter().map(|t| path_len(&t.path) + 40).sum();
        pins + frecency + next + tiers
    }

    // ------------------------------------------------------------------ queries

    /// Frecency of `path` at time `now` (reflects folded events only).
    pub fn frecency(&self, path: &Path, now: u64) -> f32 {
        self.frecency
            .get(path)
            .map_or(0.0, |f| decayed(f.score, f.at, now))
    }

    /// The `limit` highest-frecency paths, best first.
    pub fn top(&self, now: u64, limit: usize) -> Vec<(PathBuf, f32)> {
        let mut rows: Vec<(PathBuf, f32)> = self
            .frecency
            .iter()
            .map(|(p, f)| (p.clone(), decayed(f.score, f.at, now)))
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        rows.truncate(limit);
        rows
    }

    /// The most likely next directories after `dir`, best first (at most `limit`).
    pub fn predict_next(&self, dir: &Path, limit: usize) -> Vec<PathBuf> {
        self.next
            .get(dir)
            .map(|row| row.iter().take(limit).map(|s| s.to.clone()).collect())
            .unwrap_or_default()
    }

    /// Whether `dir` is pinned.
    pub fn is_pinned(&self, dir: &Path) -> bool {
        self.pins.contains(dir)
    }

    /// Pinned directories in path order.
    pub fn pins(&self) -> impl Iterator<Item = &PathBuf> {
        self.pins.iter()
    }

    /// Counters describing the current state.
    pub fn stats(&self) -> BehavStats {
        BehavStats {
            recorded: self.recorded,
            folded: self.folded,
            pending: self.ring.len(),
            overwritten: self.ring.overwritten(),
            frecency_rows: self.frecency.len(),
            next_rows: self.next.len(),
            pins: self.pins.len(),
            tier_rows: self.tiers.len(),
        }
    }

    // ------------------------------------------------------------------ tiers and dirtiness

    /// Hand over the current tier scores (`fm_tiers::TierTable::export_raw`). Only the
    /// strongest [`MAX_TIER_ROWS`] rows are kept, sorted by path. A real change counts as
    /// one unit of dirty weight.
    pub fn set_tiers(&mut self, mut rows: Vec<TierRaw>) {
        // This crate's text format accepts only finite, non-negative scores. Apply the same
        // contract on the in-memory API so NaN/negative values cannot create unstable ordering
        // or values that the next load silently discards.
        rows.retain(|row| row.score.is_finite() && row.score >= 0.0);
        if rows.len() > MAX_TIER_ROWS {
            rows.sort_by(|a, b| {
                b.score
                    .total_cmp(&a.score)
                    .then_with(|| a.path.cmp(&b.path))
            });
            rows.truncate(MAX_TIER_ROWS);
        }
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        if rows != self.tiers {
            self.tiers = rows;
            self.dirty += 1.0;
        }
    }

    /// Tier rows to restore into `fm_tiers::TierTable::import_raw`.
    pub fn tiers(&self) -> &[TierRaw] {
        &self.tiers
    }

    /// Return and reset the change weight accumulated since the last call; feed it to
    /// `fm_persist::Flusher::mark_dirty`.
    pub fn take_dirty(&mut self) -> f32 {
        std::mem::take(&mut self.dirty)
    }

    // ------------------------------------------------------------------ encoding

    /// Deterministic text encoding of the aggregates (the ring is not included). Call
    /// [`fold`](Self::fold) first so pending events are part of it.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = String::with_capacity(
            self.estimated_bytes()
                .saturating_add(HEADER.len() + 1)
                .max(4096),
        );
        out.push_str(HEADER);
        out.push('\n');
        for pin in &self.pins {
            let _ = writeln!(out, "P\t{}", codec::escape(pin));
        }
        let mut frecency: Vec<(&PathBuf, &Frec)> = self.frecency.iter().collect();
        frecency.sort_by(|a, b| a.0.cmp(b.0));
        for (path, f) in frecency {
            let _ = writeln!(out, "F\t{:.3}\t{}\t{}", f.score, f.at, codec::escape(path));
        }
        let mut next: Vec<(&PathBuf, &Vec<Succ>)> = self.next.iter().collect();
        next.sort_by(|a, b| a.0.cmp(b.0));
        for (from, row) in next {
            for succ in row {
                let _ = writeln!(
                    out,
                    "N\t{}\t{}\t{:.3}",
                    codec::escape(from),
                    codec::escape(&succ.to),
                    succ.score
                );
            }
        }
        for tier in &self.tiers {
            let _ = writeln!(
                out,
                "T\t{:.3}\t{}\t{}\t{}",
                tier.score,
                tier.at,
                u8::from(tier.hot),
                codec::escape(&tier.path)
            );
        }
        out.into_bytes()
    }

    /// Decode a state written by [`to_bytes`](Self::to_bytes). Individual malformed lines are
    /// skipped; a wrong header rejects the whole file. Caps are re-applied at `now`.
    pub fn from_bytes(bytes: &[u8], now: u64) -> Result<Self, LoadError> {
        let text = std::str::from_utf8(bytes).map_err(|_| LoadError::NotText)?;
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(LoadError::UnknownFormat);
        }
        let mut state = Self::new();
        let mut current_next_source: Option<PathBuf> = None;
        for line in lines {
            // Parse fields with a stack-only iterator. Collecting a Vec for every line adds
            // one heap allocation per row when loading a perfectly ordinary behaviour file.
            let mut fields = line.split('\t');
            let tag = fields.next();
            let a = fields.next();
            let b = fields.next();
            let c = fields.next();
            let d = fields.next();
            if fields.next().is_some() {
                continue;
            }
            match (tag, a, b, c, d) {
                (Some("P"), Some(path), None, None, None) => {
                    if let Some(path) = codec::unescape(path) {
                        if state.pins.len() < MAX_PINS {
                            state.pins.insert(path);
                        }
                    }
                }
                (Some("F"), Some(score), Some(at), Some(path), None) => {
                    if let (Some(score), Ok(at), Some(path)) = (
                        codec::parse_score(score),
                        at.parse::<u64>(),
                        codec::unescape(path),
                    ) {
                        state.frecency.insert(
                            path,
                            Frec {
                                score,
                                at: at.min(now),
                            },
                        );
                        // As soon as the same threshold as fold-time pruning is crossed, keep
                        // only the strongest candidates. This prevents a very large or corrupt
                        // input from temporarily materializing an unbounded frecency map.
                        if state.frecency.len() > MAX_FRECENCY {
                            state.trim_frecency(MAX_FRECENCY * 3 / 4, now);
                        }
                    }
                }
                (Some("N"), Some(from), Some(to), Some(score), None) => {
                    if let (Some(from), Some(to), Some(score)) = (
                        codec::unescape(from),
                        codec::unescape(to),
                        codec::parse_score(score),
                    ) {
                        // The encoder writes every source's successor row contiguously. Trim
                        // between source groups so the current row is never truncated halfway
                        // through its serialized successors.
                        if current_next_source.as_ref() != Some(&from) {
                            if state.next.len() > MAX_NEXT_ROWS {
                                state.trim_next(MAX_NEXT_ROWS * 3 / 4);
                            }
                            current_next_source = Some(from.clone());
                        }
                        let row = state.next.entry(from).or_default();
                        if let Some(existing) = row.iter_mut().find(|s| s.to == to) {
                            // Tolerate duplicate successor lines without returning duplicate
                            // predictions. Valid files never need this branch.
                            if score.total_cmp(&existing.score).is_gt() {
                                existing.score = score;
                            }
                        } else if row.len() < MAX_SUCCESSORS {
                            row.push(Succ { to, score });
                        } else if let Some((weakest_index, weakest)) =
                            row.iter().enumerate().min_by(|(_, a), (_, b)| {
                                a.score.total_cmp(&b.score).then_with(|| b.to.cmp(&a.to))
                            })
                        {
                            // As with tiers, don't let a weak early line crowd out a stronger
                            // successor that appears later in a non-canonical input file.
                            let score_order = score.total_cmp(&weakest.score);
                            if score_order.is_gt() || (score_order.is_eq() && to < weakest.to) {
                                row[weakest_index] = Succ { to, score };
                            }
                        }
                    }
                }
                (Some("T"), Some(score), Some(at), Some(hot), Some(path)) => {
                    if let (Some(score), Ok(at), Some(path)) = (
                        codec::parse_score(score),
                        at.parse::<u64>(),
                        codec::unescape(path),
                    ) {
                        if hot == "0" || hot == "1" {
                            let tier = TierRaw {
                                path,
                                score,
                                at: at.min(now),
                                hot: hot == "1",
                            };
                            if state.tiers.len() < MAX_TIER_ROWS {
                                state.tiers.push(tier);
                            } else if let Some((weakest_index, weakest)) =
                                state.tiers.iter().enumerate().min_by(|(_, a), (_, b)| {
                                    a.score
                                        .total_cmp(&b.score)
                                        .then_with(|| b.path.cmp(&a.path))
                                })
                            {
                                // Keep the strongest rows regardless of input ordering. This
                                // matches set_tiers() instead of silently favoring the first
                                // 256 paths in a large or hand-edited state file.
                                let score_order = tier.score.total_cmp(&weakest.score);
                                if score_order.is_gt()
                                    || (score_order.is_eq() && tier.path < weakest.path)
                                {
                                    state.tiers[weakest_index] = tier;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        for row in state.next.values_mut() {
            row.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.to.cmp(&b.to)));
        }
        state.tiers.sort_by(|a, b| a.path.cmp(&b.path));
        state.prune(now);
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 3600;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn folding_builds_frecency_and_keeps_the_ring_empty() {
        let mut b = Behavior::new();
        b.open_dir(Path::new("/a"), 100);
        b.open_dir(Path::new("/a"), 100);
        b.open_dir(Path::new("/b"), 100);
        assert_eq!(b.pending(), 3);
        assert_eq!(
            b.frecency(Path::new("/a"), 100),
            0.0,
            "unfolded events are not visible yet"
        );
        assert_eq!(b.fold(100), 3);
        assert_eq!(b.pending(), 0);
        assert!((b.frecency(Path::new("/a"), 100) - 2.0).abs() < 1e-4);
        assert!((b.frecency(Path::new("/b"), 100) - 1.0).abs() < 1e-4);
        assert_eq!(b.top(100, 1)[0].0, p("/a"));
    }

    #[test]
    fn frecency_decays_with_a_fourteen_day_half_life() {
        let mut b = Behavior::new();
        for _ in 0..4 {
            b.open_dir(Path::new("/a"), 0);
        }
        b.fold(0);
        let after = b.frecency(Path::new("/a"), 14 * DAY);
        assert!((after - 2.0).abs() < 0.01, "got {after}");
    }

    #[test]
    fn opening_a_file_credits_the_file_and_half_credits_its_folder() {
        let mut b = Behavior::new();
        b.open_file(Path::new("/d/x.txt"), 10);
        b.fold(10);
        assert!((b.frecency(Path::new("/d/x.txt"), 10) - 1.0).abs() < 1e-4);
        assert!((b.frecency(Path::new("/d"), 10) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn transitions_keep_only_the_top_successors() {
        let mut b = Behavior::new();
        // /a -> /b three times, /a -> /c once, then /a -> four other folders once each.
        for _ in 0..3 {
            b.open_dir(Path::new("/a"), 1);
            b.open_dir(Path::new("/b"), 1);
        }
        b.open_dir(Path::new("/a"), 1);
        b.open_dir(Path::new("/c"), 1);
        for extra in ["/d", "/e", "/f", "/g"] {
            b.open_dir(Path::new("/a"), 1);
            b.open_dir(Path::new(extra), 1);
        }
        b.fold(1);
        let next = b.predict_next(Path::new("/a"), 10);
        assert_eq!(next.len(), MAX_SUCCESSORS);
        assert_eq!(next[0], p("/b"), "the most frequent successor ranks first");
        assert!(b.predict_next(Path::new("/zzz"), 3).is_empty());
    }

    #[test]
    fn back_is_neutral_for_transitions_and_reopening_the_same_dir_is_not_a_transition() {
        let mut b = Behavior::new();
        b.open_dir(Path::new("/a"), 1);
        b.open_dir(Path::new("/a"), 2);
        b.back(Path::new("/a"), 3);
        b.fold(3);
        assert!(b.predict_next(Path::new("/a"), 3).is_empty());
        assert!((b.frecency(Path::new("/a"), 3) - 2.5).abs() < 1e-3);
    }

    #[test]
    fn pins_apply_immediately_and_survive_folding() {
        let mut b = Behavior::new();
        assert!(b.pin(Path::new("/p"), 5));
        assert!(b.is_pinned(Path::new("/p")));
        b.fold(5);
        assert!(b.is_pinned(Path::new("/p")));
        assert!(b.unpin(Path::new("/p"), 6));
        b.fold(6);
        assert!(!b.is_pinned(Path::new("/p")));
        assert!(!b.unpin(Path::new("/p"), 7), "nothing to remove");
    }

    #[test]
    fn pin_limit_is_enforced() {
        let mut b = Behavior::new();
        for i in 0..MAX_PINS {
            assert!(b.pin(Path::new(&format!("/p{i}")), 1));
        }
        assert!(!b.pin(Path::new("/one-too-many"), 1));
        assert_eq!(b.pins().count(), MAX_PINS);
    }

    #[test]
    fn queued_pin_history_cannot_undo_an_immediate_unpin_when_the_ring_fills() {
        let mut b = Behavior::with_ring_capacity(1);
        let pinned = Path::new("/pinned");

        assert!(b.pin(pinned, 1));
        assert!(b.is_pinned(pinned));
        assert_eq!(b.pending(), 1);

        // `record(Unpin)` must first fold the full ring. The queued historical Pin must not
        // restore the pin after unpin() has already committed its state change.
        assert!(b.unpin(pinned, 2));
        assert!(!b.is_pinned(pinned));
        assert_eq!(b.pending(), 1);

        b.fold(2);
        assert!(!b.is_pinned(pinned));
    }

    #[test]
    fn pin_state_is_consistent_when_another_action_forces_a_fold() {
        let mut b = Behavior::with_ring_capacity(1);
        b.open_dir(Path::new("/visited"), 1);
        assert_eq!(b.pending(), 1);

        assert!(b.pin(Path::new("/pinned"), 2));
        assert!(b.is_pinned(Path::new("/pinned")));
        assert_eq!(b.pending(), 1);
        b.fold(2);
        assert!(b.is_pinned(Path::new("/pinned")));
    }

    #[test]
    fn a_full_ring_folds_itself_instead_of_losing_events() {
        let mut b = Behavior::with_ring_capacity(4);
        for i in 0..10 {
            b.open_dir(Path::new("/a"), i);
        }
        b.fold(10);
        assert_eq!(b.stats().overwritten, 0);
        assert!(b.frecency(Path::new("/a"), 10) > 9.0);
    }

    #[test]
    fn aggregates_stay_within_their_caps() {
        let mut b = Behavior::with_ring_capacity(64);
        for i in 0..(MAX_FRECENCY * 3) {
            b.open_dir(Path::new(&format!("/dir{i}")), 1000 + i as u64);
        }
        b.fold(1000 + (MAX_FRECENCY * 3) as u64);
        let s = b.stats();
        assert!(s.frecency_rows <= MAX_FRECENCY, "{}", s.frecency_rows);
        assert!(s.next_rows <= MAX_NEXT_ROWS, "{}", s.next_rows);
    }

    #[test]
    fn stale_entries_are_forgotten() {
        let mut b = Behavior::new();
        b.open_dir(Path::new("/old"), 0);
        b.fold(0);
        b.open_dir(Path::new("/new"), 400 * DAY);
        b.fold(400 * DAY);
        assert_eq!(b.frecency(Path::new("/old"), 400 * DAY), 0.0);
        assert_eq!(b.stats().frecency_rows, 1);
    }

    #[test]
    fn encoding_is_deterministic_and_round_trips() {
        let mut b = Behavior::new();
        b.open_dir(Path::new("/home/u/A b"), 100);
        b.open_dir(Path::new("/home/u/tab\there"), 101);
        b.open_file(Path::new("/home/u/100% file.txt"), 102);
        b.pin(Path::new("/home/u/pinned"), 103);
        b.set_tiers(vec![TierRaw {
            path: p("/home/u/Docs"),
            score: 9.5,
            at: 90,
            hot: true,
        }]);
        b.fold(110);
        let first = b.to_bytes();
        assert_eq!(first, b.to_bytes(), "same state, same bytes");

        let loaded = Behavior::from_bytes(&first, 110).unwrap();
        assert_eq!(
            loaded.to_bytes(),
            first,
            "decode then encode is the identity"
        );
        assert!(loaded.is_pinned(Path::new("/home/u/pinned")));
        assert_eq!(loaded.tiers().len(), 1);
        assert_eq!(
            loaded.predict_next(Path::new("/home/u/A b"), 1),
            [p("/home/u/tab\there")]
        );
    }

    #[test]
    fn time_passing_does_not_change_the_encoding() {
        let mut b = Behavior::new();
        b.open_dir(Path::new("/a"), 100);
        b.fold(100);
        let at_100 = b.to_bytes();
        b.fold(100 + DAY); // nothing pending, nothing recent enough to be forgotten
        assert_eq!(b.to_bytes(), at_100);
    }

    #[test]
    fn bad_input_is_rejected_or_skipped() {
        assert_eq!(
            Behavior::from_bytes(b"\xff\xfe", 0).unwrap_err(),
            LoadError::NotText
        );
        assert_eq!(
            Behavior::from_bytes(b"fmbehav 9\n", 0).unwrap_err(),
            LoadError::UnknownFormat
        );
        let ok = Behavior::from_bytes(
            b"fmbehav 1\nF\tnan\t1\t/x\nF\t2.000\tbad\t/y\nP\t/keep\ngarbage\n",
            10,
        )
        .unwrap();
        assert!(ok.is_pinned(Path::new("/keep")));
        assert_eq!(ok.stats().frecency_rows, 0);
    }

    #[test]
    fn duplicate_successor_rows_do_not_create_duplicate_predictions() {
        let loaded = Behavior::from_bytes(
            b"fmbehav 1\nN\t/a\t/b\t1.000\nN\t/a\t/b\t2.000\nN\t/a\t/c\t1.500\n",
            10,
        )
        .unwrap();
        assert_eq!(loaded.predict_next(Path::new("/a"), 10), [p("/b"), p("/c")]);
    }

    #[test]
    fn loading_an_oversized_successor_row_keeps_the_strongest_destinations() {
        let mut encoded = String::from("fmbehav 1\n");
        for (destination, score) in [("/b", 1), ("/c", 2), ("/d", 3), ("/e", 4), ("/f", 9)] {
            writeln!(encoded, "N\t/a\t{destination}\t{score}.000").unwrap();
        }

        let loaded = Behavior::from_bytes(encoded.as_bytes(), 10).unwrap();
        assert_eq!(
            loaded.predict_next(Path::new("/a"), MAX_SUCCESSORS),
            [p("/f"), p("/e"), p("/d"), p("/c")]
        );
    }

    #[test]
    fn invalid_tier_scores_are_not_persisted() {
        let mut b = Behavior::new();
        b.set_tiers(vec![
            TierRaw {
                path: p("/nan"),
                score: f32::NAN,
                at: 1,
                hot: false,
            },
            TierRaw {
                path: p("/negative"),
                score: -1.0,
                at: 1,
                hot: false,
            },
            TierRaw {
                path: p("/valid"),
                score: 2.0,
                at: 1,
                hot: true,
            },
        ]);
        assert_eq!(b.tiers().len(), 1);
        assert_eq!(b.tiers()[0].path, p("/valid"));
        let loaded = Behavior::from_bytes(&b.to_bytes(), 1).unwrap();
        assert_eq!(loaded.tiers(), b.tiers());
    }

    #[test]
    fn loading_oversized_tier_tables_keeps_the_strongest_rows() {
        let mut encoded = String::from("fmbehav 1\n");
        for score in 0..(MAX_TIER_ROWS + 12) {
            writeln!(encoded, "T\t{score}.000\t1\t0\t/tier{score:03}").unwrap();
        }

        let loaded = Behavior::from_bytes(encoded.as_bytes(), 1).unwrap();
        assert_eq!(loaded.tiers().len(), MAX_TIER_ROWS);
        assert!(loaded.tiers().iter().any(|row| row.path == p("/tier267")));
        assert!(!loaded.tiers().iter().any(|row| row.path == p("/tier000")));
        assert!(
            loaded
                .tiers()
                .windows(2)
                .all(|pair| pair[0].path <= pair[1].path)
        );
    }

    #[test]
    fn set_tiers_reports_change_only_once() {
        let mut b = Behavior::new();
        let rows = vec![TierRaw {
            path: p("/d"),
            score: 3.0,
            at: 1,
            hot: false,
        }];
        b.set_tiers(rows.clone());
        assert_eq!(b.take_dirty(), 1.0);
        b.set_tiers(rows);
        assert_eq!(b.take_dirty(), 0.0);
    }

    #[test]
    fn dirty_weight_accumulates_from_folded_events_and_resets() {
        let mut b = Behavior::new();
        b.open_dir(Path::new("/a"), 1);
        b.open_dir(Path::new("/b"), 1);
        assert_eq!(b.take_dirty(), 0.0, "weight arrives at fold time");
        b.fold(1);
        assert!((b.take_dirty() - 2.0).abs() < 1e-4);
        assert_eq!(b.take_dirty(), 0.0);
    }
}
