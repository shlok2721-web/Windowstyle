//! A logind *delay* inhibitor: while held, suspend and shutdown wait (a few seconds at most,
//! `InhibitDelayMaxSec`) so state can be flushed first. Implemented with `systemd-inhibit`
//! running `cat`; closing `cat`'s stdin ends the command and releases the lock.

use std::process::{Child, Command, Stdio};
use std::thread;

pub(crate) struct Inhibitor {
    child: Child,
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
        Some(Self { child })
    }

    /// Release the lock and reap the helper process without blocking the caller.
    pub(crate) fn release(mut self) {
        drop(self.child.stdin.take());
        let mut child = self.child;
        let _ = thread::Builder::new()
            .name("file-manager-inhibit-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
}
