//! Persist only what cannot be re-derived, as rarely and as cheaply as possible.
//!
//! * [`AtomicFile`]: one small file, replaced atomically (`write tmp` → `fdatasync` →
//!   `rename`). A write whose bytes hash to the same value as what is already on disk is
//!   **skipped**, so an unchanged state costs zero writes.
//! * [`Flusher`] + [`FlushPolicy`]: decides *when* to flush: at shutdown, or when enough
//!   meaningful change has piled up **and** at least `min_interval` (default 30 min) has passed.
//! * [`WriteCounter`]: counts every byte this crate writes so "bytes written per hour" can be
//!   reported (and other writers, such as the thumbnail cache, can add to the same counter).
//!
//! The directory itself is intentionally not `fsync`ed: the state is a preference cache, not
//! user data, so after a power loss the previous file (or none) is an acceptable outcome.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Hard upper bound for the state file. The behaviour layer keeps its aggregates far below
/// this; the bound only guards against a runaway producer.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;

/// Default minimum time between two non-shutdown flushes.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Default amount of folded behaviour weight that must accumulate before a periodic flush.
pub const DEFAULT_THRESHOLD: f32 = 8.0;

/// 64-bit FNV-1a hash (stable across runs and platforms, unlike `DefaultHasher`).
pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// Write accounting
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CounterInner {
    started: Instant,
    bytes: AtomicU64,
    writes: AtomicU64,
    skipped: AtomicU64,
}

/// Point-in-time copy of a [`WriteCounter`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WriteStats {
    /// Bytes handed to the filesystem.
    pub bytes: u64,
    /// Number of write operations (files replaced).
    pub writes: u64,
    /// Number of flushes skipped because nothing changed.
    pub skipped: u64,
    /// Seconds since the counter was created.
    pub elapsed_secs: f64,
    /// `bytes` extrapolated to one hour (0 until at least one second has elapsed).
    pub bytes_per_hour: f64,
}

/// Shared, cheaply clonable counter of bytes written to disk.
#[derive(Clone, Debug)]
pub struct WriteCounter(Arc<CounterInner>);

impl Default for WriteCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteCounter {
    /// A fresh counter starting at zero, with the clock started now.
    pub fn new() -> Self {
        Self(Arc::new(CounterInner {
            started: Instant::now(),
            bytes: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
        }))
    }

    /// Record one write of `bytes` bytes.
    pub fn add_write(&self, bytes: u64) {
        self.0.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.0.writes.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one flush that was skipped because the content was unchanged.
    pub fn add_skip(&self) {
        self.0.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// Total bytes written so far.
    pub fn bytes(&self) -> u64 {
        self.0.bytes.load(Ordering::Relaxed)
    }

    /// Snapshot of all counters, including the per-hour rate.
    pub fn snapshot(&self) -> WriteStats {
        let bytes = self.0.bytes.load(Ordering::Relaxed);
        let elapsed_secs = self.0.started.elapsed().as_secs_f64();
        let bytes_per_hour = if elapsed_secs >= 1.0 {
            bytes as f64 * 3600.0 / elapsed_secs
        } else {
            0.0
        };
        WriteStats {
            bytes,
            writes: self.0.writes.load(Ordering::Relaxed),
            skipped: self.0.skipped.load(Ordering::Relaxed),
            elapsed_secs,
            bytes_per_hour,
        }
    }
}

// ---------------------------------------------------------------------------
// Atomic, hash-skipped file
// ---------------------------------------------------------------------------

/// Result of [`AtomicFile::write_if_changed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The bytes hash to what is already on disk: nothing was written.
    Skipped,
    /// The file was replaced.
    Written {
        /// Number of bytes written.
        bytes: u64,
    },
}

/// A single small file that is only ever replaced atomically and never rewritten with
/// identical content.
#[derive(Debug)]
pub struct AtomicFile {
    path: PathBuf,
    last_hash: Option<u64>,
    counter: WriteCounter,
}

impl AtomicFile {
    /// Manage `path`; writes are reported to `counter`.
    pub fn new(path: impl Into<PathBuf>, counter: WriteCounter) -> Self {
        Self {
            path: path.into(),
            last_hash: None,
            counter,
        }
    }

    /// The managed path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The counter this file reports to.
    pub fn counter(&self) -> &WriteCounter {
        &self.counter
    }

    /// Read the current file, if any, and remember its hash so that an identical first flush
    /// is skipped. Files above [`MAX_FILE_BYTES`] are reported as `InvalidData`.
    pub fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
        match fs::metadata(&self.path) {
            Ok(md) if md.len() > MAX_FILE_BYTES as u64 => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "state file is unexpectedly large",
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
        match fs::read(&self.path) {
            Ok(bytes) => {
                self.last_hash = Some(fnv64(&bytes));
                Ok(Some(bytes))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Replace the file with `bytes` unless they hash to what was last read or written.
    /// Uses `write tmp` → `fdatasync` → `rename`, in the same directory.
    pub fn write_if_changed(&mut self, bytes: &[u8]) -> io::Result<Outcome> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "state exceeds the size bound",
            ));
        }
        let hash = fnv64(bytes);
        if self.last_hash == Some(hash) {
            self.counter.add_skip();
            return Ok(Outcome::Skipped);
        }
        let parent = match self.path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        fs::create_dir_all(&parent)?;
        let file_name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "state".to_owned());
        let tmp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
        let result = (|| -> io::Result<()> {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_data()?;
            drop(file);
            fs::rename(&tmp, &self.path)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        self.last_hash = Some(hash);
        self.counter.add_write(bytes.len() as u64);
        Ok(Outcome::Written {
            bytes: bytes.len() as u64,
        })
    }
}

// ---------------------------------------------------------------------------
// Flush policy
// ---------------------------------------------------------------------------

/// When a periodic (non-shutdown) flush is allowed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlushPolicy {
    /// Minimum time between two periodic flushes.
    pub min_interval: Duration,
    /// Minimum accumulated change weight for a periodic flush.
    pub threshold: f32,
}

impl Default for FlushPolicy {
    fn default() -> Self {
        Self {
            min_interval: DEFAULT_MIN_INTERVAL,
            threshold: DEFAULT_THRESHOLD,
        }
    }
}

/// Tracks accumulated change and answers "should we flush now?".
#[derive(Clone, Debug)]
pub struct Flusher {
    policy: FlushPolicy,
    dirty_weight: f32,
    last_flush: Instant,
    shutting_down: bool,
}

impl Flusher {
    /// A flusher whose interval clock starts at `now`.
    pub fn new(policy: FlushPolicy, now: Instant) -> Self {
        Self {
            policy,
            dirty_weight: 0.0,
            last_flush: now,
            shutting_down: false,
        }
    }

    /// Add meaningful change (for example the weight of newly folded events).
    pub fn mark_dirty(&mut self, weight: f32) {
        if weight.is_finite() && weight > 0.0 {
            self.dirty_weight += weight;
        }
    }

    /// Accumulated change since the last flush.
    pub fn dirty_weight(&self) -> f32 {
        self.dirty_weight
    }

    /// Announce shutdown (window closed, SIGTERM, suspend hook): the next check says flush.
    pub fn request_shutdown(&mut self) {
        self.shutting_down = true;
    }

    /// Whether a flush is due at `now`.
    pub fn should_flush(&self, now: Instant) -> bool {
        self.shutting_down
            || (self.dirty_weight > self.policy.threshold
                && now.saturating_duration_since(self.last_flush) > self.policy.min_interval)
    }

    /// Record that a flush happened at `now` (clears the dirty weight).
    pub fn flushed(&mut self, now: Instant) {
        self.dirty_weight = 0.0;
        self.last_flush = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_is_skipped_and_counts_no_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let counter = WriteCounter::new();
        let mut file = AtomicFile::new(dir.path().join("state.txt"), counter.clone());
        assert_eq!(
            file.write_if_changed(b"hello").unwrap(),
            Outcome::Written { bytes: 5 }
        );
        assert_eq!(file.write_if_changed(b"hello").unwrap(), Outcome::Skipped);
        assert_eq!(
            file.write_if_changed(b"hello!").unwrap(),
            Outcome::Written { bytes: 6 }
        );
        let stats = counter.snapshot();
        assert_eq!((stats.bytes, stats.writes, stats.skipped), (11, 2, 1));
        assert_eq!(fs::read(file.path()).unwrap(), b"hello!");
    }

    #[test]
    fn reading_the_existing_file_makes_an_identical_first_flush_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("state.txt");
        let mut first = AtomicFile::new(&path, WriteCounter::new());
        first.write_if_changed(b"abc").unwrap();

        let counter = WriteCounter::new();
        let mut second = AtomicFile::new(&path, counter.clone());
        assert_eq!(second.read().unwrap().as_deref(), Some(&b"abc"[..]));
        assert_eq!(second.write_if_changed(b"abc").unwrap(), Outcome::Skipped);
        assert_eq!(counter.bytes(), 0);
    }

    #[test]
    fn missing_file_reads_as_none_and_no_temporary_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = AtomicFile::new(dir.path().join("state.txt"), WriteCounter::new());
        assert!(file.read().unwrap().is_none());
        file.write_if_changed(b"x").unwrap();
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn oversized_state_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = AtomicFile::new(dir.path().join("state.txt"), WriteCounter::new());
        let big = vec![0_u8; MAX_FILE_BYTES + 1];
        assert!(file.write_if_changed(&big).is_err());
    }

    #[test]
    fn policy_needs_both_weight_and_time_unless_shutting_down() {
        let t0 = Instant::now();
        let mut f = Flusher::new(FlushPolicy::default(), t0);
        let later = t0 + Duration::from_secs(31 * 60);
        assert!(!f.should_flush(later), "no change, no flush");
        f.mark_dirty(100.0);
        assert!(!f.should_flush(t0 + Duration::from_secs(60)), "too soon");
        assert!(f.should_flush(later));
        f.flushed(later);
        assert_eq!(f.dirty_weight(), 0.0);
        f.mark_dirty(1.0);
        f.request_shutdown();
        assert!(f.should_flush(later), "shutdown always flushes");
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv64(b"a"), fnv64(b"b"));
    }
}
