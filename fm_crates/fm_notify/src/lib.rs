//! Detect filesystem changes so a directory listing can refresh itself.
//!
//! Built on Linux **inotify** (through the safe `rustix` wrapper). Events are
//! debounced (default 100 ms) and coalesced, so copying a big file produces one
//! `Created` instead of thousands of `Modified`.
//!
//! This crate has **no dependency on egui**. To wake an egui app, pass a closure
//! that calls `ctx.request_repaint()`:
//!
//! ```no_run
//! # fn demo(ctx: &impl Clone) -> std::io::Result<()> {
//! # struct Ctx; impl Ctx { fn request_repaint(&self) {} }
//! # let ctx = Ctx;
//! let mut dirs = fm_notify::DirWatcher::new(move || ctx.request_repaint())?;
//! dirs.set_dir(Some("/home/me".as_ref()))?;          // when the user navigates
//! // every frame:
//! if !dirs.poll().is_empty() { /* re-read the directory */ }
//! # Ok(()) }
//! ```
//!
//! Scope: a directory is watched **non-recursively** (what a file manager needs
//! for the folder it is showing). Linux only; elsewhere `Watcher::new` returns
//! `ErrorKind::Unsupported`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod coalesce;
mod dir;
mod event;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod imp;
#[cfg(not(target_os = "linux"))]
#[path = "unsupported.rs"]
mod imp;

pub use dir::DirWatcher;
pub use event::Event;
pub use imp::Watcher;

use std::time::Duration;

/// Watcher settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Events are delivered in batches at most this often (`ZERO` = deliver immediately).
    pub debounce: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(100),
        }
    }
}
