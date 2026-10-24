use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TrySendError},
    },
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
    watched_path: Option<PathBuf>,
    dropped_events: Arc<AtomicBool>,
    dropped_watch_removals: Arc<Mutex<HashSet<PathBuf>>>,
}

impl DirWatcher {
    /// `wake` runs on the watcher thread whenever events are queued
    /// (e.g. `move || ctx.request_repaint()`).
    pub fn new(wake: impl Fn() + Send + 'static) -> io::Result<Self> {
        Self::with_config(Config::default(), wake)
    }

    /// Like [`new`](Self::new) with explicit settings.
    pub fn with_config(config: Config, wake: impl Fn() + Send + 'static) -> io::Result<Self> {
        // Bound the UI-side queue. If the consumer falls behind, dropped
        // deltas are replaced by a Rescan on the next poll rather than allowing
        // queued batches to retain memory without limit.
        const QUEUED_BATCH_LIMIT: usize = 8;
        let (tx, rx) = mpsc::sync_channel(QUEUED_BATCH_LIMIT);
        let dropped_events = Arc::new(AtomicBool::new(false));
        let overflow_flag = Arc::clone(&dropped_events);
        let dropped_watch_removals = Arc::new(Mutex::new(HashSet::new()));
        let pending_removals = Arc::clone(&dropped_watch_removals);
        let watcher = Watcher::new(config, move |batch| match tx.try_send(batch) {
            Ok(()) => wake(),
            Err(TrySendError::Full(batch)) => {
                overflow_flag.store(true, Ordering::Release);
                let mut removals = pending_removals
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for path in batch.into_iter().filter_map(|event| match event {
                    Event::WatchRemoved { path } => Some(path),
                    _ => None,
                }) {
                    removals.insert(path);
                }
                drop(removals);
                wake();
            }
            Err(TrySendError::Disconnected(_)) => {}
        })?;
        Ok(Self {
            watcher,
            rx,
            current: None,
            watched_path: None,
            dropped_events,
            dropped_watch_removals,
        })
    }

    /// Watch `dir` instead of the previous folder (`None` stops watching).
    /// Does nothing if `dir` is already the watched folder.
    pub fn set_dir(&mut self, dir: Option<&Path>) -> io::Result<()> {
        if self.current.as_deref() == dir {
            return Ok(());
        }
        // Compute the backend path before changing the current watch, so an
        // error leaves the existing folder operational.
        let absolute = dir.map(std::path::absolute).transpose()?;
        if let Some(old) = self.watched_path.as_ref() {
            self.watcher.unwatch(old)?;
        }
        self.current = None;
        self.watched_path = None;
        while self.rx.try_recv().is_ok() {} // drain batches already queued for the previous folder

        if let (Some(requested), Some(watch_path)) = (dir, absolute) {
            self.watcher.watch(&watch_path)?;
            self.current = Some(requested.to_path_buf());
            self.watched_path = Some(watch_path);
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

        let dropped_removals = {
            let mut removals = self
                .dropped_watch_removals
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            removals.drain().collect::<Vec<_>>()
        };
        let Some(current) = self.watched_path.as_ref() else {
            // A callback may already be in flight while navigation changes.
            // With no active folder, none of its events can refresh the view.
            return Vec::new();
        };
        all.retain(|event| event_matches_dir(event, current));
        for path in dropped_removals {
            if &path == current
                && !all.iter().any(|event| matches!(event, Event::WatchRemoved { path: existing } if existing == &path))
            {
                all.push(Event::WatchRemoved { path });
            }
        }
        if self.dropped_events.swap(false, Ordering::AcqRel) {
            all.push(Event::Rescan);
        }
        if all
            .iter()
            .any(|e| matches!(e, Event::WatchRemoved { path } if path == current))
        {
            self.current = None;
            self.watched_path = None;
        }
        all
    }
}

fn event_matches_dir(event: &Event, dir: &Path) -> bool {
    match event {
        Event::Created { path, .. } | Event::Removed { path, .. } | Event::Modified { path } => {
            path.starts_with(dir)
        }
        Event::Renamed { from, to, .. } => from.starts_with(dir) || to.starts_with(dir),
        Event::WatchRemoved { path } => path == dir,
        Event::Rescan => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    #[test]
    fn events_from_previous_directory_are_filtered() {
        let current = p("/tmp/current");
        assert!(!event_matches_dir(
            &Event::Created {
                path: p("/tmp/previous/file"),
                is_dir: false,
            },
            &current,
        ));
        assert!(event_matches_dir(
            &Event::Created {
                path: p("/tmp/current/file"),
                is_dir: false,
            },
            &current,
        ));
        assert!(!event_matches_dir(
            &Event::WatchRemoved {
                path: p("/tmp/previous"),
            },
            &current,
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dropped_batches_request_a_rescan() {
        let tmp = tempfile::tempdir().unwrap();
        let mut watcher = DirWatcher::new(|| {}).unwrap();
        watcher.set_dir(Some(tmp.path())).unwrap();
        watcher.dropped_events.store(true, Ordering::Release);
        assert_eq!(watcher.poll(), vec![Event::Rescan]);
    }

    #[test]
    fn rename_is_relevant_when_either_side_is_in_current_dir() {
        let current = p("/tmp/current");
        assert!(event_matches_dir(
            &Event::Renamed {
                from: p("/tmp/current/a"),
                to: p("/tmp/elsewhere/a"),
                is_dir: false,
            },
            &current,
        ));
    }
}
