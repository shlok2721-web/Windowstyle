//! Recursive directory traversal, search and batch helpers. **No egui dependency.**
//!
//! Built on [`walkdir`]. Unreadable entries (permission denied, vanished files) are
//! skipped and counted rather than aborting the walk.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

use walkdir::{DirEntry, WalkDir};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryType {
    File,
    Dir,
    Symlink,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkEntry {
    pub path: PathBuf,
    pub depth: usize,
    pub kind: EntryType,
    /// Size in bytes (0 for directories).
    pub size: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct WalkOptions {
    /// `None` = unlimited. Depth 0 is the root itself, 1 its children.
    pub max_depth: Option<usize>,
    pub follow_links: bool,
    /// When false, dotfiles and dot-directories (and everything under them) are skipped.
    pub include_hidden: bool,
    /// Include the root directory itself in the results.
    pub include_root: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_depth: None,
            follow_links: false,
            include_hidden: false,
            include_root: false,
        }
    }
}

fn is_hidden(e: &DirEntry) -> bool {
    e.depth() > 0 && e.file_name().to_str().is_some_and(|n| n.starts_with('.'))
}

fn to_entry(e: &DirEntry) -> Option<WalkEntry> {
    let md = e.metadata().ok()?;
    let ft = e.file_type();
    let kind = if ft.is_symlink() {
        EntryType::Symlink
    } else if ft.is_dir() {
        EntryType::Dir
    } else {
        EntryType::File
    };
    Some(WalkEntry {
        path: e.path().to_owned(),
        depth: e.depth(),
        kind,
        size: if md.is_dir() { 0 } else { md.len() },
        modified: md.modified().ok(),
    })
}

/// Lazy iterator over entries below `root`. Errors are skipped; see [`Walk::skipped`].
pub struct Walk {
    inner: Box<dyn Iterator<Item = walkdir::Result<DirEntry>> + Send>,
    include_root: bool,
    skipped: usize,
}

impl Walk {
    /// Number of entries that could not be read so far.
    pub fn skipped(&self) -> usize {
        self.skipped
    }
}

impl Iterator for Walk {
    type Item = WalkEntry;
    fn next(&mut self) -> Option<WalkEntry> {
        loop {
            match self.inner.next()? {
                Ok(e) => {
                    if e.depth() == 0 && !self.include_root {
                        continue;
                    }
                    match to_entry(&e) {
                        Some(w) => return Some(w),
                        None => self.skipped += 1,
                    }
                }
                Err(_) => self.skipped += 1,
            }
        }
    }
}

pub fn walk(root: impl AsRef<Path>, opts: &WalkOptions) -> Walk {
    walk_inner(root.as_ref(), opts, None)
}

fn walk_inner(root: &Path, opts: &WalkOptions, excluded_root: Option<PathBuf>) -> Walk {
    let mut w = WalkDir::new(root)
        .follow_links(opts.follow_links)
        .sort_by_file_name();
    if let Some(d) = opts.max_depth {
        w = w.max_depth(d);
    }
    let hidden = opts.include_hidden;
    let inner = w.into_iter().filter_entry(move |entry| {
        if excluded_root
            .as_ref()
            .is_some_and(|excluded| entry.path().starts_with(excluded))
        {
            return false;
        }
        hidden || !is_hidden(entry)
    });
    Walk {
        inner: Box::new(inner),
        include_root: opts.include_root,
        skipped: 0,
    }
}

// ---------------------------------------------------------------------------
// Name matching
// ---------------------------------------------------------------------------

/// Case-insensitive wildcard match: `*` = any run, `?` = any one char.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[derive(Clone, Debug)]
pub struct SearchOptions {
    /// Text to look for. Contains `*`/`?` => wildcard over the whole file name;
    /// otherwise a case-insensitive substring match.
    pub query: String,
    pub walk: WalkOptions,
    pub files: bool,
    pub dirs: bool,
    /// Stop after this many matches.
    pub max_results: Option<usize>,
}

impl SearchOptions {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            walk: WalkOptions::default(),
            files: true,
            dirs: true,
            max_results: None,
        }
    }

    pub fn matches_name(&self, name: &str) -> bool {
        if self.query.is_empty() {
            return true;
        }
        if self.query.contains(['*', '?']) {
            wildcard_match(&self.query, name)
        } else {
            name.to_lowercase().contains(&self.query.to_lowercase())
        }
    }

    fn wants(&self, e: &WalkEntry) -> bool {
        let type_ok = match e.kind {
            EntryType::Dir => self.dirs,
            _ => self.files,
        };
        type_ok
            && e.path
                .file_name()
                .map(|n| self.matches_name(&n.to_string_lossy()))
                .unwrap_or(false)
    }
}

/// Cooperative cancellation flag; clone it freely.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Blocking search; calls `on_match` for each hit. Returns the number of hits.
pub fn search(
    root: &Path,
    opts: &SearchOptions,
    cancel: &CancelToken,
    mut on_match: impl FnMut(WalkEntry),
) -> usize {
    let mut found = 0;
    for e in walk(root, &opts.walk) {
        if cancel.is_cancelled() {
            break;
        }
        if opts.wants(&e) {
            on_match(e);
            found += 1;
            if opts.max_results.is_some_and(|m| found >= m) {
                break;
            }
        }
    }
    found
}

pub enum SearchMsg {
    Hit(WalkEntry),
    /// Always the final message, sent even after cancellation.
    Done {
        found: usize,
        cancelled: bool,
    },
}

/// Handle to a background search. Poll `rx` each frame with `try_recv`.
pub struct SearchHandle {
    pub rx: Receiver<SearchMsg>,
    pub cancel: CancelToken,
    thread: Option<JoinHandle<()>>,
}

impl SearchHandle {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for SearchHandle {
    fn drop(&mut self) {
        self.cancel.cancel(); // dropping the handle stops the worker
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Run [`search`] on a worker thread. Dropping the handle cancels it.
pub fn spawn_search(root: PathBuf, opts: SearchOptions) -> SearchHandle {
    let (tx, rx) = mpsc::channel();
    let cancel = CancelToken::new();
    let c = cancel.clone();
    let thread = thread::spawn(move || {
        let found = search(&root, &opts, &c, |e| {
            let _ = tx.send(SearchMsg::Hit(e));
        });
        let _ = tx.send(SearchMsg::Done {
            found,
            cancelled: c.is_cancelled(),
        });
    });
    SearchHandle {
        rx,
        cancel,
        thread: Some(thread),
    }
}

// ---------------------------------------------------------------------------
// Batch helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub skipped: u64,
}

/// Total size / counts of everything under `path` (hidden items included), as the
/// Properties dialog or a delete/copy confirmation needs. A plain file counts as 1 file.
pub fn summarize(path: &Path, cancel: &CancelToken) -> Summary {
    let mut s = Summary::default();
    let opts = WalkOptions {
        include_hidden: true,
        include_root: true,
        ..Default::default()
    };
    let mut it = walk(path, &opts);
    for e in it.by_ref() {
        if cancel.is_cancelled() {
            break;
        }
        match e.kind {
            EntryType::Dir => s.dirs += 1,
            _ => {
                s.files += 1;
                s.bytes += e.size;
            }
        }
    }
    s.skipped = it.skipped() as u64;
    s
}

/// Flatten `paths` (files and folders) into the list of files below them, e.g. as
/// the work list for a batch copy/delete/zip. Folders themselves are returned
/// after their contents (children first), which is the order deletion needs.
pub fn expand_for_batch(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        let is_dir = std::fs::symlink_metadata(p)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if is_dir {
            let w = WalkDir::new(p).contents_first(true).follow_links(false);
            out.extend(w.into_iter().filter_map(Result::ok).map(|e| e.into_path()));
        } else {
            out.push(p.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join("a/b")).unwrap();
        fs::create_dir_all(r.join(".git/objects")).unwrap();
        fs::write(r.join("top.txt"), "12345").unwrap();
        fs::write(r.join("a/Report.TXT"), "123").unwrap();
        fs::write(r.join("a/b/deep.rs"), "1").unwrap();
        fs::write(r.join(".hidden"), "1").unwrap();
        fs::write(r.join(".git/objects/x"), "1").unwrap();
        d
    }

    fn names(it: impl Iterator<Item = WalkEntry>) -> Vec<String> {
        let mut v: Vec<_> = it
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn hidden_skipped_by_default_with_subtrees() {
        let d = tree();
        let n = names(walk(d.path(), &WalkOptions::default()));
        assert_eq!(n, ["Report.TXT", "a", "b", "deep.rs", "top.txt"]);
        let all = names(walk(
            d.path(),
            &WalkOptions {
                include_hidden: true,
                ..Default::default()
            },
        ));
        assert!(all.contains(&".git".to_owned()) && all.contains(&"x".to_owned()));
    }

    #[test]
    fn depth_limit_and_root() {
        let d = tree();
        let o = WalkOptions {
            max_depth: Some(1),
            ..Default::default()
        };
        assert_eq!(names(walk(d.path(), &o)), ["a", "top.txt"]);
        let o = WalkOptions {
            max_depth: Some(0),
            include_root: true,
            ..Default::default()
        };
        assert_eq!(walk(d.path(), &o).count(), 1);
    }

    #[test]
    fn wildcard() {
        assert!(wildcard_match("*.txt", "Report.TXT"));
        assert!(wildcard_match("r?port*", "Report.TXT"));
        assert!(wildcard_match("*", ""));
        assert!(!wildcard_match("*.rs", "a.txt"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("a*b*c", "aXXbYY"));
    }

    #[test]
    fn search_substring_wildcard_and_filters() {
        let d = tree();
        let c = CancelToken::new();
        let mut hits = vec![];
        search(d.path(), &SearchOptions::new("report"), &c, |e| {
            hits.push(e)
        });
        assert_eq!(names(hits.into_iter()), ["Report.TXT"]);

        let mut o = SearchOptions::new("*.txt");
        o.dirs = false;
        let mut hits = vec![];
        let n = search(d.path(), &o, &c, |e| hits.push(e));
        assert_eq!(
            (n, names(hits.into_iter())),
            (2, vec!["Report.TXT".into(), "top.txt".into()])
        );

        let mut o = SearchOptions::new("");
        o.max_results = Some(2);
        assert_eq!(search(d.path(), &o, &c, |_| {}), 2);
    }

    #[test]
    fn background_search_and_cancel() {
        let d = tree();
        let h = spawn_search(d.path().to_owned(), SearchOptions::new("deep"));
        let mut hits = 0;
        loop {
            match h.rx.recv().unwrap() {
                SearchMsg::Hit(_) => hits += 1,
                SearchMsg::Done { found, cancelled } => {
                    assert_eq!((found, cancelled), (1, false));
                    break;
                }
            }
        }
        assert_eq!(hits, 1);
        drop(h); // must not hang

        let c = CancelToken::new();
        c.cancel();
        assert_eq!(search(d.path(), &SearchOptions::new(""), &c, |_| {}), 0);
    }

    #[test]
    fn summary_counts_hidden_too() {
        let d = tree();
        let s = summarize(d.path(), &CancelToken::new());
        assert_eq!((s.files, s.bytes), (5, 5 + 3 + 1 + 1 + 1));
        assert_eq!(s.dirs, 5); // root, a, a/b, .git, .git/objects
        assert_eq!(s.skipped, 0);
        let one = summarize(&d.path().join("top.txt"), &CancelToken::new());
        assert_eq!((one.files, one.bytes), (1, 5));
    }

    #[test]
    fn batch_expansion_is_children_first() {
        let d = tree();
        let v = expand_for_batch(&[d.path().join("a"), d.path().join("top.txt")]);
        let pos = |s: &str| v.iter().position(|p| p.ends_with(s)).unwrap();
        assert!(pos("a/b/deep.rs") < pos("a/b"));
        assert!(
            pos("a/b")
                < v.iter()
                    .position(|p| p.file_name().unwrap() == "a")
                    .unwrap()
        );
        assert!(v.last().unwrap().ends_with("top.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loops_do_not_hang_when_following() {
        let d = tree();
        std::os::unix::fs::symlink(d.path(), d.path().join("a/loop")).unwrap();
        let o = WalkOptions {
            follow_links: true,
            ..Default::default()
        };
        let mut it = walk(d.path(), &o);
        let n = it.by_ref().count();
        assert!(n > 0 && it.skipped() >= 1); // loop reported as an error and skipped
    }
}
