//! inotify backend: one background thread, an eventfd to stop it.

use std::{
    collections::HashMap,
    ffi::OsStr,
    io,
    mem::MaybeUninit,
    os::{fd::OwnedFd, unix::ffi::OsStrExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Instant,
};

use rustix::{
    event::{EventfdFlags, PollFd, PollFlags, eventfd, poll},
    fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags},
    io::Errno,
};

use crate::{
    Config, Event,
    coalesce::{Raw, coalesce},
};

#[derive(Default)]
struct State {
    by_wd: HashMap<i32, PathBuf>,
    by_path: HashMap<PathBuf, i32>,
}

struct Shared {
    inotify: OwnedFd,
    wake: OwnedFd,
    state: Mutex<State>,
    stop: AtomicBool,
}

/// Watches paths and reports changes as batches of [`Event`]s, from a background thread.
///
/// Dropping the watcher stops the thread. Directories are watched non-recursively.
pub struct Watcher {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

fn to_io(e: Errno) -> io::Error {
    io::Error::from(e)
}

impl Watcher {
    /// Start a watcher. `handler` is called on the watcher thread with each non-empty batch;
    /// keep it quick (send to a channel, request a repaint, ...).
    pub fn new(
        config: Config,
        handler: impl FnMut(Vec<Event>) + Send + 'static,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            inotify: inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).map_err(to_io)?,
            wake: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).map_err(to_io)?,
            state: Mutex::new(State::default()),
            stop: AtomicBool::new(false),
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("fm_notify".into())
            .spawn(move || run(worker, config, handler))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Start a watcher that delivers batches through a channel.
    pub fn channel(config: Config) -> io::Result<(Self, mpsc::Receiver<Vec<Event>>)> {
        let (tx, rx) = mpsc::channel();
        let w = Self::new(config, move |batch| {
            let _ = tx.send(batch);
        })?;
        Ok((w, rx))
    }

    /// Watch a directory (or a single file). Reported paths use the supplied path as their base.
    ///
    /// Returns [`io::ErrorKind::AlreadyExists`] if the same filesystem object is already watched
    /// through a different pathname, because one inotify watch descriptor cannot represent both
    /// aliases without corrupting path-to-watch bookkeeping.
    pub fn watch(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = std::path::absolute(path.as_ref())?;
        let flags = WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MODIFY
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::ATTRIB
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF
            | WatchFlags::EXCL_UNLINK;
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        // A watch descriptor identifies an inode, not a pathname. Watching a
        // symlink alias to an already watched directory can therefore return
        // an existing descriptor. Keep one pathname per descriptor instead of
        // creating inconsistent forward/reverse mappings.
        let previous_wd = st.by_path.get(&path).copied();
        let wd = inotify::add_watch(&self.shared.inotify, &path, flags).map_err(to_io)?;
        if st.by_wd.get(&wd).is_some_and(|existing| existing != &path) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "this filesystem object is already watched through another path",
            ));
        }

        if let Some(old_wd) = previous_wd.filter(|old_wd| *old_wd != wd) {
            // The pathname may have been replaced since it was first watched.
            // Remove the old descriptor only after the replacement watch exists.
            match inotify::remove_watch(&self.shared.inotify, old_wd) {
                Ok(()) | Err(Errno::INVAL) => {}
                Err(e) => {
                    let _ = inotify::remove_watch(&self.shared.inotify, wd);
                    return Err(to_io(e));
                }
            }
            if st.by_wd.get(&old_wd) == Some(&path) {
                st.by_wd.remove(&old_wd);
            }
        }
        st.by_wd.insert(wd, path.clone());
        st.by_path.insert(path, wd);
        Ok(())
    }

    /// Stop watching `path` (no error if it wasn't watched).
    pub fn unwatch(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = std::path::absolute(path.as_ref())?;
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(wd) = st.by_path.get(&path).copied() {
            // Preserve the state mapping if the kernel refuses removal so the
            // caller can retry and events are not silently detached from it.
            match inotify::remove_watch(&self.shared.inotify, wd) {
                Ok(()) | Err(Errno::INVAL) => {}
                Err(e) => return Err(to_io(e)),
            }
            st.by_path.remove(&path);
            if st.by_wd.get(&wd) == Some(&path) {
                st.by_wd.remove(&wd);
            }
        }
        Ok(())
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let _ = rustix::io::write(&self.shared.wake, &1u64.to_ne_bytes());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// Keep a burst from retaining an unbounded number of paths while the debounce
// timer is pending. Once the cap is exceeded, exact deltas are no longer
// trustworthy, so request one full rescan and retain watch-removal notices.
const MAX_PENDING_RAW_EVENTS: usize = 8 * 1024;

fn push_pending(out: &mut Vec<Raw>, raw: Raw) {
    if matches!(&raw, Raw::Overflow) {
        let removed: Vec<PathBuf> = out
            .drain(..)
            .filter_map(|event| match event {
                Raw::WatchRemoved { path } => Some(path),
                _ => None,
            })
            .collect();
        out.push(Raw::Overflow);
        for path in removed {
            push_watch_removed(out, path);
        }
        return;
    }

    if out
        .first()
        .is_some_and(|event| matches!(event, Raw::Overflow))
    {
        if let Raw::WatchRemoved { path } = raw {
            push_watch_removed(out, path);
        }
        return;
    }

    if out.len() < MAX_PENDING_RAW_EVENTS {
        out.push(raw);
        return;
    }

    let removed: Vec<PathBuf> = out
        .drain(..)
        .filter_map(|event| match event {
            Raw::WatchRemoved { path } => Some(path),
            _ => None,
        })
        .collect();
    out.push(Raw::Overflow);
    for path in removed {
        push_watch_removed(out, path);
    }
    if let Raw::WatchRemoved { path } = raw {
        push_watch_removed(out, path);
    }
}

fn push_watch_removed(out: &mut Vec<Raw>, path: PathBuf) {
    if !out
        .iter()
        .any(|event| matches!(event, Raw::WatchRemoved { path: p } if p == &path))
    {
        out.push(Raw::WatchRemoved { path });
    }
}

fn run(shared: Arc<Shared>, config: Config, mut handler: impl FnMut(Vec<Event>)) {
    let mut buf = [MaybeUninit::<u8>::uninit(); 16 * 1024];
    let mut pending: Vec<Raw> = Vec::new();
    let mut deadline: Option<Instant> = None;

    loop {
        let timeout = deadline.map(|d| {
            let left = d.saturating_duration_since(Instant::now());
            rustix::event::Timespec {
                tv_sec: left.as_secs() as _,
                tv_nsec: left.subsec_nanos() as _,
            }
        });
        let (readable, woken) = {
            let mut fds = [
                PollFd::new(&shared.inotify, PollFlags::IN),
                PollFd::new(&shared.wake, PollFlags::IN),
            ];
            match poll(&mut fds, timeout.as_ref()) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(_) => return,
            }
            (
                fds[0].revents().contains(PollFlags::IN),
                fds[1].revents().contains(PollFlags::IN),
            )
        };
        if woken || shared.stop.load(Ordering::SeqCst) {
            return;
        }
        if readable {
            drain(&shared, &mut buf, &mut pending);
            if !pending.is_empty() && deadline.is_none() {
                deadline = Some(Instant::now() + config.debounce);
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            deadline = None;
            let events = coalesce(std::mem::take(&mut pending));
            if !events.is_empty() {
                handler(events);
            }
        }
    }
}

/// Read every queued inotify event and translate it.
fn drain(shared: &Shared, buf: &mut [MaybeUninit<u8>], out: &mut Vec<Raw>) {
    let mut reader = inotify::Reader::new(&shared.inotify, buf);
    loop {
        let ev = match reader.next() {
            Ok(ev) => ev,
            Err(Errno::WOULDBLOCK) => return,
            Err(Errno::INTR) => continue,
            Err(_) => return,
        };
        let mask = ev.events();
        if mask.contains(ReadFlags::QUEUE_OVERFLOW) {
            push_pending(out, Raw::Overflow);
            continue;
        }
        let wd = ev.wd();
        let base = {
            let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(base) = st.by_wd.get(&wd).cloned() else {
                continue;
            };

            if mask.contains(ReadFlags::IGNORED) {
                st.by_wd.remove(&wd);
                if st.by_path.get(&base) == Some(&wd) {
                    st.by_path.remove(&base);
                }
                continue;
            }
            if mask.intersects(ReadFlags::DELETE_SELF | ReadFlags::UNMOUNT) {
                drop(st);
                push_pending(out, Raw::WatchRemoved { path: base });
                continue; // the kernel follows up with IGNORED
            }
            if mask.contains(ReadFlags::MOVE_SELF) {
                st.by_wd.remove(&wd);
                if st.by_path.get(&base) == Some(&wd) {
                    st.by_path.remove(&base);
                }
                let _ = inotify::remove_watch(&shared.inotify, wd);
                drop(st);
                push_pending(out, Raw::WatchRemoved { path: base });
                continue;
            }
            base
        };

        let path = match ev.file_name().map(|n| n.to_bytes()) {
            Some(n) if !n.is_empty() => base.join(OsStr::from_bytes(n)),
            _ => base,
        };
        let is_dir = mask.contains(ReadFlags::ISDIR);
        let cookie = ev.cookie();
        if mask.contains(ReadFlags::CREATE) {
            push_pending(out, Raw::Created { path, is_dir });
        } else if mask.contains(ReadFlags::DELETE) {
            push_pending(out, Raw::Removed { path, is_dir });
        } else if mask.contains(ReadFlags::MOVED_FROM) {
            push_pending(
                out,
                Raw::MovedFrom {
                    path,
                    is_dir,
                    cookie,
                },
            );
        } else if mask.contains(ReadFlags::MOVED_TO) {
            push_pending(
                out,
                Raw::MovedTo {
                    path,
                    is_dir,
                    cookie,
                },
            );
        } else if mask.intersects(ReadFlags::MODIFY | ReadFlags::CLOSE_WRITE | ReadFlags::ATTRIB) {
            push_pending(out, Raw::Modified { path });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_event_cap_collapses_delta_to_rescan() {
        let mut pending = Vec::with_capacity(MAX_PENDING_RAW_EVENTS);
        for i in 0..MAX_PENDING_RAW_EVENTS {
            push_pending(
                &mut pending,
                Raw::Modified {
                    path: PathBuf::from(format!("/d/{i}")),
                },
            );
        }
        assert_eq!(pending.len(), MAX_PENDING_RAW_EVENTS);

        push_pending(
            &mut pending,
            Raw::Modified {
                path: PathBuf::from("/d/overflow"),
            },
        );
        assert_eq!(pending, vec![Raw::Overflow]);
    }

    #[test]
    fn overflow_retains_distinct_watch_removals_only_once() {
        let mut pending = Vec::new();
        push_pending(
            &mut pending,
            Raw::WatchRemoved {
                path: PathBuf::from("/d"),
            },
        );
        push_pending(&mut pending, Raw::Overflow);
        push_pending(
            &mut pending,
            Raw::WatchRemoved {
                path: PathBuf::from("/d"),
            },
        );
        push_pending(
            &mut pending,
            Raw::WatchRemoved {
                path: PathBuf::from("/other"),
            },
        );
        assert_eq!(
            pending,
            vec![
                Raw::Overflow,
                Raw::WatchRemoved {
                    path: PathBuf::from("/d")
                },
                Raw::WatchRemoved {
                    path: PathBuf::from("/other")
                },
            ]
        );
    }
}
