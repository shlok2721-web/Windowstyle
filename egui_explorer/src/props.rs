//! Details shown by the Properties window.
//!
//! Folder statistics (total size, item counts, ...) need a recursive walk, which can take a
//! long time on big trees. The walk therefore runs on a worker thread that publishes running
//! totals; the UI polls a [`Snapshot`] every frame and shows live numbers until the scan is
//! done. Only `std` is used and symbolic links are never followed.

use std::fs::{self, Metadata};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use egui::{Context, Id};

use crate::icons::Icon;
use crate::model::{Entry, EntryKind};

/// How often the worker publishes running totals to the UI.
const PUBLISH_EVERY: Duration = Duration::from_millis(80);

// ---------------------------------------------------------------------------
// Scan data
// ---------------------------------------------------------------------------

/// Totals of everything *inside* the scanned targets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub files: u64,
    pub folders: u64,
    pub symlinks: u64,
    pub hidden: u64,
    /// Sum of file lengths.
    pub bytes: u64,
    /// Space actually allocated on disk (block granularity on Unix).
    pub disk_bytes: u64,
    /// Entries that could not be read (permissions, races, ...).
    pub unreadable: u64,
    pub largest: Option<(String, u64)>,
    /// Most recent modification time of any file found.
    pub newest: Option<SystemTime>,
}

/// Low-level metadata of a single selected item.
#[derive(Clone, Debug, Default)]
pub struct FileMeta {
    pub created: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub readonly: bool,
    pub is_symlink: bool,
    pub link_target: Option<PathBuf>,
    pub len: u64,
    pub disk_bytes: u64,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub links: Option<u64>,
    pub inode: Option<u64>,
}

/// Copy of the worker's current state.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub stats: Stats,
    pub meta: Option<FileMeta>,
    pub done: bool,
}

#[derive(Default)]
struct Shared {
    stats: Stats,
    meta: Option<FileMeta>,
}

/// A running (or finished) scan. Shared between the UI and the worker thread.
#[derive(Default)]
pub struct Scan {
    shared: Mutex<Shared>,
    done: AtomicBool,
    cancel: AtomicBool,
}

impl Scan {
    /// Start scanning `targets` on a background thread.
    pub fn spawn(targets: Vec<PathBuf>) -> Arc<Scan> {
        let scan = Arc::new(Scan::default());
        let worker = Arc::clone(&scan);
        let started = std::thread::Builder::new()
            .name("fm-props-scan".into())
            .spawn(move || {
                worker.run(&targets);
                worker.done.store(true, Ordering::Release);
            });
        if started.is_err() {
            // No thread available: report an empty, finished scan instead of spinning forever.
            scan.done.store(true, Ordering::Release);
        }
        scan
    }

    /// Ask the worker to stop as soon as possible.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        let done = self.done.load(Ordering::Acquire);
        let shared = self.lock();
        Snapshot {
            stats: shared.stats.clone(),
            meta: shared.meta.clone(),
            done,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn publish(&self, stats: &Stats) {
        self.lock().stats = stats.clone();
    }

    fn run(&self, targets: &[PathBuf]) {
        if let [only] = targets
            && let Ok(md) = fs::symlink_metadata(only)
        {
            self.lock().meta = Some(file_meta(only, &md));
        }

        let mut stats = Stats::default();
        let mut stack: Vec<PathBuf> = Vec::new();
        let mut last_publish = Instant::now();

        for target in targets {
            if self.cancelled() {
                return;
            }
            let Ok(md) = fs::symlink_metadata(target) else {
                stats.unreadable += 1;
                continue;
            };
            if md.is_dir() {
                stats.disk_bytes += disk_usage(&md);
                stack.push(target.clone());
            } else {
                let name = target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                count_leaf(&mut stats, &md, &name);
            }
        }

        while let Some(dir) = stack.pop() {
            if self.cancelled() {
                return;
            }
            let Ok(read) = fs::read_dir(&dir) else {
                stats.unreadable += 1;
                continue;
            };
            for item in read {
                let Ok(item) = item else {
                    stats.unreadable += 1;
                    continue;
                };
                // `DirEntry::metadata` does not follow symbolic links.
                let Ok(md) = item.metadata() else {
                    stats.unreadable += 1;
                    continue;
                };
                let name = item.file_name().to_string_lossy().into_owned();
                if md.is_dir() {
                    stats.folders += 1;
                    stats.disk_bytes += disk_usage(&md);
                    if name.starts_with('.') {
                        stats.hidden += 1;
                    }
                    stack.push(item.path());
                } else {
                    count_leaf(&mut stats, &md, &name);
                }
            }
            if last_publish.elapsed() >= PUBLISH_EVERY {
                self.publish(&stats);
                last_publish = Instant::now();
            }
        }
        self.publish(&stats);
    }
}

/// Account for a non-directory item.
fn count_leaf(stats: &mut Stats, md: &Metadata, name: &str) {
    if name.starts_with('.') {
        stats.hidden += 1;
    }
    if md.file_type().is_symlink() {
        stats.symlinks += 1;
        return;
    }
    let len = md.len();
    stats.files += 1;
    stats.bytes += len;
    stats.disk_bytes += disk_usage(md);
    if stats.largest.as_ref().is_none_or(|(_, l)| len > *l) {
        stats.largest = Some((name.to_owned(), len));
    }
    if let Ok(t) = md.modified()
        && stats.newest.is_none_or(|n| t > n)
    {
        stats.newest = Some(t);
    }
}

#[cfg(unix)]
fn disk_usage(md: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn disk_usage(md: &Metadata) -> u64 {
    md.len().div_ceil(4096).saturating_mul(4096)
}

fn file_meta(path: &Path, md: &Metadata) -> FileMeta {
    let is_symlink = md.file_type().is_symlink();
    #[allow(unused_mut)]
    let mut meta = FileMeta {
        created: md.created().ok(),
        accessed: md.accessed().ok(),
        modified: md.modified().ok(),
        readonly: md.permissions().readonly(),
        is_symlink,
        link_target: if is_symlink {
            fs::read_link(path).ok()
        } else {
            None
        },
        len: md.len(),
        disk_bytes: disk_usage(md),
        ..FileMeta::default()
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mode = Some(md.mode());
        meta.uid = Some(md.uid());
        meta.gid = Some(md.gid());
        meta.links = Some(md.nlink());
        meta.inode = Some(md.ino());
    }
    meta
}

// ---------------------------------------------------------------------------
// Scan lifetime (kept in egui's temp memory so the model needs no new fields)
// ---------------------------------------------------------------------------

type Slot = (u64, Option<Arc<Scan>>);

fn slot_id() -> Id {
    Id::new("exp_props_scan")
}

/// Hash that identifies a set of scan targets.
pub fn key_of<'a>(paths: impl Iterator<Item = &'a Path>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in paths {
        p.hash(&mut h);
    }
    h.finish()
}

/// The scan for `key`: reuses the running one or cancels it and starts a new one.
pub fn scan_for(ctx: &Context, key: u64, targets: impl FnOnce() -> Vec<PathBuf>) -> Arc<Scan> {
    ctx.data_mut(|d| {
        let slot: &mut Slot = d.get_temp_mut_or_default(slot_id());
        if let Some(scan) = &slot.1
            && slot.0 == key
        {
            return Arc::clone(scan);
        }
        if let Some(old) = slot.1.take() {
            old.cancel();
        }
        let scan = Scan::spawn(targets());
        *slot = (key, Some(Arc::clone(&scan)));
        scan
    })
}

/// Stop and forget the current scan (the Properties window was closed).
pub fn release(ctx: &Context) {
    ctx.data_mut(|d| {
        let slot: &mut Slot = d.get_temp_mut_or_default(slot_id());
        if let Some(old) = slot.1.take() {
            old.cancel();
        }
        slot.0 = 0;
    });
}

// ---------------------------------------------------------------------------
// Presentation
// ---------------------------------------------------------------------------

/// One line of the Properties window.
#[derive(Clone, Debug)]
pub enum Row {
    Section(&'static str),
    Item(&'static str, String),
}

/// Everything the Properties window needs to draw itself.
pub struct View {
    pub icon: Icon,
    pub title: String,
    pub subtitle: String,
    pub rows: Vec<Row>,
}

/// Group digits: `1234567` -> `1,234,567`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1.23 MB` style size with three significant digits.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    let unit = UNITS[u];
    if v >= 100.0 {
        format!("{v:.0} {unit}")
    } else if v >= 10.0 {
        format!("{v:.1} {unit}")
    } else {
        format!("{v:.2} {unit}")
    }
}

/// `1.23 MB (1,289,013 bytes)`.
pub fn size_line(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else {
        format!("{} ({} bytes)", human_size(bytes), group_digits(bytes))
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", group_digits(n), if n == 1 { one } else { many })
}

fn contents_line(s: &Stats) -> String {
    format!(
        "{}, {}",
        plural(s.files, "file", "files"),
        plural(s.folders, "folder", "folders")
    )
}

/// `-rw-r--r-- (644)` (pure bit maths, so it needs no platform gate).
fn mode_line(mode: u32, is_dir: bool, is_link: bool) -> String {
    let kind = if is_link {
        'l'
    } else if is_dir {
        'd'
    } else {
        '-'
    };
    let mut s = String::with_capacity(16);
    s.push(kind);
    for shift in [6u32, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        s.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        s.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        s.push(if bits & 0o1 != 0 { 'x' } else { '-' });
    }
    format!("{s} ({:03o})", mode & 0o777)
}

fn yes_no(v: bool) -> String {
    String::from(if v { "Yes" } else { "No" })
}

fn push(rows: &mut Vec<Row>, label: &'static str, value: impl Into<String>) {
    rows.push(Row::Item(label, value.into()));
}

fn push_time(
    rows: &mut Vec<Row>,
    label: &'static str,
    t: Option<SystemTime>,
    fmt: fn(SystemTime) -> String,
) {
    if let Some(t) = t {
        push(rows, label, fmt(t));
    }
}

/// Shared "size + contents" rows for folders and multi-selections.
fn tree_rows(rows: &mut Vec<Row>, snap: &Snapshot, fmt: fn(SystemTime) -> String) {
    let s = &snap.stats;
    let live = |text: String| {
        if snap.done {
            text
        } else {
            format!("{text} (calculating...)")
        }
    };
    push(rows, "Size", live(size_line(s.bytes)));
    push(rows, "Size on disk", live(human_size(s.disk_bytes)));
    push(rows, "Contains", live(contents_line(s)));
    if s.hidden > 0 {
        push(rows, "Hidden items", group_digits(s.hidden));
    }
    if s.symlinks > 0 {
        push(rows, "Symbolic links", group_digits(s.symlinks));
    }
    if let Some((name, len)) = &s.largest {
        push(rows, "Largest file", format!("{name} ({})", human_size(*len)));
    }
    push_time(rows, "Latest change", s.newest, fmt);
    if snap.done && s.unreadable > 0 {
        push(
            rows,
            "Skipped",
            format!("{} could not be read", plural(s.unreadable, "item", "items")),
        );
    }
}

fn single_rows(
    rows: &mut Vec<Row>,
    e: &Entry,
    snap: &Snapshot,
    fmt: fn(SystemTime) -> String,
) {
    push(rows, "Type", e.type_name.clone());
    let parent = e
        .path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    push(rows, "Location", parent);
    let meta = snap.meta.as_ref();

    match e.kind {
        EntryKind::Drive => {
            push(rows, "Mount point", e.path.display().to_string());
            if let Some((total, free)) = e.space {
                let used = total.saturating_sub(free);
                push(rows, "Capacity", size_line(total));
                push(rows, "Used space", size_line(used));
                push(rows, "Free space", size_line(free));
                if total > 0 {
                    push(
                        rows,
                        "Usage",
                        format!("{:.0}% used", used as f64 * 100.0 / total as f64),
                    );
                }
            }
        }
        EntryKind::Folder => tree_rows(rows, snap, fmt),
        EntryKind::File => {
            push(rows, "Size", size_line(meta.map_or(e.size, |m| m.len)));
            if let Some(m) = meta {
                push(rows, "Size on disk", human_size(m.disk_bytes));
            }
        }
    }

    rows.push(Row::Section("Dates"));
    push_time(rows, "Created", meta.and_then(|m| m.created), fmt);
    push_time(
        rows,
        "Modified",
        e.modified.or_else(|| meta.and_then(|m| m.modified)),
        fmt,
    );
    push_time(rows, "Accessed", meta.and_then(|m| m.accessed), fmt);

    rows.push(Row::Section("Attributes"));
    push(rows, "Hidden", yes_no(e.hidden));
    if let Some(m) = meta {
        push(rows, "Read-only", yes_no(m.readonly));
        if let Some(mode) = m.mode {
            push(
                rows,
                "Permissions",
                mode_line(mode, e.is_dir(), m.is_symlink),
            );
        }
        if let (Some(uid), Some(gid)) = (m.uid, m.gid) {
            push(rows, "Owner", format!("uid {uid}, gid {gid}"));
        }
        if let Some(links) = m.links {
            push(rows, "Hard links", group_digits(links));
        }
        if let Some(inode) = m.inode {
            push(rows, "Inode", group_digits(inode));
        }
        if let Some(target) = &m.link_target {
            push(rows, "Link target", target.display().to_string());
        }
    }
}

fn many_rows(
    rows: &mut Vec<Row>,
    sel: &[&Entry],
    location: &Path,
    snap: &Snapshot,
    fmt: fn(SystemTime) -> String,
) {
    let dirs = sel.iter().filter(|e| e.is_dir()).count() as u64;
    let files = sel.len() as u64 - dirs;
    push(
        rows,
        "Selected",
        format!(
            "{} ({}, {})",
            plural(sel.len() as u64, "item", "items"),
            plural(files, "file", "files"),
            plural(dirs, "folder", "folders")
        ),
    );
    let parent = sel[0].path.parent();
    let same = sel.iter().all(|e| e.path.parent() == parent);
    let loc = if same {
        parent.unwrap_or(location).display().to_string()
    } else {
        "Various locations".to_owned()
    };
    push(rows, "Location", loc);
    tree_rows(rows, snap, fmt);
}

/// Build everything the window shows for the current selection (or the current folder when
/// nothing is selected).
pub fn build_view(
    sel: &[&Entry],
    location: &Path,
    entry_count: usize,
    snap: &Snapshot,
    fmt: fn(SystemTime) -> String,
) -> View {
    let mut rows = Vec::new();
    match sel {
        [] => {
            let name = location
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| location.display().to_string());
            push(&mut rows, "Type", "File folder");
            push(&mut rows, "Location", location.display().to_string());
            push(
                &mut rows,
                "Items",
                plural(entry_count as u64, "item", "items"),
            );
            tree_rows(&mut rows, snap, fmt);
            if let Some(m) = &snap.meta {
                rows.push(Row::Section("Dates"));
                push_time(&mut rows, "Created", m.created, fmt);
                push_time(&mut rows, "Modified", m.modified, fmt);
                push_time(&mut rows, "Accessed", m.accessed, fmt);
                if let Some(mode) = m.mode {
                    rows.push(Row::Section("Attributes"));
                    push(&mut rows, "Permissions", mode_line(mode, true, false));
                }
            }
            View {
                icon: Icon::Folder,
                title: name,
                subtitle: "File folder".to_owned(),
                rows,
            }
        }
        [e] => {
            single_rows(&mut rows, e, snap, fmt);
            View {
                icon: e.icon(),
                title: e.name.clone(),
                subtitle: e.type_name.clone(),
                rows,
            }
        }
        many => {
            many_rows(&mut rows, many, location, snap, fmt);
            View {
                icon: Icon::Folder,
                title: plural(many.len() as u64, "item", "items"),
                subtitle: "Multiple selection".to_owned(),
                rows,
            }
        }
    }
}
