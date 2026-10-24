//! Process lifecycle hooks, delivered as plain [`Event`]s you poll from your UI loop.
//!
//! * **Signals**: `SIGTERM`, `SIGHUP` and `SIGINT` (through `signal-hook`, so this crate
//!   itself contains no `unsafe`). After the first one a watchdog gives the application
//!   [`TERMINATE_GRACE`] to exit by itself, then exits the process with `128 + signal`.
//! * **Suspend / shutdown**: systemd-logind's `PrepareForSleep` / `PrepareForShutdown`
//!   signals (read from `dbus-monitor` or `gdbus`), plus a logind *delay inhibitor* so that
//!   suspend and shutdown wait while you flush. Call
//!   [`release_inhibitor`](Lifecycle::release_inhibitor) when done and
//!   [`rearm_inhibitor`](Lifecycle::rearm_inhibitor) after [`Event::Resume`].
//!
//! Everything is best effort: a missing tool only means that hook is unavailable, see
//! [`Lifecycle::status`]. Idle cost is zero (blocked threads, no polling).
//!
//! ```ignore
//! let wake = ctx.clone();
//! let mut life = fm_lifecycle::Lifecycle::start(move || wake.request_repaint());
//! // every frame:
//! for event in life.poll() {
//!     match event {
//!         fm_lifecycle::Event::Terminate(_) | fm_lifecycle::Event::Shutdown => { /* flush, then exit */ }
//!         fm_lifecycle::Event::Suspend => { /* flush */ life.release_inhibitor(); }
//!         fm_lifecycle::Event::Resume => life.rearm_inhibitor(),
//!     }
//! }
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod inhibit;
mod monitor;

use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::{Handle, Signals};

use inhibit::Inhibitor;

/// How long the application gets to exit after a terminating signal before the process is
/// ended anyway.
pub const TERMINATE_GRACE: Duration = Duration::from_millis(500);

/// Something the application should react to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// `SIGTERM`, `SIGHUP` or `SIGINT` (the number is carried): flush and exit.
    Terminate(i32),
    /// The system is about to suspend or hibernate: flush now, then release the inhibitor.
    Suspend,
    /// The system woke up: re-arm the inhibitor and refresh anything time-sensitive.
    Resume,
    /// The system is about to shut down or reboot: flush and exit.
    Shutdown,
}

/// Which hooks are actually working.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Signal handlers are installed.
    pub signals: bool,
    /// A logind monitor process is running.
    pub suspend_monitor: bool,
    /// A delay inhibitor is currently held.
    pub inhibitor: bool,
}

/// Owner of the signal handlers, the logind monitor and the inhibitor. Dropping it removes
/// all of them.
pub struct Lifecycle {
    rx: Receiver<Event>,
    tx: Sender<Event>,
    signals: Option<Handle>,
    signal_thread: Option<JoinHandle<()>>,
    monitor_child: Arc<Mutex<Option<Child>>>,
    monitor_active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    monitor_thread: Option<JoinHandle<()>>,
    inhibitor: Option<Inhibitor>,
}

fn start_signals(
    tx: Sender<Event>,
    wake: Arc<dyn Fn() + Send + Sync>,
    stop: Arc<AtomicBool>,
) -> Option<(Handle, JoinHandle<()>)> {
    let mut signals = Signals::new([SIGTERM, SIGHUP, SIGINT]).ok()?;
    let handle = signals.handle();
    let worker = thread::Builder::new()
        .name("file-manager-signals".into())
        .spawn(move || {
            let Some(signal) = signals.forever().next() else {
                return;
            };
            let _ = tx.send(Event::Terminate(signal));
            wake();

            // Watchdog: if the UI is stuck, do not let the process hang forever. The owner
            // can cancel it by dropping Lifecycle after completing its own shutdown.
            let deadline = Instant::now() + TERMINATE_GRACE;
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                // park_timeout avoids periodic wakeups; unpark from Drop cancels the wait.
                // The deadline loop also handles spurious wakeups without shortening grace.
                thread::park_timeout(deadline - now);
            }
            if !stop.load(Ordering::Acquire) {
                std::process::exit(128 + signal);
            }
        })
        .ok()?;
    Some((handle, worker))
}

impl Lifecycle {
    /// Install the hooks. `wake` is called from a background thread whenever an event is
    /// queued (for egui: `move || ctx.request_repaint()`). Never fails: unavailable hooks are
    /// reported by [`status`](Self::status).
    pub fn start(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let monitor_child = Arc::new(Mutex::new(None));
        let monitor_active = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (signals, signal_thread) =
            start_signals(tx.clone(), Arc::clone(&wake), Arc::clone(&stop))
                .map_or((None, None), |(handle, worker)| {
                    (Some(handle), Some(worker))
                });
        let monitor_thread = monitor::spawn(
            tx.clone(),
            wake,
            Arc::clone(&monitor_child),
            Arc::clone(&monitor_active),
            Arc::clone(&stop),
        );
        Self {
            rx,
            tx,
            signals,
            signal_thread,
            monitor_child,
            monitor_active,
            stop,
            monitor_thread,
            inhibitor: Inhibitor::acquire(),
        }
    }

    /// Take every queued event (non-blocking).
    pub fn poll(&self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Queue an event as if it had come from the system (for tests and menu commands).
    pub fn inject(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// Which hooks are working right now.
    pub fn status(&self) -> Status {
        Status {
            signals: self.signals.is_some(),
            suspend_monitor: self.monitor_active.load(Ordering::Acquire),
            inhibitor: self.inhibitor.is_some(),
        }
    }

    /// Let a pending suspend or shutdown proceed. Call after flushing.
    pub fn release_inhibitor(&mut self) {
        if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.release();
        }
    }

    /// Hold a delay inhibitor again (no-op when one is held). Call after [`Event::Resume`].
    pub fn rearm_inhibitor(&mut self) {
        if self.inhibitor.is_none() {
            self.inhibitor = Inhibitor::acquire();
        }
    }
}

impl Drop for Lifecycle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.signals.take() {
            handle.close();
        }
        let signal_worker = self.signal_thread.take();
        if let Some(worker) = signal_worker.as_ref() {
            worker.thread().unpark();
        }

        // Release the delay inhibitor before any waits below. Drop means the owner is
        // shutting down; holding the system sleep lock while reaping workers would make a
        // stalled cleanup path delay suspend or shutdown unnecessarily.
        self.release_inhibitor();

        let child = self
            .monitor_child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Killing the monitor child above unblocks its stdout reader. Join both owned
        // workers so no lifecycle thread remains detached after this owner is dropped.
        if let Some(worker) = self.monitor_thread.take() {
            let _ = worker.join();
        }
        if let Some(worker) = signal_worker {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_events_are_polled_in_order_and_drained() {
        let (tx, rx) = mpsc::channel();
        let life = Lifecycle {
            rx,
            tx,
            signals: None,
            signal_thread: None,
            monitor_child: Arc::new(Mutex::new(None)),
            monitor_active: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            monitor_thread: None,
            inhibitor: None,
        };
        life.inject(Event::Suspend);
        life.inject(Event::Terminate(15));
        assert_eq!(life.poll(), [Event::Suspend, Event::Terminate(15)]);
        assert!(life.poll().is_empty());
        assert_eq!(life.status(), Status::default());
    }
}
