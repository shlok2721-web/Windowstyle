//! RAM-only directory listing cache validated by the directory's own stamp.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A directory modified within this window is treated as unsafe and always rescanned:
/// filesystem timestamps are coarse, so a second change in the same tick can be invisible.
pub const RECENT_WINDOW: Duration = Duration::from_secs(2);

/// Kind of a directory entry, taken from `d_type` (no per-entry stat needed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link (target not resolved).
    Symlink,
    /// Anything else, or unknown.
    Other,
}

impl Kind {
    fn of(ft: fs::FileType) -> Self {
        if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else {
            Kind::Other
        }
    }
}

/// Per-entry metadata (symlinks are not followed, matching `DirEntry::metadata`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Meta {
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// Modification time.
    pub modified: Option<SystemTime>,
    /// Whether the entry itself is a directory.
    pub is_dir: bool,
}

impl Meta {
    pub(crate) fn of(md: &Metadata) -> Self {
        let is_dir = md.is_dir();
        Self {
            size: if is_dir { 0 } else { md.len() },
            modified: md.modified().ok(),
            is_dir,
        }
    }
}

/// One entry of a listing.
#[derive(Clone, Debug)]
pub struct Item {
    /// Full path.
    pub path: PathBuf,
    /// Entry kind.
    pub kind: Kind,
    /// Metadata, if it could be read.
    pub meta: Option<Meta>,
}

pub(crate) type Names = Vec<(OsString, Kind)>;
pub(crate) type Metas = Vec<Option<Meta>>;

/// Identity + timestamps of a directory; any entry add/remove/rename changes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    dev: u64,
    ino: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

fn to_time(t: (i64, i64)) -> SystemTime {
    if t.0 < 0 {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH + Duration::new(t.0 as u64, t.1.clamp(0, 999_999_999) as u32)
}

impl Stamp {
    #[cfg(unix)]
    pub(crate) fn of(md: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: md.dev(),
            ino: md.ino(),
            mtime: (md.mtime(), md.mtime_nsec()),
            ctime: (md.ctime(), md.ctime_nsec()),
        }
    }

    #[cfg(not(unix))]
    pub(crate) fn of(md: &Metadata) -> Self {
        let m = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or((0, 0), |d| {
                (d.as_secs() as i64, i64::from(d.subsec_nanos()))
            });
        Self {
            dev: 0,
            ino: 0,
            mtime: m,
            ctime: m,
        }
    }

    /// Whether the directory changed so recently that an equal stamp proves nothing.
    pub(crate) fn is_recent(&self, now: SystemTime) -> bool {
        let latest = to_time(self.mtime).max(to_time(self.ctime));
        match now.duration_since(latest) {
            Ok(age) => age < RECENT_WINDOW,
            Err(_) => true, // timestamp in the future (clock skew): never trust
        }
    }
}

/// Read names (+kinds) and optionally per-entry metadata in a single pass.
/// Uses `DirEntry::metadata`, i.e. `fstatat` relative to the open directory.
pub(crate) fn scan(dir: &Path, want_meta: bool) -> io::Result<(Names, Metas)> {
    let rd = fs::read_dir(dir)?;
    let mut names = Vec::new();
    let mut metas = Vec::new();
    for de in rd.flatten() {
        let kind = de.file_type().map(Kind::of).unwrap_or(Kind::Other);
        if want_meta {
            metas.push(de.metadata().ok().map(|m| Meta::of(&m)));
        }
        names.push((de.file_name(), kind));
    }
    Ok((names, metas))
}

/// Live per-entry metadata for already-known names.
pub(crate) fn live_metas(dir: &Path, names: &Names) -> Metas {
    names
        .iter()
        .map(|(n, _)| fs::symlink_metadata(dir.join(n)).ok().map(|m| Meta::of(&m)))
        .collect()
}

#[derive(Debug)]
pub(crate) struct Cached {
    pub stamp: Stamp,
    pub names: Names,
    /// Only ever `Some` for hot, watched directories.
    pub meta: Option<Metas>,
    pub loaded: SystemTime,
    pub last_used: u64,
    pub bytes: usize,
    pub hot: bool,
}

/// Size bounds for the cache.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum cached directories (hot + warm).
    pub max_entries: usize,
    /// Approximate RAM budget for all cached listings.
    pub max_bytes: usize,
    /// Warm entries older than this are rescanned even if their stamp matches.
    pub warm_ttl: Duration,
    /// Directories with more entries than this are never cached (treated as cold).
    pub max_names_per_dir: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 768,
            max_bytes: 32 * 1024 * 1024,
            warm_ttl: Duration::from_secs(600),
            max_names_per_dir: 100_000,
        }
    }
}

fn weigh(names: &Names, meta: &Option<Metas>) -> usize {
    let n: usize = names.iter().map(|(name, _)| name.len() + 40).sum();
    n + meta.as_ref().map_or(0, |m| m.len() * 40) + 96
}

#[derive(Debug, Default)]
pub(crate) struct DirCache {
    pub map: HashMap<PathBuf, Cached>,
    tick: u64,
    bytes: usize,
}

impl DirCache {
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn touch(&mut self, dir: &Path) {
        self.tick += 1;
        let t = self.tick;
        if let Some(c) = self.map.get_mut(dir) {
            c.last_used = t;
        }
    }

    pub fn remove(&mut self, dir: &Path) {
        if let Some(c) = self.map.remove(dir) {
            self.bytes = self.bytes.saturating_sub(c.bytes);
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }

    /// Drop the stored metadata of `dir` (keeps names), e.g. when it stops being hot.
    pub fn drop_meta(&mut self, dir: &Path) {
        if let Some(c) = self.map.get_mut(dir) {
            if c.meta.take().is_some() {
                let nb = weigh(&c.names, &None);
                self.bytes = self.bytes.saturating_sub(c.bytes.saturating_sub(nb));
                c.bytes = nb;
            }
        }
    }

    /// Store freshly fetched metadata for an existing entry.
    pub fn set_meta(&mut self, dir: &Path, metas: Metas) {
        if let Some(c) = self.map.get_mut(dir) {
            let nb = weigh(&c.names, &Some(metas.clone()));
            self.bytes = (self.bytes + nb).saturating_sub(c.bytes);
            c.bytes = nb;
            c.meta = Some(metas);
        }
    }

    pub fn insert(
        &mut self,
        dir: &Path,
        stamp: Stamp,
        names: Names,
        meta: Option<Metas>,
        now: SystemTime,
        hot: bool,
        limits: &Limits,
    ) {
        self.remove(dir);
        self.tick += 1;
        let bytes = weigh(&names, &meta);
        self.bytes += bytes;
        self.map.insert(
            dir.to_path_buf(),
            Cached {
                stamp,
                names,
                meta,
                loaded: now,
                last_used: self.tick,
                bytes,
                hot,
            },
        );
        self.evict(dir, limits);
    }

    /// Evict least-recently-used entries (warm before hot) until within bounds,
    /// never evicting `keep` (the entry just inserted).
    fn evict(&mut self, keep: &Path, limits: &Limits) {
        while self.map.len() > limits.max_entries || self.bytes > limits.max_bytes {
            let victim = self
                .map
                .iter()
                .filter(|(p, _)| p.as_path() != keep)
                .min_by_key(|(_, c)| (c.hot, c.last_used))
                .map(|(p, _)| p.clone());
            match victim {
                Some(v) => self.remove(&v),
                None => break,
            }
        }
    }
}
