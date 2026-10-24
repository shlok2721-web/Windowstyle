# fm_notify

Detect filesystem changes so a directory listing refreshes itself. Linux **inotify** via the safe `rustix`
wrapper. Events are debounced (default 100 ms) and coalesced: copying a big file gives one `Created`,
an editor's save-by-rename gives one `Modified`, a temp file that appears and vanishes gives nothing.

* No `unsafe`, **no egui dependency**. Linux only; elsewhere `Watcher::new` returns `ErrorKind::Unsupported`.
* Directories are watched **non-recursively** (what a file manager needs for the folder on screen).
* `DirWatcher` bounds queued batches; if the UI falls behind, it emits `Rescan` instead of retaining an unbounded backlog.

```rust
let mut dirs = fm_notify::DirWatcher::new(move || ctx.request_repaint())?; // wake egui from the watcher thread
dirs.set_dir(Some(&path))?;                 // when Event::Navigate fires
// every frame:
if !dirs.poll().is_empty() { reload(&path); }
```

Lower level: `Watcher::new(config, |batch: Vec<Event>| ..)` or `Watcher::channel(config)`, with
`watch(path)` / `unwatch(path)`.

`Event` = `Created`, `Removed`, `Modified`, `Renamed { from, to }`, `WatchRemoved` (folder deleted/moved/unmounted; navigate away),
`Rescan` (kernel queue overflowed; re-read everything). `Event` is `#[non_exhaustive]`.
