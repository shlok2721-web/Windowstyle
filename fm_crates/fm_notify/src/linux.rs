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

    /// Watch a directory (or a single file). Reported paths are `path` joined with the entry name.
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
        let wd = inotify::add_watch(&self.shared.inotify, &path, flags).map_err(to_io)?;
        st.by_wd.insert(wd, path.clone());
        st.by_path.insert(path, wd);
        Ok(())
    }

    /// Stop watching `path` (no error if it wasn't watched).
    pub fn unwatch(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = std::path::absolute(path.as_ref())?;
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(wd) = st.by_path.remove(&path) {
            st.by_wd.remove(&wd);
            match inotify::remove_watch(&self.shared.inotify, wd) {
                Ok(()) | Err(Errno::INVAL) => {}
                Err(e) => return Err(to_io(e)),
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
            out.push(Raw::Overflow);
            continue;
        }
        let wd = ev.wd();
        let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(base) = st.by_wd.get(&wd).cloned() else {
            continue;
        };

        if mask.contains(ReadFlags::IGNORED) {
            st.by_wd.remove(&wd);
            st.by_path.remove(&base);
            continue;
        }
        if mask.intersects(ReadFlags::DELETE_SELF | ReadFlags::UNMOUNT) {
            out.push(Raw::WatchRemoved { path: base });
            continue; // the kernel follows up with IGNORED
        }
        if mask.contains(ReadFlags::MOVE_SELF) {
            st.by_wd.remove(&wd);
            st.by_path.remove(&base);
            let _ = inotify::remove_watch(&shared.inotify, wd);
            out.push(Raw::WatchRemoved { path: base });
            continue;
        }

        let path = match ev.file_name().map(|n| n.to_bytes()) {
            Some(n) if !n.is_empty() => base.join(OsStr::from_bytes(n)),
            _ => base,
        };
        let is_dir = mask.contains(ReadFlags::ISDIR);
        let cookie = ev.cookie();
        if mask.contains(ReadFlags::CREATE) {
            out.push(Raw::Created { path, is_dir });
        } else if mask.contains(ReadFlags::DELETE) {
            out.push(Raw::Removed { path, is_dir });
        } else if mask.contains(ReadFlags::MOVED_FROM) {
            out.push(Raw::MovedFrom {
                path,
                is_dir,
                cookie,
            });
        } else if mask.contains(ReadFlags::MOVED_TO) {
            out.push(Raw::MovedTo {
                path,
                is_dir,
                cookie,
            });
        } else if mask.intersects(ReadFlags::MODIFY | ReadFlags::CLOSE_WRITE | ReadFlags::ATTRIB) {
            out.push(Raw::Modified { path });
        }
    }
}
