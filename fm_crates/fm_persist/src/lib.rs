//! Persist only what cannot be re-derived, as rarely and as cheaply as possible.
//!
//! * [`AtomicFile`]: one small file, replaced atomically (`write tmp` → `sync_all` →
//!   `rename`). A write whose bytes hash and length match the last content read or written is
//!   **skipped**, so an unchanged state costs zero writes.
//! * [`Flusher`] + [`FlushPolicy`]: decides *when* to flush: at shutdown, or when enough
//!   meaningful change has piled up **and** at least `min_interval` (default 30 min) has passed.
//! * [`WriteCounter`]: counts payload bytes in successful file replacements so committed write
//!   volume can be reported. Other writers, such as the thumbnail cache, can share the counter.
//!
//! The directory itself is intentionally not `fsync`ed: the state is a preference cache, not
//! user data, so after a power loss the previous file (or none) is an acceptable outcome.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fs;
use std::io::{self, Read, Write};
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

// A process-wide sequence makes temporary names distinct between concurrent writers.
// `create_new` remains the final authority and protects against stale files and PID reuse.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    /// Payload bytes in successful file replacements.
    pub bytes: u64,
    /// Number of successful file replacements.
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

    /// Record one successful file replacement containing `bytes` payload bytes.
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
    /// The content matches the last successful read or write: nothing was written.
    Skipped,
    /// The file was replaced.
    Written {
        /// Number of bytes written.
        bytes: u64,
    },
}

/// A single small file that is only ever replaced atomically and never rewritten with
/// content matching the last successful read or write. The cache assumes this instance owns
/// updates to the path; external edits are observed by calling [`AtomicFile::read`] again.
#[derive(Debug)]
pub struct AtomicFile {
    path: PathBuf,
    last_hash: Option<u64>,
    last_len: Option<usize>,
    counter: WriteCounter,
}

fn create_temp_file(parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    // Bounded retries avoid looping forever in a hostile or unusually stale directory.
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(".fm-persist-{}-{sequence}.tmp", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary state file after 128 attempts",
    ))
}

impl AtomicFile {
    /// Manage `path`; writes are reported to `counter`.
    pub fn new(path: impl Into<PathBuf>, counter: WriteCounter) -> Self {
        Self {
            path: path.into(),
            last_hash: None,
            last_len: None,
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
        // A failed or missing read must not leave a cache entry that could suppress a
        // necessary subsequent write. Open once, then inspect and read that same file.
        self.last_hash = None;
        self.last_len = None;
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let metadata = file.metadata()?;
        if metadata.len() > MAX_FILE_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "state file is unexpectedly large",
            ));
        }

        // Metadata can become stale while another process is writing. Limit the actual
        // read to max+1 bytes as well, so a growing file cannot cause an unbounded read.
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "state file grew beyond the size bound while being read",
            ));
        }

        self.last_hash = Some(fnv64(&bytes));
        self.last_len = Some(bytes.len());
        Ok(Some(bytes))
    }

    /// Replace the file with `bytes` unless their hash and length match the last read or write.
    /// Uses a unique temporary file, syncs it, then renames it within the same directory.
    /// When replacing an existing file, its permission bits are retained where the platform
    /// exposes them through standard filesystem metadata.
    pub fn write_if_changed(&mut self, bytes: &[u8]) -> io::Result<Outcome> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "state exceeds the size bound",
            ));
        }
        let hash = fnv64(bytes);
        if self.last_hash == Some(hash) && self.last_len == Some(bytes.len()) {
            self.counter.add_skip();
            return Ok(Outcome::Skipped);
        }
        let parent = match self.path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        fs::create_dir_all(&parent)?;
        let previous_permissions = match fs::metadata(&self.path) {
            Ok(metadata) => Some(metadata.permissions()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let (tmp, mut file) = create_temp_file(&parent)?;
        let result = (|| -> io::Result<()> {
            file.write_all(bytes)?;
            if let Some(permissions) = previous_permissions {
                file.set_permissions(permissions)?;
            }
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &self.path)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        self.last_hash = Some(hash);
        self.last_len = Some(bytes.len());
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
    /// Minimum accumulated change weight for a periodic flush. Negative or NaN values fall back
    /// to [`DEFAULT_THRESHOLD`]; positive infinity disables weight-triggered periodic flushing.
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
    pub fn new(mut policy: FlushPolicy, now: Instant) -> Self {
        if policy.threshold.is_nan() || policy.threshold < 0.0 {
            policy.threshold = DEFAULT_THRESHOLD;
        }
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
            // Keep the accumulator finite even after an extreme number of events.
            self.dirty_weight = (self.dirty_weight + weight).min(f32::MAX);
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
            || (self.dirty_weight > 0.0
                && self.dirty_weight >= self.policy.threshold
                && now.saturating_duration_since(self.last_flush) >= self.policy.min_interval)
    }

    /// Record that a flush happened at `now` (clears the dirty weight).
    pub fn flushed(&mut self, now: Instant) {
        self.dirty_weight = 0.0;
        self.last_flush = now;
        self.shutting_down = false;
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
    fn deleting_a_previously_read_file_does_not_leave_a_stale_skip_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.txt");
        let counter = WriteCounter::new();
        let mut file = AtomicFile::new(&path, counter.clone());
        file.write_if_changed(b"same").unwrap();
        assert_eq!(file.read().unwrap().as_deref(), Some(&b"same"[..]));
        fs::remove_file(&path).unwrap();
        assert!(file.read().unwrap().is_none());

        assert_eq!(
            file.write_if_changed(b"same").unwrap(),
            Outcome::Written { bytes: 4 }
        );
        assert_eq!(fs::read(path).unwrap(), b"same");
        assert_eq!(counter.snapshot().writes, 2);
    }

    #[test]
    fn read_rejects_oversized_files_without_caching_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.txt");
        fs::write(&path, vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
        let mut file = AtomicFile::new(&path, WriteCounter::new());
        assert_eq!(file.read().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            file.write_if_changed(b"ok").unwrap(),
            Outcome::Written { bytes: 2 }
        );
        assert_eq!(fs::read(path).unwrap(), b"ok");
    }

    #[test]
    fn concurrent_writers_use_independent_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.txt");
        let mut threads = Vec::new();
        let expected: Vec<Vec<u8>> = (0..12).map(|i| vec![b'a' + i; 1024]).collect();
        for content in expected.clone() {
            let path = path.clone();
            threads.push(std::thread::spawn(move || {
                let mut file = AtomicFile::new(path, WriteCounter::new());
                file.write_if_changed(&content).unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let actual = fs::read(&path).unwrap();
        assert!(expected.iter().any(|candidate| candidate == &actual));
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_file_preserves_its_permission_bits() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.txt");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let mut file = AtomicFile::new(&path, WriteCounter::new());
        assert_eq!(
            file.write_if_changed(b"new").unwrap(),
            Outcome::Written { bytes: 3 }
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn flush_policy_uses_inclusive_boundaries_and_clears_shutdown_after_flush() {
        let t0 = Instant::now();
        let policy = FlushPolicy {
            min_interval: Duration::from_secs(10),
            threshold: 2.0,
        };
        let mut flusher = Flusher::new(policy, t0);
        flusher.mark_dirty(2.0);
        assert!(!flusher.should_flush(t0 + Duration::from_secs(9)));
        assert!(flusher.should_flush(t0 + Duration::from_secs(10)));
        flusher.request_shutdown();
        assert!(flusher.should_flush(t0 + Duration::from_secs(10)));
        flusher.flushed(t0 + Duration::from_secs(10));
        assert!(!flusher.should_flush(t0 + Duration::from_secs(11)));
    }

    #[test]
    fn invalid_flush_thresholds_fall_back_to_the_default() {
        let t0 = Instant::now();
        for threshold in [f32::NAN, -1.0, f32::NEG_INFINITY] {
            let policy = FlushPolicy {
                min_interval: Duration::ZERO,
                threshold,
            };
            let mut flusher = Flusher::new(policy, t0);
            flusher.mark_dirty(DEFAULT_THRESHOLD);
            assert!(flusher.should_flush(t0));
        }
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv64(b"a"), fnv64(b"b"));
    }
}
