//! Three tiers of attention for directory listings. **RAM only: this crate never writes.**
//!
//! | Tier | In RAM | Freshness |
//! |---|---|---|
//! | **Hot** | names + full metadata | `inotify` watch (applied by the host) + directory stamp |
//! | **Warm** | names only | directory `mtime`/`ctime` validated on every access, no watch |
//! | **Cold / denied** | nothing | read live, then dropped |
//!
//! A warm listing is valid while the directory's `(dev, ino, mtime, ctime)` stamp is
//! unchanged *and* the directory was not modified within the last [`RECENT_WINDOW`], because
//! coarse timestamps can hide a second change inside the same tick. Entry adds, removes and
//! renames always change the directory stamp, so no watch is needed to notice them. Content
//! changes of a file do not, which is why warm metadata is always fetched live and hot
//! metadata is only trusted while a watch is live and has not reported an event.
//!
//! Promotion/demotion uses a decayed score with hysteresis ([`PROMOTE_AT`]/[`DEMOTE_AT`]).
//!
//! There is no on-disk index: [`Tiers::search_hot`] searches the names that the hot tier
//! already holds in RAM, and [`Tiers::prime_hot`] fills that RAM in small, time-boxed steps
//! after startup.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod cache;
mod deny;
mod score;
mod search;

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use cache::{Item, Kind, Limits, Meta, RECENT_WINDOW};
pub use deny::DenyList;
pub use score::{
    DEMOTE_AT, HALF_LIFE_SECS, MAX_EXPORTED, MAX_TRACKED, PROMOTE_AT, SEED_SCORE, TierRaw, TierRow,
    TierTable,
};

use cache::{DirCache, Stamp, live_metas, scan};

/// Tier a directory currently belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Names + metadata in RAM, watched.
    Hot,
    /// Names in RAM, validated by directory stamp.
    Warm,
    /// Nothing kept (denied, oversized, or never visited).
    Cold,
}

/// Maximum number of simultaneously watched hot directories.
pub const MAX_WATCHES: usize = 256;

/// Counters that make the savings measurable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Listings served from RAM after a stamp check (no `getdents`).
    pub name_hits: u64,
    /// Listings that required a full directory scan.
    pub scans: u64,
    /// Hot listings whose per-entry metadata came from RAM (no per-entry stat).
    pub meta_ram_hits: u64,
    /// Per-entry live stats performed for warm/cold listings.
    pub live_stats: u64,
    /// Reads of denied/oversized directories that bypassed the cache.
    pub cold_reads: u64,
}

/// Result of [`Tiers::search_hot`].
#[derive(Clone, Debug, Default)]
pub struct SearchReport {
    /// Matching entries (at most the requested limit), with metadata.
    pub items: Vec<Item>,
    /// Hot directories whose RAM listing was searched.
    pub dirs_searched: usize,
    /// Hot directories that exist (searched or not).
    pub dirs_total: usize,
    /// Hot directories not searched because they are not in RAM yet (see
    /// [`Tiers::prime_hot`]) or could not be cached.
    pub dirs_unprimed: usize,
    /// At least one additional matching entry existed beyond the requested limit.
    pub truncated: bool,
}

/// Watches the host should add/remove to match the current hot set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchPlan {
    /// Directories to start watching (non-recursively).
    pub add: Vec<PathBuf>,
    /// Directories to stop watching.
    pub remove: Vec<PathBuf>,
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The tier manager: policy ([`TierTable`], [`DenyList`]) plus the listing cache.
pub struct Tiers {
    deny: DenyList,
    table: TierTable,
    cache: DirCache,
    limits: Limits,
    watched: HashSet<PathBuf>,
    watch_failed: HashSet<PathBuf>,
    /// Hot directories that could not be primed (unreadable or over the size bound).
    prime_failed: HashSet<PathBuf>,
    stats: Stats,
}

impl Tiers {
    /// Create a manager with the given deny list and default limits.
    pub fn new(deny: DenyList) -> Self {
        Self::with_limits(deny, Limits::default())
    }

    /// Create a manager with explicit cache limits.
    pub fn with_limits(deny: DenyList, limits: Limits) -> Self {
        Self {
            deny,
            table: TierTable::new(),
            cache: DirCache::default(),
            limits,
            watched: HashSet::new(),
            watch_failed: HashSet::new(),
            prime_failed: HashSet::new(),
            stats: Stats::default(),
        }
    }

    /// Paths that start out hot: `~/.local/bin` and `$PATH` entries inside `home`.
    /// (XDG user dirs are known to the host application, which passes them to
    /// [`seed`](Self::seed).)
    pub fn default_seed_paths(home: &Path) -> Vec<PathBuf> {
        let mut out = vec![home.join(".local/bin")];
        if let Some(path) = std::env::var_os("PATH") {
            for p in std::env::split_paths(&path) {
                if p.starts_with(home) && !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        out
    }

    /// Seed directories as hot (restored state for the same path takes precedence).
    pub fn seed(&mut self, dirs: impl IntoIterator<Item = PathBuf>) {
        let now = unix_secs(SystemTime::now());
        for d in dirs {
            if !self.deny.is_denied(&d) {
                self.table.seed(&d, now);
            }
        }
    }

    /// Pin a directory so it is always hot.
    pub fn pin(&mut self, dir: impl Into<PathBuf>) {
        self.table.pin(dir);
    }

    /// Remove a pin.
    pub fn unpin(&mut self, dir: &Path) {
        self.table.unpin(dir);
    }

    /// Read access to the score table (for persistence by the behaviour layer).
    pub fn table(&self) -> &TierTable {
        &self.table
    }

    /// Mutable access to the score table (restore from persistence).
    pub fn table_mut(&mut self) -> &mut TierTable {
        &mut self.table
    }

    /// Savings counters.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Number of cached listings and their approximate RAM use in bytes.
    pub fn cache_usage(&self) -> (usize, usize) {
        (self.cache.len(), self.cache.bytes())
    }

    /// Which tier `dir` is in right now.
    pub fn tier_of(&self, dir: &Path) -> Tier {
        if self.deny.is_denied(dir) {
            Tier::Cold
        } else if self.table.is_hot(dir, unix_secs(SystemTime::now())) {
            Tier::Hot
        } else if self.cache.map.contains_key(dir) {
            Tier::Warm
        } else {
            Tier::Cold
        }
    }

    /// Whether `dir` is denied (never cached or watched).
    pub fn is_denied(&self, dir: &Path) -> bool {
        self.deny.is_denied(dir)
    }

    /// List `dir` with names, kinds and metadata. `visit` records a user navigation
    /// (feeds promotion); pass `false` for background reads.
    pub fn read(&mut self, dir: &Path, visit: bool) -> io::Result<Vec<Item>> {
        self.read_at(dir, visit, SystemTime::now())
    }

    /// [`read`](Self::read) with an explicit clock, for deterministic tests.
    pub fn read_at(&mut self, dir: &Path, visit: bool, now: SystemTime) -> io::Result<Vec<Item>> {
        let (names, metas) = self.ensure(dir, now, true)?;
        if self.cache.map.contains_key(dir) {
            self.prime_failed.remove(dir);
        }
        let metas = match metas {
            Some(m) => m,
            None => {
                self.stats.live_stats += names.len() as u64;
                live_metas(dir, &names)
            }
        };
        if visit && !self.deny.is_denied(dir) {
            let secs = unix_secs(now);
            let first = !self.table.contains(dir);
            self.table.visit(dir, secs);
            // A git working tree is a strong "I will be back" signal, and the name list
            // that proves it is already in RAM, so this costs nothing.
            if first && names.iter().any(|(n, _)| n == ".git") {
                self.table.bump(dir, 3.0, secs);
            }
        }
        Ok(names
            .into_iter()
            .zip(metas)
            .map(|((name, kind), meta)| Item {
                path: dir.join(name),
                kind,
                meta,
            })
            .collect())
    }

    /// Sub-directories of `dir` (for tree views). Needs only names and `d_type`; symlinks and
    /// unknown kinds are resolved with one live `stat` each. Never counts as a visit.
    pub fn subdirs(&mut self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let (names, _) = self.ensure(dir, SystemTime::now(), false)?;
        Ok(names
            .into_iter()
            .filter_map(|(name, kind)| {
                let p = dir.join(name);
                match kind {
                    Kind::Dir => Some(p),
                    Kind::Symlink | Kind::Other => p.is_dir().then_some(p),
                    Kind::File => None,
                }
            })
            .collect())
    }

    /// Core: return names (+ metadata when `want_meta` and available without extra stats),
    /// from RAM when provably fresh, otherwise by scanning.
    fn ensure(
        &mut self,
        dir: &Path,
        now: SystemTime,
        want_meta: bool,
    ) -> io::Result<(cache::Names, Option<cache::Metas>)> {
        if self.deny.is_denied(dir) {
            self.cache.remove(dir);
            self.stats.cold_reads += 1;
            let (names, metas) = scan(dir, want_meta)?;
            return Ok((names, want_meta.then_some(metas)));
        }
        let md = match fs::metadata(dir) {
            Ok(md) => md,
            Err(e) => {
                self.cache.remove(dir);
                return Err(e);
            }
        };
        let stamp = Stamp::of(&md);
        let hot = self.table.is_hot(dir, unix_secs(now));
        let trusted_hot = hot && self.watched.contains(dir);

        let valid = self.cache.map.get(dir).is_some_and(|c| {
            let ttl_ok = trusted_hot
                || now
                    .duration_since(c.loaded)
                    .is_ok_and(|age| age < self.limits.warm_ttl);
            c.stamp == stamp && !stamp.is_recent(now) && ttl_ok
        });

        if valid {
            self.stats.name_hits += 1;
            self.cache.set_hot(dir, hot);
            self.cache.touch(dir);
            if !trusted_hot {
                self.cache.drop_meta(dir);
            }
            if trusted_hot && want_meta && self.cache.map[dir].meta.is_none() {
                // Became hot (and watched) after it was cached: fetch metadata once, now that
                // a live watch can invalidate it, and keep it in RAM from here on.
                let names = self.cache.map[dir].names.clone();
                self.stats.live_stats += names.len() as u64;
                let metas = live_metas(dir, &names);
                self.cache.set_meta(dir, &metas, &self.limits);
                return Ok((names, Some(metas)));
            }
            let c = &self.cache.map[dir];
            let names = c.names.clone();
            let metas = match (&c.meta, want_meta) {
                (Some(m), true) if trusted_hot => {
                    self.stats.meta_ram_hits += 1;
                    let mut m = m.clone();
                    // A sub-directory's own mtime changes when *its* contents change, which
                    // a watch on this directory does not report, so refresh those live.
                    for (i, (name, kind)) in names.iter().enumerate() {
                        if *kind == Kind::Dir {
                            m[i] = fs::symlink_metadata(dir.join(name))
                                .ok()
                                .map(|x| Meta::of(&x));
                        }
                    }
                    Some(m)
                }
                _ => None,
            };
            return Ok((names, metas));
        }

        // Miss: scan. Metadata is stored only when a live watch can later invalidate it.
        self.stats.scans += 1;
        let scan_meta = want_meta;
        let (names, metas) = match scan(dir, scan_meta) {
            Ok(result) => result,
            Err(error) => {
                // Do not leave an invalid pre-scan snapshot available after a failed refresh.
                self.cache.remove(dir);
                return Err(error);
            }
        };
        if names.len() <= self.limits.max_names_per_dir {
            let store_meta = (trusted_hot && scan_meta).then(|| metas.clone());
            self.cache.insert(
                dir,
                stamp,
                names.clone(),
                store_meta,
                now,
                hot,
                &self.limits,
            );
        } else {
            self.cache.remove(dir);
            self.stats.cold_reads += 1;
        }
        Ok((names, scan_meta.then_some(metas)))
    }

    /// Load the names of hot directories that are not in RAM yet, stopping once `budget`
    /// has elapsed so the caller can spread the work over frames. Returns how many hot
    /// directories are still waiting; call again until it returns `0`.
    pub fn prime_hot(&mut self, budget: Duration) -> usize {
        let started = Instant::now();
        let now = SystemTime::now();
        let hot = self.table.hot_dirs(unix_secs(now), MAX_WATCHES);
        let mut pending = 0;
        for dir in hot {
            if self.deny.is_denied(&dir)
                || self.prime_failed.contains(&dir)
                || self.cache.map.contains_key(&dir)
            {
                continue;
            }
            if started.elapsed() >= budget {
                pending += 1;
                continue;
            }
            // Metadata is only worth fetching when a live watch can keep it fresh.
            let want_meta = self.watched.contains(&dir);
            if self.ensure(&dir, now, want_meta).is_err() || !self.cache.map.contains_key(&dir) {
                self.prime_failed.insert(dir);
            }
        }
        pending
    }

    /// Search entry names in the **hot tier only**, entirely from RAM (hot listings that are
    /// valid by directory stamp; nothing is scanned except a directory modified within the
    /// last two seconds). `query` is whitespace-separated terms that must all match; a term
    /// containing `/` is matched against the whole path. Metadata comes from RAM when a live
    /// watch vouches for it, otherwise it is read live for the matches only.
    pub fn search_hot(&mut self, query: &str, limit: usize) -> SearchReport {
        let mut report = SearchReport::default();
        let Some(matcher) = search::Matcher::new(query) else {
            return report;
        };
        if limit == 0 {
            return report;
        }
        let now = SystemTime::now();
        let hot = self.table.hot_dirs(unix_secs(now), MAX_WATCHES);
        report.dirs_total = hot.len();
        for dir in hot {
            if self.deny.is_denied(&dir) || !self.cache.map.contains_key(&dir) {
                report.dirs_unprimed += 1;
                continue;
            }
            // Searching needs names for every entry, but metadata only for actual matches.
            // Asking `ensure` for metadata here would stat every child after an invalidation.
            let Ok((names, _)) = self.ensure(&dir, now, false) else {
                report.dirs_unprimed += 1;
                continue;
            };
            let trusted_hot =
                self.table.is_hot(&dir, unix_secs(now)) && self.watched.contains(&dir);
            let cached_metas = if trusted_hot {
                self.cache.map.get(&dir).and_then(|c| c.meta.as_ref())
            } else {
                None
            };
            let dir_str = if matcher.needs_path {
                dir.to_string_lossy().into_owned()
            } else {
                String::new()
            };
            report.dirs_searched += 1;
            let mut used_cached_meta = false;
            for (i, (name, kind)) in names.iter().enumerate() {
                let display_name = name.to_string_lossy();
                if !matcher.matches(&display_name, &dir_str) {
                    continue;
                }
                // We need one extra match to know that the result was actually truncated.
                // This avoids reporting truncation when the total number of matches equals
                // the requested limit exactly.
                if report.items.len() == limit {
                    if used_cached_meta {
                        self.stats.meta_ram_hits += 1;
                    }
                    report.truncated = true;
                    return report;
                }
                let path = dir.join(name);
                let meta = match cached_metas {
                    Some(all) if *kind != Kind::Dir => {
                        used_cached_meta = true;
                        all.get(i).copied().flatten()
                    }
                    Some(_) => fs::symlink_metadata(&path).ok().map(|md| Meta::of(&md)),
                    None => {
                        self.stats.live_stats += 1;
                        fs::symlink_metadata(&path).ok().map(|md| Meta::of(&md))
                    }
                };
                report.items.push(Item {
                    path,
                    kind: *kind,
                    meta,
                });
            }
            if used_cached_meta {
                self.stats.meta_ram_hits += 1;
            }
        }
        report
    }

    /// Forget everything cached for `dir` (user refresh, or a change event).
    pub fn invalidate(&mut self, dir: &Path) {
        self.cache.remove(dir);
        self.prime_failed.remove(dir);
    }

    /// Forget every cached listing (e.g. the kernel event queue overflowed).
    pub fn invalidate_all(&mut self) {
        self.cache.clear();
        self.prime_failed.clear();
    }

    /// A filesystem event touched `path`: drop its parent's listing, and its own if it is
    /// a cached directory (created/removed/renamed directories).
    pub fn touch_path(&mut self, path: &Path) {
        if let Some(parent) = path.parent() {
            self.cache.remove(parent);
            self.prime_failed.remove(parent);
        }
        self.cache.remove(path);
        self.prime_failed.remove(path);
    }

    /// The set of watches needed to keep the hot tier fresh, as a diff against what the host
    /// reports as currently watched. Apply it, then report each outcome with
    /// [`watch_result`](Self::watch_result).
    pub fn watch_plan(&mut self) -> WatchPlan {
        let now = unix_secs(SystemTime::now());
        let desired: Vec<PathBuf> = self
            .table
            .hot_dirs(now, MAX_WATCHES)
            .into_iter()
            .filter(|d| !self.deny.is_denied(d) && !self.watch_failed.contains(d) && d.is_dir())
            .collect();
        let want: HashSet<&PathBuf> = desired.iter().collect();
        let add = desired
            .iter()
            .filter(|d| !self.watched.contains(*d))
            .cloned()
            .collect();
        let remove = self
            .watched
            .iter()
            .filter(|d| !want.contains(d))
            .cloned()
            .collect();
        WatchPlan { add, remove }
    }

    /// Report the outcome of applying a watch (`ok == false` e.g. when out of inotify
    /// watches: the directory then simply stays warm and is never retried).
    pub fn watch_result(&mut self, dir: &Path, ok: bool) {
        if ok {
            self.watch_failed.remove(dir);
            self.prime_failed.remove(dir);
            if self.watched.insert(dir.to_path_buf()) {
                // Anything cached before the watch was live may have missed an event.
                self.cache.remove(dir);
            }
        } else {
            if self.watched.remove(dir) {
                self.cache.drop_meta(dir);
            }
            self.watch_failed.insert(dir.to_path_buf());
        }
    }

    /// Report that the host stopped watching `dir`.
    pub fn unwatched(&mut self, dir: &Path) {
        if self.watched.remove(dir) {
            self.cache.drop_meta(dir);
        }
    }

    /// Currently watched hot directories.
    pub fn watched(&self) -> impl Iterator<Item = &PathBuf> {
        self.watched.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    fn later(secs: u64) -> SystemTime {
        SystemTime::now() + Duration::from_secs(secs)
    }

    fn tick() {
        // Directory timestamps are coarse; separate changes so each one is visible.
        sleep(Duration::from_millis(25));
    }

    fn names(items: &[Item]) -> Vec<String> {
        let mut v: Vec<String> = items
            .iter()
            .map(|i| i.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn fixture() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for n in ["a.txt", "b.txt", "c.txt"] {
            fs::write(d.path().join(n), "x").unwrap();
        }
        fs::create_dir(d.path().join("sub")).unwrap();
        d
    }

    fn tiers() -> Tiers {
        Tiers::new(DenyList::empty())
    }

    #[test]
    fn warm_hit_skips_the_scan_and_detects_added_and_removed_entries() {
        let d = fixture();
        let mut t = tiers();
        let first = t.read_at(d.path(), false, later(10)).unwrap();
        assert_eq!(names(&first), ["a.txt", "b.txt", "c.txt", "sub"]);
        assert_eq!(t.stats().scans, 1);

        t.read_at(d.path(), false, later(10)).unwrap();
        assert_eq!((t.stats().scans, t.stats().name_hits), (1, 1));

        tick();
        fs::write(d.path().join("new.txt"), "n").unwrap();
        let after_add = t.read_at(d.path(), false, later(20)).unwrap();
        assert!(names(&after_add).contains(&"new.txt".to_owned()));
        assert_eq!(t.stats().scans, 2);

        tick();
        fs::remove_file(d.path().join("a.txt")).unwrap();
        let after_rm = t.read_at(d.path(), false, later(30)).unwrap();
        assert!(!names(&after_rm).contains(&"a.txt".to_owned()));
        assert_eq!(t.stats().scans, 3);

        tick();
        fs::rename(d.path().join("b.txt"), d.path().join("b2.txt")).unwrap();
        let after_mv = t.read_at(d.path(), false, later(40)).unwrap();
        assert!(names(&after_mv).contains(&"b2.txt".to_owned()));
    }

    #[test]
    fn recently_modified_directory_is_never_trusted() {
        let d = fixture();
        let mut t = tiers();
        t.read(d.path(), false).unwrap();
        t.read(d.path(), false).unwrap();
        assert_eq!(
            t.stats().name_hits,
            0,
            "inside the 2 s window every read must rescan"
        );
        assert_eq!(t.stats().scans, 2);
    }

    #[test]
    fn warm_metadata_is_always_live() {
        let d = fixture();
        let mut t = tiers();
        let size_of = |items: &[Item]| {
            items
                .iter()
                .find(|i| i.path.ends_with("a.txt"))
                .unwrap()
                .meta
                .unwrap()
                .size
        };
        assert_eq!(size_of(&t.read_at(d.path(), false, later(10)).unwrap()), 1);
        tick();
        fs::write(d.path().join("a.txt"), "much longer").unwrap(); // dir stamp unchanged
        let second = t.read_at(d.path(), false, later(10)).unwrap();
        assert_eq!(size_of(&second), 11);
        assert_eq!(t.stats().name_hits, 1);
        assert!(t.stats().live_stats >= 4);
    }

    #[test]
    fn warm_entries_expire_by_ttl() {
        let d = fixture();
        let mut t = tiers();
        t.read_at(d.path(), false, later(10)).unwrap();
        t.read_at(d.path(), false, later(10 + 599)).unwrap();
        assert_eq!(t.stats().scans, 1);
        t.read_at(d.path(), false, later(10 + 601)).unwrap();
        assert_eq!(t.stats().scans, 2);
    }

    #[test]
    fn denied_directories_are_read_live_and_never_cached() {
        let d = fixture();
        let nm = d.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("pkg.js"), "x").unwrap();
        let mut t = Tiers::new(DenyList::defaults(None));
        assert_eq!(names(&t.read_at(&nm, true, later(10)).unwrap()), ["pkg.js"]);
        assert_eq!(t.cache_usage().0, 0);
        assert_eq!(t.tier_of(&nm), Tier::Cold);
        assert_eq!(t.stats().cold_reads, 1);
        assert!(
            !t.table().contains(&nm),
            "denied directories are not scored"
        );
        assert!(t.watch_plan().add.is_empty());
    }

    #[test]
    fn hot_metadata_comes_from_ram_only_while_a_watch_is_live() {
        let d = fixture();
        let mut t = tiers();
        t.seed([d.path().to_path_buf()]);
        assert_eq!(t.tier_of(d.path()), Tier::Hot);
        let plan = t.watch_plan();
        assert_eq!(plan.add, [d.path().to_path_buf()]);

        // Before the watch is live, hot behaves like warm: nothing is trusted.
        t.read_at(d.path(), false, later(10)).unwrap();
        t.read_at(d.path(), false, later(10)).unwrap();
        assert_eq!(t.stats().meta_ram_hits, 0);

        t.watch_result(d.path(), true);
        assert_eq!(
            t.cache_usage().0,
            0,
            "a pre-watch cache entry may have missed events"
        );
        let size = |items: &[Item]| {
            items
                .iter()
                .find(|i| i.path.ends_with("a.txt"))
                .unwrap()
                .meta
                .unwrap()
                .size
        };
        assert_eq!(size(&t.read_at(d.path(), false, later(10)).unwrap()), 1);
        assert_eq!(size(&t.read_at(d.path(), false, later(10)).unwrap()), 1);
        assert_eq!(t.stats().meta_ram_hits, 1);

        // A content change does not alter the directory stamp: RAM is stale until the
        // watch event arrives, which the host reports through touch_path.
        tick();
        fs::write(d.path().join("a.txt"), "changed!").unwrap();
        assert_eq!(size(&t.read_at(d.path(), false, later(10)).unwrap()), 1);
        t.touch_path(&d.path().join("a.txt"));
        assert_eq!(size(&t.read_at(d.path(), false, later(10)).unwrap()), 8);

        // Unwatching drops RAM metadata again.
        let ram_hits = t.stats().meta_ram_hits;
        t.unwatched(d.path());
        t.read_at(d.path(), false, later(10)).unwrap();
        assert_eq!(t.stats().meta_ram_hits, ram_hits);
    }

    #[test]
    fn hot_subdirectory_mtime_is_refreshed_live() {
        let d = fixture();
        let mut t = tiers();
        t.seed([d.path().to_path_buf()]);
        t.watch_result(d.path(), true);
        let sub = d.path().join("sub");
        let mtime = |items: &[Item]| {
            items
                .iter()
                .find(|i| i.path == sub)
                .unwrap()
                .meta
                .unwrap()
                .modified
        };
        let before = mtime(&t.read_at(d.path(), false, later(10)).unwrap());
        tick();
        fs::write(sub.join("inner.txt"), "x").unwrap(); // changes sub's mtime, not d's stamp
        let after = mtime(&t.read_at(d.path(), false, later(10)).unwrap());
        assert_ne!(before, after);
    }

    #[test]
    fn entry_cached_before_becoming_hot_gains_ram_metadata_once() {
        let d = fixture();
        let mut t = tiers();
        t.read_at(d.path(), false, later(10)).unwrap(); // warm
        t.seed([d.path().to_path_buf()]);
        t.cache.map.get_mut(d.path()).unwrap(); // sanity: entry exists
        t.watched.insert(d.path().to_path_buf()); // watch went live without dropping (test hook)
        t.read_at(d.path(), false, later(10)).unwrap(); // upgrade: one live fetch
        assert_eq!(t.stats().meta_ram_hits, 0);
        let live_after_first = t.stats().live_stats;
        t.read_at(d.path(), false, later(10)).unwrap();
        assert_eq!(
            t.stats().live_stats,
            live_after_first,
            "second read must not stat entries"
        );
        assert_eq!(t.stats().meta_ram_hits, 1);
    }

    #[test]
    fn failed_watch_keeps_the_directory_warm_and_is_not_retried() {
        let d = fixture();
        let mut t = tiers();
        t.seed([d.path().to_path_buf()]);
        assert_eq!(t.watch_plan().add.len(), 1);
        t.watch_result(d.path(), false);
        assert!(t.watch_plan().add.is_empty());
        t.read_at(d.path(), false, later(10)).unwrap();
        t.read_at(d.path(), false, later(10)).unwrap();
        assert_eq!(t.stats().meta_ram_hits, 0);
        assert_eq!(t.stats().name_hits, 1);
    }

    #[test]
    fn watch_plan_removes_demoted_directories() {
        let d = fixture();
        let mut t = tiers();
        t.seed([d.path().to_path_buf()]);
        t.watch_result(d.path(), true);
        t.table_mut().import(
            [TierRow {
                path: d.path().to_path_buf(),
                score: 0.5,
                hot: false,
            }],
            unix_secs(SystemTime::now()),
        );
        let plan = t.watch_plan();
        assert_eq!(plan.remove, [d.path().to_path_buf()]);
    }

    #[test]
    fn subdirs_uses_names_only_and_resolves_symlinks() {
        let d = fixture();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.path().join("sub"), d.path().join("link_to_dir")).unwrap();
            std::os::unix::fs::symlink(d.path().join("a.txt"), d.path().join("link_to_file"))
                .unwrap();
        }
        let mut t = tiers();
        let mut subs = t.subdirs(d.path()).unwrap();
        subs.sort();
        let got: Vec<String> = subs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        #[cfg(unix)]
        assert_eq!(got, ["link_to_dir", "sub"]);
        #[cfg(not(unix))]
        assert_eq!(got, ["sub"]);
        assert_eq!(
            t.stats().live_stats,
            0,
            "tree expansion must not stat every entry"
        );
        assert!(
            !t.table().contains(d.path()),
            "tree expansion is not a visit"
        );
    }

    #[test]
    fn lru_bounds_the_number_of_cached_directories() {
        let root = tempfile::tempdir().unwrap();
        let dirs: Vec<PathBuf> = (0..3)
            .map(|i| {
                let p = root.path().join(format!("d{i}"));
                fs::create_dir(&p).unwrap();
                p
            })
            .collect();
        let limits = Limits {
            max_entries: 2,
            ..Limits::default()
        };
        let mut t = Tiers::with_limits(DenyList::empty(), limits);
        for d in &dirs {
            t.read_at(d, false, later(10)).unwrap();
        }
        assert_eq!(t.cache_usage().0, 2);
        assert_eq!(
            t.tier_of(&dirs[0]),
            Tier::Cold,
            "least recently used was evicted"
        );
        assert_eq!(t.tier_of(&dirs[2]), Tier::Warm);
    }

    #[test]
    fn byte_budget_is_enforced() {
        let d = fixture();
        let limits = Limits {
            max_bytes: 150,
            ..Limits::default()
        };
        let mut t = Tiers::with_limits(DenyList::empty(), limits);
        t.read_at(d.path(), false, later(10)).unwrap();
        let other = tempfile::tempdir().unwrap();
        t.read_at(other.path(), false, later(10)).unwrap();
        assert!(t.cache_usage().1 <= 150 || t.cache_usage().0 == 1);
    }

    #[test]
    fn metadata_is_not_retained_when_it_exceeds_the_cache_budget() {
        let d = fixture();
        let limits = Limits {
            max_bytes: 300,
            ..Limits::default()
        };
        let mut t = Tiers::with_limits(DenyList::empty(), limits);
        t.seed([d.path().to_path_buf()]);
        t.watch_result(d.path(), true);

        let first = t.read_at(d.path(), false, later(10)).unwrap();
        assert!(first.iter().all(|item| item.meta.is_some()));
        assert_eq!(t.cache_usage().0, 1);
        assert!(t.cache_usage().1 <= 300, "{:?}", t.cache_usage());
        assert!(t.cache.map.get(d.path()).unwrap().meta.is_none());

        let second = t.read_at(d.path(), false, later(10)).unwrap();
        assert!(second.iter().all(|item| item.meta.is_some()));
        assert!(t.cache_usage().1 <= 300, "{:?}", t.cache_usage());
        assert!(t.cache.map.get(d.path()).unwrap().meta.is_none());
    }

    #[test]
    fn zero_entry_budget_disables_caching() {
        let d = fixture();
        let limits = Limits {
            max_entries: 0,
            ..Limits::default()
        };
        let mut t = Tiers::with_limits(DenyList::empty(), limits);
        t.read_at(d.path(), false, later(10)).unwrap();
        t.read_at(d.path(), false, later(10)).unwrap();
        assert_eq!(t.cache_usage(), (0, 0));
        assert_eq!(t.stats().scans, 2);
    }

    #[test]
    fn oversized_directories_are_not_cached() {
        let d = fixture();
        let limits = Limits {
            max_names_per_dir: 2,
            ..Limits::default()
        };
        let mut t = Tiers::with_limits(DenyList::empty(), limits);
        assert_eq!(t.read_at(d.path(), false, later(10)).unwrap().len(), 4);
        assert_eq!(t.cache_usage().0, 0);
        t.read_at(d.path(), false, later(10)).unwrap();
        assert_eq!(t.stats().scans, 2);
    }

    #[test]
    fn visits_promote_and_git_trees_start_ahead() {
        let d = fixture();
        fs::create_dir(d.path().join(".git")).unwrap();
        let mut t = tiers();
        t.read_at(d.path(), true, later(10)).unwrap();
        let s = t.table().score(d.path(), unix_secs(later(10)));
        assert!((s - 4.0).abs() < 0.01, "1 visit + 3 git bonus, got {s}");
        for _ in 0..4 {
            t.read_at(d.path(), true, later(10)).unwrap();
        }
        assert_eq!(t.tier_of(d.path()), Tier::Hot);

        let plain = fixture();
        let mut u = tiers();
        for _ in 0..4 {
            u.read_at(plain.path(), true, later(10)).unwrap();
        }
        assert_eq!(u.tier_of(plain.path()), Tier::Warm);
    }

    #[test]
    fn missing_directory_errors_and_drops_its_cache_entry() {
        let d = fixture();
        let p = d.path().join("sub");
        let mut t = tiers();
        t.read_at(&p, false, later(10)).unwrap();
        fs::remove_dir(&p).unwrap();
        assert!(t.read_at(&p, false, later(20)).is_err());
        assert_eq!(t.cache_usage().0, 0);
    }

    #[test]
    fn touch_path_and_invalidate_force_a_rescan() {
        let d = fixture();
        let mut t = tiers();
        t.read_at(d.path(), false, later(10)).unwrap();
        t.touch_path(&d.path().join("whatever"));
        t.read_at(d.path(), false, later(10)).unwrap();
        assert_eq!(t.stats().scans, 2);
        t.invalidate(d.path());
        t.read_at(d.path(), false, later(10)).unwrap();
        assert_eq!(t.stats().scans, 3);
        t.invalidate_all();
        assert_eq!(t.cache_usage(), (0, 0));
    }

    #[test]
    fn hot_search_uses_ram_listings_and_skips_unprimed_directories() {
        let d = fixture();
        let mut t = tiers();
        t.seed([d.path().to_path_buf()]);
        t.watch_result(d.path(), true);

        // Not primed yet: nothing is scanned by a search.
        let before = t.search_hot("a.txt", 10);
        assert!(before.items.is_empty());
        assert_eq!((before.dirs_total, before.dirs_unprimed), (1, 1));

        assert_eq!(t.prime_hot(Duration::from_secs(5)), 0);
        let hit = t.search_hot("A.TXT", 10);
        assert_eq!(hit.items.len(), 1);
        assert_eq!(hit.dirs_searched, 1);
        assert!(hit.items[0].path.ends_with("a.txt"));
        assert_eq!(hit.items[0].meta.unwrap().size, 1);

        let all = t.search_hot("txt", 2);
        assert_eq!(all.items.len(), 2);
        assert!(all.truncated);

        let exact = t.search_hot("a.txt", 1);
        assert_eq!(exact.items.len(), 1);
        assert!(
            !exact.truncated,
            "exactly reaching the limit is not truncation"
        );
        assert!(t.search_hot("   ", 10).items.is_empty());
    }

    #[test]
    fn hot_search_only_fetches_metadata_for_matches_after_a_rescan() {
        let d = fixture();
        let mut t = tiers();
        t.seed([d.path().to_path_buf()]);
        t.watch_result(d.path(), true);
        assert_eq!(t.prime_hot(Duration::from_secs(5)), 0);

        // Invalidate the directory stamp without deleting the cached entry. The search must
        // rescan names only, then fetch metadata for the one matching result.
        fs::write(d.path().join("new-entry.txt"), "x").unwrap();
        let before = t.stats().live_stats;
        let result = t.search_hot("a.txt", 10);
        assert_eq!(result.items.len(), 1);
        assert_eq!(t.stats().live_stats - before, 1);
    }

    #[test]
    fn prime_hot_skips_directories_that_cannot_be_cached() {
        let d = fixture();
        let limits = Limits {
            max_names_per_dir: 2,
            ..Limits::default()
        };
        let mut t = Tiers::with_limits(DenyList::empty(), limits);
        t.seed([d.path().to_path_buf()]);
        assert_eq!(t.prime_hot(Duration::from_secs(5)), 0);
        assert_eq!(
            t.prime_hot(Duration::from_secs(5)),
            0,
            "failed priming is not retried"
        );
        assert_eq!(t.cache_usage().0, 0);
    }

    #[test]
    fn invalidate_allows_a_previously_unreadable_hot_directory_to_be_retried() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("appears-later");
        let mut t = tiers();
        t.seed([dir.clone()]);

        assert_eq!(t.prime_hot(Duration::from_secs(1)), 0);
        assert!(t.prime_failed.contains(&dir));

        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("file.txt"), "x").unwrap();
        assert_eq!(t.prime_hot(Duration::from_secs(1)), 0);
        assert_eq!(
            t.cache_usage().0,
            0,
            "failed priming is skipped until invalidated"
        );

        t.invalidate(&dir);
        assert_eq!(t.prime_hot(Duration::from_secs(1)), 0);
        assert_eq!(t.cache_usage().0, 1);
    }

    #[test]
    fn default_seed_paths_include_local_bin() {
        let p = Tiers::default_seed_paths(Path::new("/home/u"));
        assert!(p.contains(&PathBuf::from("/home/u/.local/bin")));
    }
}
