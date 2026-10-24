//! Image thumbnail generation. **No egui dependency.**
//!
//! Output is plain RGBA8 bytes ([`Thumbnail`]). The app converts it to a texture in
//! one place, e.g.:
//!
//! ```ignore
//! let img = egui::ColorImage::from_rgba_unmultiplied([t.width as _, t.height as _], &t.rgba);
//! let handle = ctx.load_texture(name, img, Default::default());
//! entry.thumbnail = Some(handle.id());   // keep `handle` alive somewhere!
//! ```

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::{DynamicImage, ImageReader, Limits};

/// Decoded thumbnail, straight (non-premultiplied) RGBA, row-major.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Thumbnail {
    pub fn size(&self) -> [usize; 2] {
        [self.width as usize, self.height as usize]
    }
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

#[derive(Debug)]
pub enum ThumbError {
    Io(std::io::Error),
    Decode(image::ImageError),
    /// `max_edge` was 0.
    InvalidSize,
    UnsupportedMedia,
    VideoToolUnavailable,
    VideoDecode,
    SourceChanged,
    InvalidCacheEntry,
}

impl fmt::Display for ThumbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o error: {e}"),
            Self::Decode(e) => write!(f, "decode error: {e}"),
            Self::InvalidSize => write!(f, "thumbnail size must be > 0"),
            Self::UnsupportedMedia => write!(f, "unsupported media type"),
            Self::VideoToolUnavailable => write!(f, "video thumbnails require ffmpeg"),
            Self::VideoDecode => write!(f, "ffmpeg could not decode a video frame"),
            Self::SourceChanged => {
                write!(f, "source changed while its thumbnail was being generated")
            }
            Self::InvalidCacheEntry => write!(f, "invalid thumbnail cache entry"),
        }
    }
}
impl std::error::Error for ThumbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Decode(error) => Some(error),
            _ => None,
        }
    }
}
impl From<std::io::Error> for ThumbError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<image::ImageError> for ThumbError {
    fn from(e: image::ImageError) -> Self {
        Self::Decode(e)
    }
}

/// Extensions this crate can decode (matches the enabled `image` features).
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff",
];

pub fn is_supported(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        SUPPORTED_EXTENSIONS
            .iter()
            .any(|s| e.eq_ignore_ascii_case(s))
    })
}

/// Decode limits so a hostile/huge image cannot exhaust memory.
fn limits() -> Limits {
    let mut l = Limits::default();
    l.max_image_width = Some(20_000);
    l.max_image_height = Some(20_000);
    l.max_alloc = Some(512 * 1024 * 1024);
    l
}

/// Create a thumbnail whose longest edge is at most `max_edge`, keeping aspect
/// ratio. Never upscales.
pub fn generate(path: &Path, max_edge: u32) -> Result<Thumbnail, ThumbError> {
    if max_edge == 0 {
        return Err(ThumbError::InvalidSize);
    }
    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
    reader.limits(limits());
    finish(reader.decode()?, max_edge)
}

/// Same as [`generate`] for bytes already in memory.
pub fn generate_from_bytes(bytes: &[u8], max_edge: u32) -> Result<Thumbnail, ThumbError> {
    if max_edge == 0 {
        return Err(ThumbError::InvalidSize);
    }
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    reader.limits(limits());
    finish(reader.decode()?, max_edge)
}

fn finish(img: DynamicImage, max_edge: u32) -> Result<Thumbnail, ThumbError> {
    let img = if img.width() > max_edge || img.height() > max_edge {
        img.thumbnail(max_edge, max_edge)
    } else {
        img
    };
    let rgba = img.into_rgba8();
    Ok(Thumbnail {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    path: PathBuf,
    modified: Option<SystemTime>,
    max_edge: u32,
}

/// Small in-memory LRU cache bounded by approximate resident bytes.
///
/// LRU updates use a lazy queue: each hit appends a stamped key, and stale stamps are
/// discarded during eviction. Periodic compaction keeps the queue bounded without an O(n)
/// scan on every cache hit. Entries are keyed by path, modification time, and requested edge;
/// decode failures are cached too to avoid repeating expensive failed decodes every frame.
pub struct ThumbCache {
    map: HashMap<Key, CachedEntry>,
    order: VecDeque<(Key, u64)>,
    bytes: usize,
    max_bytes: usize,
    tick: u64,
}

struct CachedEntry {
    thumbnail: Option<Thumbnail>,
    tick: u64,
}

impl ThumbCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_bytes,
            tick: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    /// Approximate retained bytes, including estimated key and queue overhead.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Cached lookup without decoding. Does not change the LRU order.
    pub fn peek(
        &self,
        path: &Path,
        modified: Option<SystemTime>,
        max_edge: u32,
    ) -> Option<Option<&Thumbnail>> {
        let key = Key {
            path: path.to_owned(),
            modified,
            max_edge,
        };
        self.map.get(&key).map(|entry| entry.thumbnail.as_ref())
    }

    /// Return the cached thumbnail or decode it now (blocking). `None` = undecodable.
    /// For a responsive UI call this from a worker thread, not the frame loop.
    pub fn get_or_generate(
        &mut self,
        path: &Path,
        modified: Option<SystemTime>,
        max_edge: u32,
    ) -> Option<&Thumbnail> {
        let key = Key {
            path: path.to_owned(),
            modified,
            max_edge,
        };
        if self.map.contains_key(&key) {
            self.touch(&key);
        } else {
            let thumbnail = generate(path, max_edge).ok();
            let weight = cache_entry_weight(&key, thumbnail.as_ref());
            let tick = self.next_tick();
            self.bytes = self.bytes.saturating_add(weight);
            self.map
                .insert(key.clone(), CachedEntry { thumbnail, tick });
            self.order.push_back((key.clone(), tick));
            self.compact_order_if_needed();
            self.evict(&key);
        }
        self.map
            .get(&key)
            .and_then(|entry| entry.thumbnail.as_ref())
    }

    fn next_tick(&mut self) -> u64 {
        // Renumber only on the practically unreachable counter wraparound. This also keeps
        // old queue stamps valid and ordered if a long-running process reaches the boundary.
        if self.tick == u64::MAX {
            let mut live: Vec<(u64, Key)> = self
                .map
                .iter()
                .map(|(key, entry)| (entry.tick, key.clone()))
                .collect();
            live.sort_unstable_by_key(|(tick, _)| *tick);
            self.order.clear();
            for (index, (_, key)) in live.into_iter().enumerate() {
                let tick = (index as u64).saturating_add(1);
                if let Some(entry) = self.map.get_mut(&key) {
                    entry.tick = tick;
                }
                self.order.push_back((key, tick));
            }
            self.tick = self.map.len() as u64;
        }
        self.tick += 1;
        self.tick
    }

    fn touch(&mut self, key: &Key) {
        let tick = self.next_tick();
        if let Some(entry) = self.map.get_mut(key) {
            entry.tick = tick;
        } else {
            return;
        }
        self.order.push_back((key.clone(), tick));
        self.compact_order_if_needed();
    }

    fn compact_order_if_needed(&mut self) {
        let limit = self.map.len().saturating_mul(2).saturating_add(64);
        if self.order.len() <= limit {
            return;
        }
        let map = &self.map;
        self.order
            .retain(|(key, tick)| map.get(key).is_some_and(|entry| entry.tick == *tick));
    }

    fn evict(&mut self, keep: &Key) {
        while self.bytes > self.max_bytes {
            let Some((old, tick)) = self.order.pop_front() else {
                break;
            };
            if !self.map.get(&old).is_some_and(|entry| entry.tick == tick) {
                continue;
            }
            // A single entry can exceed the budget; retain it so this API can still return a
            // reference. All other entries are evicted first.
            if &old == keep {
                self.order.push_back((old, tick));
                if self.map.len() == 1 {
                    break;
                }
                continue;
            }
            if let Some(entry) = self.map.remove(&old) {
                self.bytes = self
                    .bytes
                    .saturating_sub(cache_entry_weight(&old, entry.thumbnail.as_ref()));
            }
        }
    }
}

fn cache_entry_weight(key: &Key, thumbnail: Option<&Thumbnail>) -> usize {
    key.path
        .as_os_str()
        .len()
        .saturating_mul(2)
        .saturating_add(192)
        .saturating_add(thumbnail.map_or(0, Thumbnail::byte_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, Rgba([10, 20, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn downscales_keeping_aspect() {
        let t = generate_from_bytes(&png_bytes(400, 200), 100).unwrap();
        assert_eq!((t.width, t.height), (100, 50));
        assert_eq!(t.rgba.len(), 100 * 50 * 4);
        assert_eq!(&t.rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn never_upscales() {
        let t = generate_from_bytes(&png_bytes(32, 16), 256).unwrap();
        assert_eq!((t.width, t.height), (32, 16));
    }

    #[test]
    fn errors() {
        assert!(matches!(
            generate_from_bytes(&png_bytes(4, 4), 0),
            Err(ThumbError::InvalidSize)
        ));
        assert!(generate_from_bytes(b"not an image", 64).is_err());
        assert!(matches!(
            generate(Path::new("/definitely/missing.png"), 64),
            Err(ThumbError::Io(_))
        ));
    }

    #[test]
    fn supported_ext() {
        assert!(is_supported(Path::new("a.JPG")));
        assert!(!is_supported(Path::new("a.txt")));
    }

    #[test]
    fn cache_lru_queue_stays_bounded_and_keeps_recent_hits() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.png");
        let b = dir.path().join("b.png");
        let c_path = dir.path().join("c.png");
        for path in [&a, &b, &c_path] {
            std::fs::write(path, png_bytes(32, 32)).unwrap();
        }

        let entry_weight = a.as_os_str().len() * 2 + 192 + 32 * 32 * 4;
        let mut cache = ThumbCache::new(entry_weight * 2 + 1);
        assert!(cache.get_or_generate(&a, None, 32).is_some());
        assert!(cache.get_or_generate(&b, None, 32).is_some());
        for _ in 0..200 {
            assert!(cache.get_or_generate(&a, None, 32).is_some());
        }
        assert!(cache.get_or_generate(&c_path, None, 32).is_some());

        assert!(cache.peek(&a, None, 32).is_some());
        assert!(cache.peek(&b, None, 32).is_none());
        assert!(cache.peek(&c_path, None, 32).is_some());
        assert!(cache.order.len() <= cache.map.len() * 2 + 64);
    }

    #[test]
    fn cache_hits_and_evicts() {
        let dir = std::env::temp_dir().join(format!("fm_thumbs_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.png"), dir.join("b.png"));
        std::fs::write(&a, png_bytes(64, 64)).unwrap();
        std::fs::write(&b, png_bytes(64, 64)).unwrap();

        let mut c = ThumbCache::new(64 * 64 * 4 + 10); // room for ~1 thumbnail
        assert!(c.get_or_generate(&a, None, 64).is_some());
        assert!(c.peek(&a, None, 64).is_some());
        assert!(c.get_or_generate(&b, None, 64).is_some());
        assert!(c.peek(&a, None, 64).is_none(), "a should have been evicted");
        assert!(c.peek(&b, None, 64).is_some());
        // failures are cached as None
        assert!(
            c.get_or_generate(&dir.join("missing.png"), None, 64)
                .is_none()
        );
        assert_eq!(c.peek(&dir.join("missing.png"), None, 64), Some(None));
        std::fs::remove_dir_all(&dir).ok();
    }
}

// ---------------------------------------------------------------------------
// Persistent cache, audit and video support
// ---------------------------------------------------------------------------

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, UNIX_EPOCH};

use fm_persist::WriteCounter;
use image::ImageEncoder;
use walkdir::WalkDir;

const DISK_MAGIC: &[u8; 8] = b"FMTHMB01";
const DISK_CACHE_VERSION: u32 = 1;
const MAX_DISK_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const PRUNE_INTERVAL_WRITES: usize = 16;
const MAX_SOURCE_PATH_BYTES: usize = 1024 * 1024;
const MAX_AUDIT_SAMPLES: usize = 100;
/// Decoded thumbnails kept in process memory (LRU), so re-requests never re-decode.
/// This sits behind the GPU texture cache (which already holds decoded thumbnails for
/// everything currently on screen), so it only needs to cover scroll-back, not a second
/// full copy of the working set; keep it well below the GPU cache's own budget.
const RAM_CACHE_BYTES: usize = 16 * 1024 * 1024;
/// Pending thumbnail bytes plus estimated path/index overhead; oldest entries are dropped above this.
const PENDING_BYTES_CAP: usize = 16 * 1024 * 1024;
static NEXT_DISK_TEMP_ID: AtomicU64 = AtomicU64::new(0);
static FFMPEG_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Common video types that can be thumbnailed when `ffmpeg` is available.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "3g2", "3gp", "avi", "divx", "f4v", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg",
    "mts", "mxf", "ogv", "rm", "rmvb", "ts", "vob", "webm", "wmv",
];

/// Common raster/vector/camera formats that may be images but are not decoded by this build.
/// Audits report them as unsupported instead of incorrectly claiming their thumbnail is absent.
const UNSUPPORTED_IMAGE_EXTENSIONS: &[&str] = &[
    "arw", "avif", "cr2", "cr3", "dng", "heic", "heif", "jxl", "nef", "orf", "pef", "psd", "raf",
    "raw", "rw2", "srw", "svg", "xcf",
];

/// High-level source media type supported by the thumbnail store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Other,
}

/// The cache state observed by a read-only audit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbnailStatus {
    Present,
    /// Not on disk *by design*: cheap to regenerate, so it lives in RAM only.
    NotPersisted,
    Missing,
    Stale,
    Corrupt,
    Unsupported,
    Error,
}

#[derive(Clone, Debug)]
pub struct ThumbnailAuditEntry {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub status: ThumbnailStatus,
    pub detail: Option<String>,
}

/// Bounded-summary report from an audit. `samples` stores at most 100 paths that need
/// attention; use `ThumbnailStore::audit_each` to consume every result without retaining it.
#[derive(Clone, Debug, Default)]
pub struct ThumbnailAudit {
    pub scanned_media: u64,
    pub images: u64,
    pub videos: u64,
    pub present: u64,
    /// Media the persist policy deliberately keeps out of the disk cache.
    pub not_persisted: u64,
    pub missing: u64,
    pub stale: u64,
    pub corrupt: u64,
    pub unsupported: u64,
    pub errors: u64,
    pub traversal_errors: u64,
    pub samples: Vec<ThumbnailAuditEntry>,
    pub samples_truncated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ThumbnailGenerationReport {
    pub scanned_media: u64,
    pub generated: u64,
    pub reused: u64,
    /// Skipped because the persist policy says they are cheaper to re-decode than to store.
    pub not_persisted: u64,
    pub unsupported: u64,
    pub failed: u64,
    pub traversal_errors: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnsureReport {
    /// `true` if the returned thumbnail came from the RAM cache or a valid persistent cache
    /// entry instead of being decoded now.
    pub reused: bool,
    /// Size of the source file whose content the thumbnail represents.
    pub source_size: u64,
    /// Modification time of the source file whose content the thumbnail represents.
    pub source_modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourceFingerprint {
    size: u64,
    modified_seconds: u64,
    modified_nanos: u32,
}

impl SourceFingerprint {
    fn read(path: &Path) -> Result<Self, ThumbError> {
        let metadata = fs::metadata(path)?;
        let modified = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Ok(Self {
            size: metadata.len(),
            modified_seconds: modified.as_secs(),
            modified_nanos: modified.subsec_nanos(),
        })
    }
}

fn ensure_report(fingerprint: SourceFingerprint, reused: bool) -> EnsureReport {
    EnsureReport {
        reused,
        source_size: fingerprint.size,
        source_modified: UNIX_EPOCH.checked_add(Duration::new(
            fingerprint.modified_seconds,
            fingerprint.modified_nanos,
        )),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheState {
    Missing,
    Stale,
    Corrupt,
}

enum CacheRead {
    Present(Thumbnail),
    State(CacheState),
}

/// Decides whether a thumbnail is worth writing to disk: persist only when
/// `decode_cost × reuse_probability` exceeds the cost of the write. In practice that means
/// videos and very large images in *hot* folders; small images are cheaper to re-decode
/// than to store and stay in RAM only.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PersistPolicy {
    /// Cost of one write in "milliseconds of decode work" (already weighted for flash wear).
    pub write_cost_ms: f32,
    /// Probability that a thumbnail of a hot-folder file is needed again.
    pub hot_reuse: f32,
    /// Probability that a thumbnail of any other file is needed again.
    pub cold_reuse: f32,
}

impl Default for PersistPolicy {
    fn default() -> Self {
        Self {
            write_cost_ms: 24.0,
            hot_reuse: 0.6,
            cold_reuse: 0.05,
        }
    }
}

impl PersistPolicy {
    /// Persist every generated thumbnail (the pre-V4 behaviour).
    pub fn always() -> Self {
        Self {
            write_cost_ms: 0.0,
            hot_reuse: 1.0,
            cold_reuse: 1.0,
        }
    }

    /// Never persist; thumbnails live in RAM only.
    pub fn never() -> Self {
        Self {
            write_cost_ms: f32::INFINITY,
            ..Self::default()
        }
    }

    /// Rough estimate of the time to decode and scale a source again.
    pub fn estimated_decode_ms(kind: MediaKind, extension: &str, source_bytes: u64) -> f32 {
        match kind {
            MediaKind::Video => 400.0,
            MediaKind::Image => {
                let mib = source_bytes as f32 / (1024.0 * 1024.0);
                let per_mib = match extension {
                    "png" => 40.0,
                    "webp" => 30.0,
                    "gif" => 25.0,
                    "jpg" | "jpeg" => 20.0,
                    _ => 15.0,
                };
                2.0 + per_mib * mib
            }
            MediaKind::Other => 0.0,
        }
    }

    /// Whether a thumbnail of this source should be written to the persistent cache.
    /// `extension` must be lowercase; `hot` says the file lives in a hot-tier folder.
    pub fn should_persist(
        &self,
        kind: MediaKind,
        extension: &str,
        source_bytes: u64,
        hot: bool,
    ) -> bool {
        if kind == MediaKind::Other {
            return false;
        }
        let reuse = if hot { self.hot_reuse } else { self.cold_reuse };
        Self::estimated_decode_ms(kind, extension, source_bytes) * reuse > self.write_cost_ms
    }
}

fn extension_lower(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

/// Outcome of [`ThumbnailStore::flush_pending`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PersistReport {
    /// Cache files written.
    pub written: u64,
    /// Bytes written.
    pub bytes: u64,
    /// Entries that could not be written.
    pub failed: u64,
    /// Entries discarded earlier because the pending queue exceeded its size cap.
    pub dropped_before_flush: u64,
}

struct RamEntry {
    fingerprint: SourceFingerprint,
    thumbnail: Arc<Thumbnail>,
    tick: u64,
}

struct RamCache {
    map: HashMap<PathBuf, RamEntry>,
    // Old stamps become inert after a cache hit. Compaction bounds stale queue entries.
    order: VecDeque<(PathBuf, u64)>,
    bytes: usize,
    tick: u64,
    max_bytes: usize,
}

impl RamCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            tick: 0,
            max_bytes,
        }
    }

    fn get(&mut self, path: &Path, fingerprint: SourceFingerprint) -> Option<Arc<Thumbnail>> {
        let tick = self.next_tick();
        let thumbnail = {
            let entry = self.map.get_mut(path)?;
            if entry.fingerprint != fingerprint {
                return None;
            }
            entry.tick = tick;
            Arc::clone(&entry.thumbnail)
        };
        self.order.push_back((path.to_path_buf(), tick));
        self.compact_order_if_needed();
        Some(thumbnail)
    }

    fn put(&mut self, path: &Path, fingerprint: SourceFingerprint, thumbnail: &Thumbnail) {
        let weight = ram_entry_weight(path, thumbnail);
        if let Some(old) = self.map.remove(path) {
            self.bytes = self
                .bytes
                .saturating_sub(ram_entry_weight(path, &old.thumbnail));
        }
        if weight > self.max_bytes {
            self.compact_order_if_needed();
            return;
        }
        while self.bytes.saturating_add(weight) > self.max_bytes {
            let Some((victim, queued_tick)) = self.order.pop_front() else {
                break;
            };
            if !self
                .map
                .get(&victim)
                .is_some_and(|entry| entry.tick == queued_tick)
            {
                continue;
            }
            if let Some(old) = self.map.remove(&victim) {
                self.bytes = self
                    .bytes
                    .saturating_sub(ram_entry_weight(&victim, &old.thumbnail));
            }
        }
        let tick = self.next_tick();
        self.bytes = self.bytes.saturating_add(weight);
        self.map.insert(
            path.to_path_buf(),
            RamEntry {
                fingerprint,
                thumbnail: Arc::new(thumbnail.clone()),
                tick,
            },
        );
        self.order.push_back((path.to_path_buf(), tick));
        self.compact_order_if_needed();
    }

    fn next_tick(&mut self) -> u64 {
        if self.tick == u64::MAX {
            let mut live: Vec<(u64, PathBuf)> = self
                .map
                .iter()
                .map(|(path, entry)| (entry.tick, path.clone()))
                .collect();
            live.sort_unstable_by_key(|(tick, _)| *tick);
            self.order.clear();
            for (index, (_, path)) in live.into_iter().enumerate() {
                let tick = (index as u64).saturating_add(1);
                if let Some(entry) = self.map.get_mut(&path) {
                    entry.tick = tick;
                }
                self.order.push_back((path, tick));
            }
            self.tick = self.map.len() as u64;
        }
        self.tick += 1;
        self.tick
    }

    fn compact_order_if_needed(&mut self) {
        let limit = self.map.len().saturating_mul(2).saturating_add(64);
        if self.order.len() <= limit {
            return;
        }
        let map = &self.map;
        self.order
            .retain(|(path, tick)| map.get(path).is_some_and(|entry| entry.tick == *tick));
    }
}

fn ram_entry_weight(path: &Path, thumbnail: &Thumbnail) -> usize {
    path.as_os_str()
        .len()
        .saturating_mul(2)
        .saturating_add(256)
        .saturating_add(thumbnail.byte_len())
}

struct PendingWrite {
    kind: MediaKind,
    fingerprint: SourceFingerprint,
    png: Vec<u8>,
    sequence: u64,
}

fn pending_entry_weight(path: &Path, png_bytes: usize) -> usize {
    png_bytes
        .saturating_add(path.as_os_str().len().saturating_mul(2))
        .saturating_add(256)
}

#[derive(Default)]
struct PendingQueue {
    map: HashMap<PathBuf, PendingWrite>,
    order: VecDeque<(PathBuf, u64)>,
    bytes: usize,
    dropped: u64,
    sequence: u64,
}

impl PendingQueue {
    fn push(&mut self, path: PathBuf, mut write: PendingWrite) {
        if let Some(old) = self.map.remove(&path) {
            self.bytes = self
                .bytes
                .saturating_sub(pending_entry_weight(&path, old.png.len()));
        }
        // A single entry larger than the whole queue budget can never fit. Drop it directly
        // rather than briefly allocating queue metadata for an entry that will be discarded.
        if pending_entry_weight(&path, write.png.len()) > PENDING_BYTES_CAP {
            self.dropped = self.dropped.saturating_add(1);
            self.compact_order_if_needed();
            return;
        }
        write.sequence = self.next_sequence();
        let sequence = write.sequence;
        self.bytes = self
            .bytes
            .saturating_add(pending_entry_weight(&path, write.png.len()));
        self.order.push_back((path.clone(), sequence));
        self.map.insert(path, write);

        while self.bytes > PENDING_BYTES_CAP {
            let Some((oldest, queued_sequence)) = self.order.pop_front() else {
                break;
            };
            if !self
                .map
                .get(&oldest)
                .is_some_and(|write| write.sequence == queued_sequence)
            {
                continue;
            }
            if let Some(old) = self.map.remove(&oldest) {
                self.bytes = self
                    .bytes
                    .saturating_sub(pending_entry_weight(&oldest, old.png.len()));
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        self.compact_order_if_needed();
    }

    fn next_sequence(&mut self) -> u64 {
        if self.sequence == u64::MAX {
            let mut live: Vec<(u64, PathBuf)> = self
                .map
                .iter()
                .map(|(path, write)| (write.sequence, path.clone()))
                .collect();
            live.sort_unstable_by_key(|(sequence, _)| *sequence);
            self.order.clear();
            for (index, (_, path)) in live.into_iter().enumerate() {
                let sequence = (index as u64).saturating_add(1);
                if let Some(write) = self.map.get_mut(&path) {
                    write.sequence = sequence;
                }
                self.order.push_back((path, sequence));
            }
            self.sequence = self.map.len() as u64;
        }
        self.sequence += 1;
        self.sequence
    }

    fn compact_order_if_needed(&mut self) {
        let limit = self.map.len().saturating_mul(2).saturating_add(64);
        if self.order.len() <= limit {
            return;
        }
        let map = &self.map;
        self.order.retain(|(path, sequence)| {
            map.get(path)
                .is_some_and(|write| write.sequence == *sequence)
        });
    }

    fn take_all(&mut self) -> Vec<(PathBuf, PendingWrite)> {
        self.order.clear();
        self.bytes = 0;
        std::mem::take(&mut self.map).into_iter().collect()
    }
}

/// Persistent thumbnail cache. Valid thumbnails are re-used without rewriting any cache
/// file. Locking is per cache key, so concurrent requests for the same media share one
/// generation while unrelated files can be processed independently.
pub struct ThumbnailStore {
    cache_dir: PathBuf,
    max_edge: u32,
    locks: Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>,
    writes_since_prune: AtomicUsize,
    policy: PersistPolicy,
    counter: WriteCounter,
    ram: Mutex<RamCache>,
    pending: Mutex<PendingQueue>,
}

impl ThumbnailStore {
    pub fn new(cache_dir: impl Into<PathBuf>, max_edge: u32) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            max_edge: max_edge.clamp(16, 2048),
            locks: Mutex::new(HashMap::new()),
            writes_since_prune: AtomicUsize::new(0),
            policy: PersistPolicy::default(),
            counter: WriteCounter::new(),
            ram: Mutex::new(RamCache::new(RAM_CACHE_BYTES)),
            pending: Mutex::new(PendingQueue::default()),
        }
    }

    /// Override the in-memory decoded-thumbnail cache size (default: [`RAM_CACHE_BYTES`]).
    /// Lets a host size this independently of its own GPU/texture-side cache, e.g. from a
    /// user-editable settings file instead of a compiled-in constant.
    pub fn with_ram_budget_bytes(self, max_bytes: usize) -> Self {
        Self {
            ram: Mutex::new(RamCache::new(max_bytes)),
            ..self
        }
    }

    /// Replace the persist policy (default: [`PersistPolicy::default`], cost based).
    pub fn with_policy(mut self, policy: PersistPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Report every byte this store writes to `counter` (shared with other writers).
    pub fn with_write_counter(mut self, counter: WriteCounter) -> Self {
        self.counter = counter;
        self
    }

    /// The active persist policy.
    pub fn policy(&self) -> PersistPolicy {
        self.policy
    }

    /// Total bytes this store has written to disk.
    pub fn bytes_written(&self) -> u64 {
        self.counter.bytes()
    }

    /// Number of encoded thumbnails waiting for [`flush_pending`](Self::flush_pending).
    pub fn pending_writes(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }

    fn ram_get(&self, path: &Path, fingerprint: SourceFingerprint) -> Option<Arc<Thumbnail>> {
        self.ram
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path, fingerprint)
    }

    fn ram_put(&self, path: &Path, fingerprint: SourceFingerprint, thumbnail: &Thumbnail) {
        self.ram
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(path, fingerprint, thumbnail);
    }

    /// Write every queued thumbnail to the persistent cache in one batch. Call this at the
    /// same rare moments as the other state flushes (periodically and at shutdown). Files are
    /// replaced atomically by rename without `fsync`: a torn file is detected as corrupt on
    /// the next read and simply regenerated.
    pub fn flush_pending(&self) -> PersistReport {
        let (items, dropped) = {
            let mut queue = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let dropped = std::mem::take(&mut queue.dropped);
            (queue.take_all(), dropped)
        };
        let mut report = PersistReport {
            dropped_before_flush: dropped,
            ..PersistReport::default()
        };
        for (source, write) in items {
            let cache_path = self.cache_path(&source);
            let lock = self.key_lock(&cache_path);
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let result = SourceFingerprint::read(&source)
                .and_then(|current| {
                    if current != write.fingerprint {
                        return Err(ThumbError::SourceChanged);
                    }
                    self.build_disk_entry(&source, write.kind, write.fingerprint, &write.png)
                })
                .and_then(|bytes| self.write_entry(&cache_path, &bytes).map(|()| bytes.len()));
            match result {
                Ok(len) => {
                    report.written += 1;
                    report.bytes += len as u64;
                }
                Err(_) => report.failed += 1,
            }
        }
        if report.written > 0 {
            let written = report.written as usize;
            let total = self
                .writes_since_prune
                .fetch_add(written, Ordering::Relaxed)
                .wrapping_add(written);
            if total / PRUNE_INTERVAL_WRITES != (total - written) / PRUNE_INTERVAL_WRITES {
                self.prune_cache();
            }
        }
        report
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn max_edge(&self) -> u32 {
        self.max_edge
    }

    /// Return `true` only when the path has an image/video extension this build recognises.
    pub fn is_supported_media(path: &Path) -> bool {
        matches!(media_kind(path), Some(MediaKind::Image | MediaKind::Video))
    }

    /// Load a valid cached thumbnail, or generate and persist it when missing/stale/corrupt.
    /// Repeated calls do not rewrite a valid cache entry.
    pub fn get_or_generate(&self, path: &Path) -> Result<Thumbnail, ThumbError> {
        self.ensure(path).map(|(thumbnail, _)| thumbnail)
    }

    /// Same as [`get_or_generate`], while reporting whether a disk-cache hit occurred.
    pub fn ensure(&self, path: &Path) -> Result<(Thumbnail, EnsureReport), ThumbError> {
        self.ensure_hinted(path, false)
    }

    /// Like [`ensure`](Self::ensure), telling the persist policy whether `path` lives in a
    /// hot-tier folder (which makes an expensive thumbnail worth keeping on disk).
    ///
    /// Order of lookups: RAM LRU, then the persistent cache, then decode. A freshly decoded
    /// thumbnail is always kept in RAM; it is only *queued* for disk when the policy says it
    /// is worth it, and reaches the disk at the next [`flush_pending`](Self::flush_pending).
    pub fn ensure_hinted(
        &self,
        path: &Path,
        hot: bool,
    ) -> Result<(Thumbnail, EnsureReport), ThumbError> {
        let kind = media_kind(path).ok_or(ThumbError::UnsupportedMedia)?;
        if self.max_edge == 0 {
            return Err(ThumbError::InvalidSize);
        }
        let cache_path = self.cache_path(path);
        let lock = self.key_lock(&cache_path);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());

        // Re-read the source fingerprint under the per-key lock. If a second request
        // generated the thumbnail while this one waited, this check turns it into a hit.
        let before = SourceFingerprint::read(path)?;
        if let Some(thumbnail) = self.ram_get(path, before) {
            // Clone pixel data after releasing the RAM-cache mutex so concurrent workers can
            // continue hitting the cache while this caller prepares its owned result.
            return Ok((thumbnail.as_ref().clone(), ensure_report(before, true)));
        }
        match self.read_cache(path, kind, before)? {
            CacheRead::Present(thumbnail) => {
                self.ram_put(path, before, &thumbnail);
                return Ok((thumbnail, ensure_report(before, true)));
            }
            CacheRead::State(CacheState::Missing | CacheState::Stale | CacheState::Corrupt) => {}
        }
        if kind == MediaKind::Video && !ffmpeg_available() {
            return Err(ThumbError::VideoToolUnavailable);
        }
        let thumbnail = match kind {
            MediaKind::Image => generate(path, self.max_edge)?,
            MediaKind::Video => generate_video(path, self.max_edge)?,
            MediaKind::Other => return Err(ThumbError::UnsupportedMedia),
        };
        let after = SourceFingerprint::read(path)?;
        if after != before {
            return Err(ThumbError::SourceChanged);
        }
        if self
            .policy
            .should_persist(kind, &extension_lower(path), before.size, hot)
        {
            let png = encode_png(&thumbnail)?;
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).push(
                path.to_path_buf(),
                PendingWrite {
                    kind,
                    fingerprint: before,
                    png,
                    sequence: 0,
                },
            );
        }
        self.ram_put(path, before, &thumbnail);
        Ok((thumbnail, ensure_report(before, false)))
    }

    /// Classify one path without creating, repairing, or modifying any files.
    pub fn audit_file(&self, path: &Path) -> ThumbnailAuditEntry {
        let Some(kind) = media_kind(path) else {
            return ThumbnailAuditEntry {
                path: path.to_path_buf(),
                kind: MediaKind::Other,
                status: ThumbnailStatus::Unsupported,
                detail: Some("not a recognized thumbnail candidate".into()),
            };
        };
        if kind == MediaKind::Other {
            return ThumbnailAuditEntry {
                path: path.to_path_buf(),
                kind,
                status: ThumbnailStatus::Unsupported,
                detail: Some(
                    "media format is recognized but unsupported by the configured decoders".into(),
                ),
            };
        }
        let extension = extension_lower(path);
        let cache_path = self.cache_path(path);
        let key_lock = self.key_lock(&cache_path);
        let _guard = key_lock.lock().unwrap_or_else(|error| error.into_inner());
        let check = SourceFingerprint::read(path).and_then(|fingerprint| {
            self.read_cache(path, kind, fingerprint)
                .map(|read| (read, fingerprint))
        });
        drop(_guard);
        // Judged as if the file were in a hot folder: that is the most generous case.
        let worth_persisting = |fingerprint: &SourceFingerprint| {
            self.policy
                .should_persist(kind, &extension, fingerprint.size, true)
        };
        let status = match &check {
            Ok((CacheRead::Present(_), _)) => ThumbnailStatus::Present,
            Ok((CacheRead::State(CacheState::Missing | CacheState::Stale), fingerprint)) => {
                if kind == MediaKind::Video && !ffmpeg_available() {
                    ThumbnailStatus::Unsupported
                } else if !worth_persisting(fingerprint) {
                    ThumbnailStatus::NotPersisted
                } else if matches!(&check, Ok((CacheRead::State(CacheState::Stale), _))) {
                    ThumbnailStatus::Stale
                } else {
                    ThumbnailStatus::Missing
                }
            }
            Ok((CacheRead::State(CacheState::Corrupt), _)) => ThumbnailStatus::Corrupt,
            Err(_) => ThumbnailStatus::Error,
        };
        let detail = match status {
            ThumbnailStatus::Unsupported if kind == MediaKind::Video => {
                Some("video thumbnails require ffmpeg".into())
            }
            ThumbnailStatus::Error => match &check {
                Err(e) => Some(e.to_string()),
                _ => None,
            },
            _ => None,
        };
        ThumbnailAuditEntry {
            path: path.to_path_buf(),
            kind,
            status,
            detail,
        }
    }

    /// Audit a tree recursively. It performs no writes and retains at most 100 diagnostic
    /// paths, independent of tree size. Symlinks are not followed.
    pub fn audit_tree(&self, root: &Path) -> ThumbnailAudit {
        let mut summary = ThumbnailAudit::default();
        let traversal_errors = self.audit_each(root, |entry| record_audit(&mut summary, entry));
        summary.traversal_errors = traversal_errors;
        summary
    }

    /// Stream every recognized media path and its cache status to `visitor`; this is suitable
    /// for very large trees because no unbounded result vector is retained.
    pub fn audit_each(&self, root: &Path, mut visitor: impl FnMut(&ThumbnailAuditEntry)) -> u64 {
        let mut traversal_errors = 0_u64;
        for item in WalkDir::new(root).follow_links(false) {
            let entry = match item {
                Ok(entry) => entry,
                Err(_) => {
                    traversal_errors = traversal_errors.saturating_add(1);
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if media_kind(path).is_none() {
                continue;
            }
            visitor(&self.audit_file(path));
        }
        traversal_errors
    }

    /// Explicitly generate missing, stale or corrupt thumbnails. Existing valid thumbnails
    /// are never regenerated. No source files are modified.
    pub fn generate_needed(&self, root: &Path) -> ThumbnailGenerationReport {
        let mut report = ThumbnailGenerationReport::default();
        for item in WalkDir::new(root).follow_links(false) {
            let entry = match item {
                Ok(entry) => entry,
                Err(_) => {
                    report.traversal_errors = report.traversal_errors.saturating_add(1);
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(kind) = media_kind(entry.path()) else {
                continue;
            };
            report.scanned_media = report.scanned_media.saturating_add(1);
            if matches!(kind, MediaKind::Image | MediaKind::Video) {
                let size = match entry.metadata() {
                    Ok(metadata) => metadata.len(),
                    Err(_) => {
                        report.failed = report.failed.saturating_add(1);
                        continue;
                    }
                };
                if !self
                    .policy
                    .should_persist(kind, &extension_lower(entry.path()), size, true)
                {
                    report.not_persisted = report.not_persisted.saturating_add(1);
                    continue;
                }
            }
            match self.ensure_hinted(entry.path(), true) {
                Ok((_, outcome)) if outcome.reused => {
                    report.reused = report.reused.saturating_add(1)
                }
                Ok((_, _)) => report.generated = report.generated.saturating_add(1),
                Err(ThumbError::UnsupportedMedia | ThumbError::VideoToolUnavailable) => {
                    report.unsupported = report.unsupported.saturating_add(1)
                }
                Err(_) => report.failed = report.failed.saturating_add(1),
            }
        }
        // An explicit maintenance run should leave its results on disk right away.
        let flushed = self.flush_pending();
        report.failed = report
            .failed
            .saturating_add(flushed.failed)
            .saturating_add(flushed.dropped_before_flush);
        report
    }

    fn cache_path(&self, source: &Path) -> PathBuf {
        let mut hash = 0xcbf29ce484222325_u64;
        for byte in source
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .copied()
            .chain(self.max_edge.to_le_bytes())
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        self.cache_dir.join(format!("{hash:016x}.fmt"))
    }

    fn key_lock(&self, cache_path: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, weak| weak.strong_count() > 0);
        if let Some(lock) = locks.get(cache_path).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(cache_path.to_path_buf(), Arc::downgrade(&lock));
        lock
    }

    fn read_cache(
        &self,
        source: &Path,
        kind: MediaKind,
        fingerprint: SourceFingerprint,
    ) -> Result<CacheRead, ThumbError> {
        let cache_path = self.cache_path(source);
        let file = match fs::File::open(&cache_path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CacheRead::State(CacheState::Missing));
            }
            Err(e) => return Err(ThumbError::Io(e)),
        };
        let size = file.metadata()?.len();
        if size > MAX_DISK_ENTRY_BYTES {
            return Ok(CacheRead::State(CacheState::Corrupt));
        }
        let mut bytes = Vec::with_capacity(size as usize);
        file.take(MAX_DISK_ENTRY_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_DISK_ENTRY_BYTES {
            return Ok(CacheRead::State(CacheState::Corrupt));
        }
        let parsed = match parse_disk_entry(&bytes) {
            Some(parsed) => parsed,
            None => return Ok(CacheRead::State(CacheState::Corrupt)),
        };
        if parsed.source != source.as_os_str().as_encoded_bytes() {
            return Ok(CacheRead::State(CacheState::Corrupt));
        }
        if parsed.version != DISK_CACHE_VERSION
            || parsed.edge != self.max_edge
            || parsed.kind != kind
        {
            return Ok(CacheRead::State(CacheState::Stale));
        }
        if parsed.fingerprint != fingerprint {
            return Ok(CacheRead::State(CacheState::Stale));
        }
        match decode_png(parsed.png, self.max_edge) {
            Ok(thumbnail) => Ok(CacheRead::Present(thumbnail)),
            Err(_) => Ok(CacheRead::State(CacheState::Corrupt)),
        }
    }

    fn build_disk_entry(
        &self,
        source: &Path,
        kind: MediaKind,
        fingerprint: SourceFingerprint,
        png: &[u8],
    ) -> Result<Vec<u8>, ThumbError> {
        let source_bytes = source.as_os_str().as_encoded_bytes();
        if source_bytes.len() > MAX_SOURCE_PATH_BYTES || png.len() as u64 > MAX_DISK_ENTRY_BYTES {
            return Err(ThumbError::InvalidCacheEntry);
        }
        let mut bytes =
            Vec::with_capacity(8 + 4 + 1 + 4 + 8 + 8 + 4 + 4 + source_bytes.len() + png.len());
        bytes.extend_from_slice(DISK_MAGIC);
        bytes.extend_from_slice(&DISK_CACHE_VERSION.to_le_bytes());
        bytes.push(match kind {
            MediaKind::Image => 1,
            MediaKind::Video => 2,
            MediaKind::Other => return Err(ThumbError::UnsupportedMedia),
        });
        bytes.extend_from_slice(&self.max_edge.to_le_bytes());
        bytes.extend_from_slice(&fingerprint.size.to_le_bytes());
        bytes.extend_from_slice(&fingerprint.modified_seconds.to_le_bytes());
        bytes.extend_from_slice(&fingerprint.modified_nanos.to_le_bytes());
        bytes.extend_from_slice(&(source_bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(source_bytes);
        bytes.extend_from_slice(png);
        if bytes.len() as u64 > MAX_DISK_ENTRY_BYTES {
            return Err(ThumbError::InvalidCacheEntry);
        }
        Ok(bytes)
    }

    /// Replace one cache file atomically. Derived data: no `fsync` of the file or directory.
    fn write_entry(&self, cache_path: &Path, bytes: &[u8]) -> Result<(), ThumbError> {
        let parent = &self.cache_dir;
        fs::create_dir_all(parent)?;
        let (temporary, mut file) = loop {
            let suffix = NEXT_DISK_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(".thumb-{}-{suffix}.tmp", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => break (candidate, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(ThumbError::Io(e)),
            }
        };
        if let Err(e) = file.write_all(bytes) {
            let _ = fs::remove_file(&temporary);
            return Err(ThumbError::Io(e));
        }
        drop(file);
        if let Err(e) = fs::rename(&temporary, cache_path) {
            let _ = fs::remove_file(&temporary);
            return Err(ThumbError::Io(e));
        }
        self.counter.add_write(bytes.len() as u64);
        Ok(())
    }

    /// Evict oldest derived cache entries only when the persistent cache grows beyond its
    /// budget. Entries currently in use by another worker are skipped until a later pass.
    fn prune_cache(&self) {
        self.prune_cache_to(MAX_CACHE_BYTES);
    }

    fn prune_cache_to(&self, budget: u64) {
        let read_dir = match fs::read_dir(&self.cache_dir) {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("Could not inspect thumbnail cache for pruning: {error}");
                return;
            }
        };
        let mut files = Vec::new();
        let mut total_bytes = 0_u64;
        for item in read_dir {
            let entry = match item {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("fmt") {
                continue;
            }
            if !entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) if metadata.is_file() => metadata,
                _ => continue,
            };
            let size = metadata.len();
            total_bytes = total_bytes.saturating_add(size);
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            files.push((modified, entry.path(), size));
        }
        if total_bytes <= budget {
            return;
        }
        files.sort_unstable_by_key(|(modified, _, _)| *modified);
        let mut failed_removals = 0_u64;
        for (_, path, size) in files {
            if total_bytes <= budget {
                break;
            }
            let key_lock = self.key_lock(&path);
            let Ok(_guard) = key_lock.try_lock() else {
                continue;
            };
            match fs::remove_file(&path) {
                Ok(()) => total_bytes = total_bytes.saturating_sub(size),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => failed_removals = failed_removals.saturating_add(1),
            }
        }
        if total_bytes > budget || failed_removals > 0 {
            eprintln!(
                "Thumbnail cache pruning incomplete: {total_bytes} bytes remain (budget {budget}); {failed_removals} removal(s) failed or were deferred."
            );
        }
    }
}

struct ParsedDiskEntry<'a> {
    version: u32,
    kind: MediaKind,
    edge: u32,
    fingerprint: SourceFingerprint,
    source: &'a [u8],
    png: &'a [u8],
}

fn parse_disk_entry(bytes: &[u8]) -> Option<ParsedDiskEntry<'_>> {
    let mut cursor = 0_usize;
    let magic = take(bytes, &mut cursor, DISK_MAGIC.len())?;
    if magic != DISK_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(take(bytes, &mut cursor, 4)?.try_into().ok()?);
    let kind = match *take(bytes, &mut cursor, 1)?.first()? {
        1 => MediaKind::Image,
        2 => MediaKind::Video,
        _ => return None,
    };
    let edge = u32::from_le_bytes(take(bytes, &mut cursor, 4)?.try_into().ok()?);
    let size = u64::from_le_bytes(take(bytes, &mut cursor, 8)?.try_into().ok()?);
    let modified_seconds = u64::from_le_bytes(take(bytes, &mut cursor, 8)?.try_into().ok()?);
    let modified_nanos = u32::from_le_bytes(take(bytes, &mut cursor, 4)?.try_into().ok()?);
    if modified_nanos >= 1_000_000_000 {
        return None;
    }
    let path_len = u32::from_le_bytes(take(bytes, &mut cursor, 4)?.try_into().ok()?) as usize;
    if path_len > MAX_SOURCE_PATH_BYTES {
        return None;
    }
    let source = take(bytes, &mut cursor, path_len)?;
    let png_len = bytes.len().checked_sub(cursor)?;
    let png = take(bytes, &mut cursor, png_len)?;
    if png.is_empty() {
        return None;
    }
    Some(ParsedDiskEntry {
        version,
        kind,
        edge,
        fingerprint: SourceFingerprint {
            size,
            modified_seconds,
            modified_nanos,
        },
        source,
        png,
    })
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = cursor.checked_add(len)?;
    let result = bytes.get(*cursor..end)?;
    *cursor = end;
    Some(result)
}

fn decode_png(png: &[u8], max_edge: u32) -> Result<Thumbnail, ThumbError> {
    let mut reader = ImageReader::new(std::io::Cursor::new(png)).with_guessed_format()?;
    reader.limits(limits());
    let img = reader.decode()?;
    if img.width() > max_edge || img.height() > max_edge {
        return Err(ThumbError::InvalidCacheEntry);
    }
    let rgba = img.into_rgba8();
    Ok(Thumbnail {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

fn encode_png(thumbnail: &Thumbnail) -> Result<Vec<u8>, ThumbError> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes).write_image(
        &thumbnail.rgba,
        thumbnail.width,
        thumbnail.height,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(bytes)
}

fn media_kind(path: &Path) -> Option<MediaKind> {
    if is_supported(path) {
        return Some(MediaKind::Image);
    }
    let extension = path.extension()?.to_str()?;
    if VIDEO_EXTENSIONS
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    {
        return Some(MediaKind::Video);
    }
    UNSUPPORTED_IMAGE_EXTENSIONS
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        .then_some(MediaKind::Other)
}

fn ffmpeg_available() -> bool {
    *FFMPEG_AVAILABLE.get_or_init(|| {
        std::process::Command::new("ffmpeg")
            .args(["-version"])
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

fn generate_video(path: &Path, max_edge: u32) -> Result<Thumbnail, ThumbError> {
    let filter = format!("scale={max_edge}:{max_edge}:force_original_aspect_ratio=decrease");
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-ss",
            "1",
            "-i",
        ])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            filter.as_str(),
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "pipe:1",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ThumbError::VideoToolUnavailable
            } else {
                ThumbError::Io(e)
            }
        })?;
    if !output.status.success() {
        return Err(ThumbError::VideoDecode);
    }
    generate_from_bytes(&output.stdout, max_edge)
}

fn record_audit(summary: &mut ThumbnailAudit, entry: &ThumbnailAuditEntry) {
    summary.scanned_media = summary.scanned_media.saturating_add(1);
    match entry.kind {
        MediaKind::Image => summary.images = summary.images.saturating_add(1),
        MediaKind::Video => summary.videos = summary.videos.saturating_add(1),
        MediaKind::Other => {}
    }
    let is_attention = !matches!(
        entry.status,
        ThumbnailStatus::Present | ThumbnailStatus::NotPersisted
    );
    match entry.status {
        ThumbnailStatus::Present => summary.present = summary.present.saturating_add(1),
        ThumbnailStatus::NotPersisted => {
            summary.not_persisted = summary.not_persisted.saturating_add(1)
        }
        ThumbnailStatus::Missing => summary.missing = summary.missing.saturating_add(1),
        ThumbnailStatus::Stale => summary.stale = summary.stale.saturating_add(1),
        ThumbnailStatus::Corrupt => summary.corrupt = summary.corrupt.saturating_add(1),
        ThumbnailStatus::Unsupported => summary.unsupported = summary.unsupported.saturating_add(1),
        ThumbnailStatus::Error => summary.errors = summary.errors.saturating_add(1),
    }
    if is_attention {
        if summary.samples.len() < MAX_AUDIT_SAMPLES {
            summary.samples.push(entry.clone());
        } else {
            summary.samples_truncated = true;
        }
    }
}

/// Default persistent thumbnail cache directory.
pub fn default_cache_dir(app: &str) -> Option<PathBuf> {
    let app_path = Path::new(app);
    if app_path.as_os_str().is_empty()
        || !app_path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".cache"))
        })?;
    Some(base.join(app_path).join("thumbnails"))
}

#[cfg(test)]
mod persistent_store_tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};
    use std::time::SystemTime;

    fn write_test_png(path: &Path) {
        let image = RgbaImage::from_pixel(48, 24, Rgba([20, 40, 60, 255]));
        image.save_with_format(path, ImageFormat::Png).unwrap();
    }

    #[test]
    fn audit_is_read_only_and_valid_thumbnail_is_reused_without_rewrite() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("sample.png");
        write_test_png(&source);
        let cache = root.path().join("cache");
        let store = ThumbnailStore::new(&cache, 64).with_policy(PersistPolicy::always());

        let audit_before = store.audit_tree(root.path());
        assert_eq!(audit_before.missing, 1);
        assert!(
            !cache.exists(),
            "read-only audit must not create its cache directory"
        );

        let (_, first) = store.ensure(&source).unwrap();
        assert!(!first.reused);
        assert_eq!(store.pending_writes(), 1);
        assert!(
            !cache.exists(),
            "nothing reaches the disk before the batched flush"
        );
        let flushed = store.flush_pending();
        assert_eq!((flushed.written, flushed.failed), (1, 0));
        assert_eq!(store.bytes_written(), flushed.bytes);
        let cache_file = store.cache_path(&source);
        let bytes_before = fs::read(&cache_file).unwrap();
        let modified_before = fs::metadata(&cache_file)
            .unwrap()
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let audit_after = store.audit_tree(root.path());
        assert_eq!(audit_after.present, 1);
        let (_, second) = store.ensure(&source).unwrap();
        assert!(
            second.reused,
            "a valid cache entry must not be generated again"
        );
        assert_eq!(fs::read(&cache_file).unwrap(), bytes_before);
        let modified_after = fs::metadata(&cache_file)
            .unwrap()
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        assert_eq!(
            modified_before, modified_after,
            "cache hit must not rewrite the existing file"
        );
    }

    #[test]
    fn default_cache_dir_rejects_paths_outside_the_application_cache_root() {
        assert!(default_cache_dir("../outside").is_none());
        assert!(default_cache_dir("/tmp/outside").is_none());
        assert!(default_cache_dir("").is_none());
    }

    #[test]
    fn unsupported_image_format_is_reported_without_generation() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("image.svg");
        fs::write(&source, b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>").unwrap();
        let cache = root.path().join("cache");
        let store = ThumbnailStore::new(&cache, 64);

        let report = store.audit_tree(root.path());
        assert_eq!(report.unsupported, 1);
        assert_eq!(report.scanned_media, 1);
        assert!(!cache.exists(), "audit must not create a cache directory");
        assert!(!ThumbnailStore::is_supported_media(&source));
    }

    #[test]
    fn corrupt_cache_is_reported_by_audit_without_repairing_it() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("sample.png");
        write_test_png(&source);
        let cache = root.path().join("cache");
        let store = ThumbnailStore::new(&cache, 64).with_policy(PersistPolicy::always());
        let _ = store.ensure(&source).unwrap();
        store.flush_pending();
        let cache_file = store.cache_path(&source);
        fs::write(&cache_file, b"corrupt").unwrap();

        let report = store.audit_tree(root.path());
        assert_eq!(report.corrupt, 1);
        assert_eq!(fs::read(cache_file).unwrap(), b"corrupt");
    }

    #[test]
    fn cheap_thumbnails_stay_in_ram_and_never_touch_the_disk() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("small.png");
        write_test_png(&source);
        let cache = root.path().join("cache");
        let store = ThumbnailStore::new(&cache, 64); // default cost-based policy

        let (_, first) = store.ensure_hinted(&source, true).unwrap();
        assert!(!first.reused);
        assert_eq!(
            store.pending_writes(),
            0,
            "a tiny PNG is cheaper to decode than to store"
        );
        let (_, second) = store.ensure_hinted(&source, true).unwrap();
        assert!(second.reused, "served from the RAM cache");
        assert_eq!(store.flush_pending(), PersistReport::default());
        assert!(!cache.exists());
        assert_eq!(store.bytes_written(), 0);
        assert_eq!(store.audit_tree(root.path()).not_persisted, 1);
    }

    #[test]
    fn a_changed_source_is_not_served_from_the_ram_cache() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("sample.png");
        write_test_png(&source);
        let store =
            ThumbnailStore::new(root.path().join("cache"), 64).with_policy(PersistPolicy::never());
        store.ensure(&source).unwrap();
        RgbaImage::from_pixel(10, 10, Rgba([1, 2, 3, 255]))
            .save_with_format(&source, ImageFormat::Png)
            .unwrap();
        let (thumbnail, report) = store.ensure(&source).unwrap();
        assert!(!report.reused);
        assert_eq!((thumbnail.width, thumbnail.height), (10, 10));
    }

    #[test]
    fn persist_policy_follows_cost_times_reuse() {
        let policy = PersistPolicy::default();
        const MIB: u64 = 1024 * 1024;
        assert!(
            !policy.should_persist(MediaKind::Image, "jpg", MIB / 2, true),
            "small jpeg"
        );
        assert!(
            policy.should_persist(MediaKind::Image, "jpg", 8 * MIB, true),
            "large jpeg in a hot folder"
        );
        assert!(
            !policy.should_persist(MediaKind::Image, "jpg", 8 * MIB, false),
            "large jpeg elsewhere"
        );
        assert!(
            policy.should_persist(MediaKind::Video, "mp4", 50 * MIB, true),
            "video in a hot folder"
        );
        assert!(
            !policy.should_persist(MediaKind::Video, "mp4", 50 * MIB, false),
            "video elsewhere"
        );
        assert!(!policy.should_persist(MediaKind::Other, "raw", 50 * MIB, true));
        assert!(PersistPolicy::always().should_persist(MediaKind::Image, "png", 10, false));
        assert!(!PersistPolicy::never().should_persist(MediaKind::Video, "mp4", 50 * MIB, true));
    }

    #[test]
    fn ram_cache_hit_queue_is_bounded() {
        let mut cache = RamCache::new(RAM_CACHE_BYTES);
        let path = PathBuf::from("unicode-文件.png");
        let fingerprint = SourceFingerprint {
            size: 12,
            modified_seconds: 1,
            modified_nanos: 2,
        };
        let thumbnail = Thumbnail {
            width: 2,
            height: 2,
            rgba: vec![1; 16],
        };
        cache.put(&path, fingerprint, &thumbnail);
        for _ in 0..500 {
            assert!(cache.get(&path, fingerprint).is_some());
        }
        assert!(cache.order.len() <= cache.map.len() * 2 + 64);
        assert_eq!(cache.bytes, ram_entry_weight(&path, &thumbnail));
    }

    #[test]
    fn pending_queue_deduplicates_repeated_paths_without_linear_queue_growth() {
        let path = PathBuf::from("unicode-文件.png");
        let fingerprint = SourceFingerprint {
            size: 1,
            modified_seconds: 1,
            modified_nanos: 0,
        };
        let mut queue = PendingQueue::default();
        for value in 0..500_u16 {
            queue.push(
                path.clone(),
                PendingWrite {
                    kind: MediaKind::Image,
                    fingerprint,
                    png: vec![value as u8],
                    sequence: 0,
                },
            );
        }
        assert_eq!(queue.map.len(), 1);
        assert_eq!(queue.bytes, pending_entry_weight(&path, 1));
        assert!(queue.order.len() <= queue.map.len() * 2 + 64);
        let items = queue.take_all();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].1.png, vec![499_u16 as u8]);
    }

    #[test]
    fn stale_pending_thumbnail_is_not_written_after_source_changes() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("sample.png");
        write_test_png(&source);
        let cache = root.path().join("cache");
        let store = ThumbnailStore::new(&cache, 64).with_policy(PersistPolicy::always());
        store.ensure(&source).unwrap();

        RgbaImage::from_pixel(7, 9, Rgba([1, 2, 3, 255]))
            .save_with_format(&source, ImageFormat::Png)
            .unwrap();
        let report = store.flush_pending();
        assert_eq!(report.written, 0);
        assert_eq!(report.failed, 1);
        assert!(!store.cache_path(&source).exists());
    }

    #[test]
    fn persistent_cache_pruning_respects_a_small_test_budget() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("first.fmt"), b"123456").unwrap();
        fs::write(cache.join("second.fmt"), b"abcde").unwrap();
        fs::write(cache.join("keep.txt"), b"not a cache entry").unwrap();
        let store = ThumbnailStore::new(&cache, 64);

        store.prune_cache_to(5);

        let total: u64 = fs::read_dir(&cache)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("fmt"))
            .filter_map(|entry| entry.metadata().ok())
            .map(|metadata| metadata.len())
            .sum();
        assert!(total <= 5);
        assert!(cache.join("keep.txt").exists());
    }
}
