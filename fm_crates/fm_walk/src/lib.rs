//! Recursive directory traversal, search and batch helpers. **No egui dependency.**
//!
//! Built on [`walkdir`]. Unreadable entries (permission denied, vanished files) are
//! skipped and counted rather than aborting the walk.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

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

#[derive(Clone, Debug, Default)]
pub struct WalkOptions {
    /// `None` = unlimited. Depth 0 is the root itself, 1 its children.
    pub max_depth: Option<usize>,
    pub follow_links: bool,
    /// When false, dotfiles and dot-directories (and everything under them) are skipped.
    pub include_hidden: bool,
    /// Include the root directory itself in the results.
    pub include_root: bool,
}

fn is_hidden(e: &DirEntry) -> bool {
    if e.depth() == 0 {
        return false;
    }

    // Most names take the allocation-free UTF-8 path. Lossy conversion is only needed
    // for unusual non-UTF-8 names so leading-dot entries are still hidden on Unix.
    e.file_name().to_str().map_or_else(
        || e.file_name().to_string_lossy().starts_with('.'),
        |n| n.starts_with('.'),
    )
}

fn to_entry(e: &DirEntry) -> Option<WalkEntry> {
    // WalkDir's metadata method already follows its configured `follow_links` policy,
    // avoiding an extra filesystem query for symbolic links.
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
    inner: walkdir::IntoIter,
    include_root: bool,
    include_hidden: bool,
    max_depth: Option<usize>,
    excluded_root: Option<PathBuf>,
    cancel: Option<CancelToken>,
    skipped: usize,
}

impl Walk {
    /// Number of entries that could not be read so far.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    fn skip_children_of(&mut self, entry: &DirEntry) {
        // At the depth limit WalkDir may not have pushed this directory onto its traversal
        // stack. Calling `skip_current_dir` there could otherwise pop the parent directory.
        if entry.file_type().is_dir() && self.max_depth.is_none_or(|limit| entry.depth() < limit) {
            self.inner.skip_current_dir();
        }
    }
}

impl Iterator for Walk {
    type Item = WalkEntry;
    fn next(&mut self) -> Option<WalkEntry> {
        loop {
            if self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
                return None;
            }
            match self.inner.next()? {
                Ok(e) => {
                    // Prune excluded trees directly on WalkDir's iterator. Besides avoiding
                    // hidden-subtree visits, this keeps cancellation checks responsive even
                    // when a directory contains a large number of filtered entries.
                    if self
                        .excluded_root
                        .as_ref()
                        .is_some_and(|excluded| e.path().starts_with(excluded))
                    {
                        self.skip_children_of(&e);
                        continue;
                    }
                    if !self.include_hidden && is_hidden(&e) {
                        self.skip_children_of(&e);
                        continue;
                    }
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
    walk_inner(root.as_ref(), opts, None, None)
}

fn walk_cancellable(root: &Path, opts: &WalkOptions, cancel: &CancelToken) -> Walk {
    walk_inner(root, opts, None, Some(cancel.clone()))
}

fn walk_inner(
    root: &Path,
    opts: &WalkOptions,
    excluded_root: Option<PathBuf>,
    cancel: Option<CancelToken>,
) -> Walk {
    let mut w = WalkDir::new(root)
        .follow_links(opts.follow_links)
        .sort_by_file_name();
    if let Some(d) = opts.max_depth {
        w = w.max_depth(d);
    }
    Walk {
        inner: w.into_iter(),
        include_root: opts.include_root,
        include_hidden: opts.include_hidden,
        max_depth: opts.max_depth,
        excluded_root,
        cancel,
        skipped: 0,
    }
}

// ---------------------------------------------------------------------------
// Name matching
// ---------------------------------------------------------------------------

/// Case-insensitive wildcard match: `*` = any run, `?` = any one char.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    if pattern.is_ascii() && text.is_ascii() {
        return wildcard_match_ascii(pattern.as_bytes(), text.as_bytes());
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    wildcard_match_lowercase(&p, &t)
}

/// ASCII wildcard matching avoids allocating lowercase strings and character vectors for the
/// overwhelmingly common ASCII filenames. Both wildcard operators are single-byte in this path.
fn wildcard_match_ascii(pattern: &[u8], text: &[u8]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < text.len() {
        if pi < pattern.len()
            && (pattern[pi] == b'?' || pattern[pi].eq_ignore_ascii_case(&text[ti]))
        {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
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
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Match already-lowercased character buffers. Recursive searches compile the pattern once,
/// avoiding repeated pattern allocations for every directory entry.
fn wildcard_match_lowercase(pattern: &[char], text: &[char]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
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
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
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
        NameMatcher::new(&self.query).matches(name)
    }

    fn wants(&self, e: &WalkEntry, matcher: &NameMatcher) -> bool {
        let type_ok = match e.kind {
            EntryType::Dir => self.dirs,
            _ => self.files,
        };
        type_ok
            && e.path
                .file_name()
                .map(|n| matcher.matches(&n.to_string_lossy()))
                .unwrap_or(false)
    }
}

/// Query compiled once per recursive search instead of lowercasing the query and rebuilding
/// its wildcard character vector for every entry. Matching remains Unicode case-insensitive.
struct NameMatcher {
    lowercase_query: String,
    wildcard_pattern: Option<Vec<char>>,
}

impl NameMatcher {
    fn new(query: &str) -> Self {
        let lowercase_query = query.to_lowercase();
        let wildcard_pattern: Option<Vec<char>> = query
            .contains(['*', '?'])
            .then(|| lowercase_query.chars().collect());
        Self {
            lowercase_query,
            wildcard_pattern,
        }
    }

    fn matches(&self, name: &str) -> bool {
        if self.lowercase_query.is_empty() {
            return true;
        }

        if name.is_ascii() && self.lowercase_query.is_ascii() {
            if self.wildcard_pattern.is_some() {
                return wildcard_match_ascii(self.lowercase_query.as_bytes(), name.as_bytes());
            }
            let query = self.lowercase_query.as_bytes();
            return name
                .as_bytes()
                .windows(query.len())
                .any(|part| part.eq_ignore_ascii_case(query));
        }

        if let Some(pattern) = &self.wildcard_pattern {
            let lowercase_name = name.to_lowercase();
            let text: Vec<char> = lowercase_name.chars().collect();
            wildcard_match_lowercase(pattern, &text)
        } else {
            name.to_lowercase().contains(&self.lowercase_query)
        }
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
    search_with_delivery(root, opts, cancel, |entry| {
        on_match(entry);
        true
    })
}

/// Internal search variant whose delivery callback can stop traversal when a bounded consumer
/// has cancelled or disconnected. The public callback API remains unchanged.
fn search_with_delivery(
    root: &Path,
    opts: &SearchOptions,
    cancel: &CancelToken,
    mut on_match: impl FnMut(WalkEntry) -> bool,
) -> usize {
    if cancel.is_cancelled() || opts.max_results == Some(0) {
        return 0;
    }

    let matcher = NameMatcher::new(&opts.query);
    let mut found = 0;
    for e in walk_cancellable(root, &opts.walk, cancel) {
        if cancel.is_cancelled() {
            break;
        }
        if opts.wants(&e, &matcher) {
            if !on_match(e) {
                break;
            }
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
            // The worker uses a bounded channel. Drain it while joining so it can always
            // finish its terminal message instead of deadlocking on a full queue.
            while !t.is_finished() {
                while self.rx.try_recv().is_ok() {}
                thread::sleep(Duration::from_millis(1));
            }
            while self.rx.try_recv().is_ok() {}
            let _ = t.join();
        }
    }
}

const SEARCH_QUEUE_CAPACITY: usize = 128;

/// Send a hit with backpressure, checking cancellation while waiting for queue capacity.
fn send_hit(tx: &SyncSender<SearchMsg>, mut msg: SearchMsg, cancel: &CancelToken) -> bool {
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        match tx.try_send(msg) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                msg = returned;
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// Send the terminal message even if cancellation was requested; `Drop` drains the bounded
/// receiver while joining, and a disconnected receiver ends this loop immediately.
fn send_done(tx: &SyncSender<SearchMsg>, mut msg: SearchMsg) {
    loop {
        match tx.try_send(msg) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(returned)) => {
                msg = returned;
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

/// Run [`search`] on a worker thread. Dropping the handle cancels it.
pub fn spawn_search(root: PathBuf, opts: SearchOptions) -> SearchHandle {
    let (tx, rx) = mpsc::sync_channel(SEARCH_QUEUE_CAPACITY);
    let cancel = CancelToken::new();
    let c = cancel.clone();
    let thread = thread::spawn(move || {
        let found =
            search_with_delivery(&root, &opts, &c, |e| send_hit(&tx, SearchMsg::Hit(e), &c));
        send_done(
            &tx,
            SearchMsg::Done {
                found,
                cancelled: c.is_cancelled(),
            },
        );
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
    let mut it = walk_cancellable(path, &opts, cancel);
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

/// Error returned by [`try_expand_for_batch`] when the complete work list cannot be built.
#[derive(Debug)]
pub enum BatchExpandError {
    Metadata {
        path: PathBuf,
        source: std::io::Error,
    },
    Walk(walkdir::Error),
}

impl std::fmt::Display for BatchExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Metadata { path, source } => {
                write!(f, "could not inspect {}: {source}", path.display())
            }
            Self::Walk(source) => write!(f, "directory traversal failed: {source}"),
        }
    }
}

impl std::error::Error for BatchExpandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Metadata { source, .. } => Some(source),
            Self::Walk(source) => Some(source),
        }
    }
}

/// Build a complete children-first batch work list or return the first traversal/metadata error.
/// This is the safer API for operations where silently omitting descendants could leave a
/// partially processed directory. Symbolic links are not followed.
pub fn try_expand_for_batch(paths: &[PathBuf]) -> Result<Vec<PathBuf>, BatchExpandError> {
    let mut out = Vec::new();
    for p in paths {
        let metadata =
            std::fs::symlink_metadata(p).map_err(|source| BatchExpandError::Metadata {
                path: p.clone(),
                source,
            })?;
        if metadata.is_dir() {
            for item in WalkDir::new(p).contents_first(true).follow_links(false) {
                let entry = item.map_err(BatchExpandError::Walk)?;
                out.push(entry.into_path());
            }
        } else {
            out.push(p.clone());
        }
    }
    Ok(out)
}

/// Flatten `paths` (files and folders) into the list of files below them, e.g. as
/// the work list for a batch copy/delete/zip. Folders themselves are returned
/// after their contents (children first), which is the order deletion needs.
///
/// This compatibility helper is best-effort and skips traversal errors. New batch-operation
/// callers that must not omit descendants should use [`try_expand_for_batch`].
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
    fn compiled_name_matcher_keeps_case_insensitive_semantics() {
        let wildcard = NameMatcher::new("*.tXt");
        assert!(wildcard.matches("Report.TXT"));
        assert!(!wildcard.matches("Report.rs"));

        let substring = NameMatcher::new("ÉCOLE");
        assert!(substring.matches("école.txt"));
        assert!(NameMatcher::new("").matches("anything"));
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

        // A zero cap means no results, not one result followed by a stop.
        o.max_results = Some(0);
        let mut emitted = 0;
        assert_eq!(search(d.path(), &o, &c, |_| emitted += 1), 0);
        assert_eq!(emitted, 0);
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
        let inputs = [d.path().join("a"), d.path().join("top.txt")];
        let v = expand_for_batch(&inputs);
        let checked = try_expand_for_batch(&inputs).unwrap();
        assert_eq!(v, checked);
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

    #[test]
    fn checked_batch_expansion_reports_missing_roots() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("does-not-exist");
        let error = try_expand_for_batch(std::slice::from_ref(&missing)).unwrap_err();
        match error {
            BatchExpandError::Metadata { path, source } => {
                assert_eq!(path, missing);
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_remains_visible() {
        use std::os::unix::fs::symlink;

        let d = tempfile::tempdir().unwrap();
        symlink("missing-target", d.path().join("broken-link")).unwrap();
        let entries: Vec<_> = walk(
            d.path(),
            &WalkOptions {
                include_hidden: true,
                ..Default::default()
            },
        )
        .collect();
        let link = entries
            .iter()
            .find(|e| e.path.ends_with("broken-link"))
            .unwrap();
        assert_eq!(link.kind, EntryType::Symlink);
        assert_eq!(link.size, "missing-target".len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn hidden_non_utf8_names_are_still_hidden() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let d = tempfile::tempdir().unwrap();
        let name = OsString::from_vec(b".hidden-\xff".to_vec());
        fs::write(d.path().join(name), b"data").unwrap();
        assert_eq!(walk(d.path(), &WalkOptions::default()).count(), 0);
        let visible = walk(
            d.path(),
            &WalkOptions {
                include_hidden: true,
                ..Default::default()
            },
        )
        .count();
        assert_eq!(visible, 1);
    }

    #[test]
    fn dropping_search_with_full_queue_does_not_deadlock() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..400 {
            fs::write(d.path().join(format!("entry-{i:04}.txt")), b"x").unwrap();
        }
        let handle = spawn_search(d.path().to_owned(), SearchOptions::new(""));
        // Leave hits unread long enough for a bounded producer to encounter backpressure.
        thread::sleep(Duration::from_millis(20));
        handle.cancel();
        drop(handle);
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
