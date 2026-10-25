//! A logind *delay* inhibitor: while held, suspend and shutdown wait (a few seconds at most,
//! `InhibitDelayMaxSec`) so state can be flushed first. Implemented with `systemd-inhibit`
//! running `cat`; closing `cat`'s stdin ends the command and releases the lock.

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

pub(crate) struct Inhibitor {
    child: Option<Child>,
}

impl Inhibitor {
    /// Take the lock. `None` when `systemd-inhibit` is not available.
    pub(crate) fn acquire() -> Option<Self> {
        let child = Command::new("systemd-inhibit")
            .args([
                "--what=sleep:shutdown",
                "--mode=delay",
                "--who=file-manager",
                "--why=Saving state before sleep or shutdown",
                "cat",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        Some(Self { child: Some(child) })
    }

    /// Release the lock and normally reap the helper process asynchronously.
    pub(crate) fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        drop(child.stdin.take());

        // Keep ownership in a shared slot until the reaper thread has definitely started.
        // If thread creation fails, synchronously reap the now-unblocked helper rather than
        // silently dropping Child and potentially leaving a zombie process behind.
        let child_slot = Arc::new(Mutex::new(Some(child)));
        let worker_slot = Arc::clone(&child_slot);
        let spawned = thread::Builder::new()
            .name("file-manager-inhibit-reaper".into())
            .spawn(move || {
                let child = worker_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(mut child) = child {
                    let _ = child.wait();
                }
            });
        if spawned.is_err() {
            let child = child_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(mut child) = child {
                let _ = child.wait();
            }
        }
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        self.release_inner();
    }
}
