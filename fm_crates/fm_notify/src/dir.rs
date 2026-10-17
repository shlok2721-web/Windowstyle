use std::{
    io,
    path::{Path, PathBuf},
    sync::mpsc::Receiver,
};

use crate::{Config, Event, Watcher};

/// Convenience for a file manager: keeps exactly one folder watched (the one on screen).
///
/// Call [`set_dir`](Self::set_dir) when the user navigates and [`poll`](Self::poll)
/// once per frame; a non-empty result means "re-read the directory".
pub struct DirWatcher {
    watcher: Watcher,
    rx: Receiver<Vec<Event>>,
    current: Option<PathBuf>,
}

impl DirWatcher {
    /// `wake` runs on the watcher thread whenever events are queued
    /// (e.g. `move || ctx.request_repaint()`).
    pub fn new(wake: impl Fn() + Send + 'static) -> io::Result<Self> {
        Self::with_config(Config::default(), wake)
    }

    /// Like [`new`](Self::new) with explicit settings.
    pub fn with_config(config: Config, wake: impl Fn() + Send + 'static) -> io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = Watcher::new(config, move |batch| {
            if tx.send(batch).is_ok() {
                wake();
            }
        })?;
        Ok(Self {
            watcher,
            rx,
            current: None,
        })
    }

    /// Watch `dir` instead of the previous folder (`None` stops watching).
    /// Does nothing if `dir` is already the watched folder.
    pub fn set_dir(&mut self, dir: Option<&Path>) -> io::Result<()> {
        if self.current.as_deref() == dir {
            return Ok(());
        }
        if let Some(old) = self.current.take() {
            let _ = self.watcher.unwatch(&old);
        }
        while self.rx.try_recv().is_ok() {} // stale events from the previous folder
        if let Some(new) = dir {
            self.watcher.watch(new)?;
            self.current = Some(new.to_path_buf());
        }
        Ok(())
    }

    /// The folder currently watched.
    pub fn dir(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    /// Take all events that arrived since the last call (empty = nothing changed).
    /// If the folder itself disappeared, the event is returned and the watch is cleared.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut all = Vec::new();
        while let Ok(batch) = self.rx.try_recv() {
            all.extend(batch);
        }
        if all.iter().any(|e| matches!(e, Event::WatchRemoved { .. })) {
            self.current = None;
        }
        all
    }
}
