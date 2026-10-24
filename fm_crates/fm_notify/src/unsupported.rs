//! Stub for platforms without inotify.

use std::{io, path::Path, sync::mpsc};

use crate::{Config, Event};

/// Watches paths and reports changes. Unsupported on this platform.
pub struct Watcher(());

fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "fm_notify supports Linux only",
    ))
}

impl Watcher {
    /// Always fails on this platform.
    pub fn new(
        _config: Config,
        _handler: impl FnMut(Vec<Event>) + Send + 'static,
    ) -> io::Result<Self> {
        unsupported()
    }
    /// Always fails on this platform.
    pub fn channel(_config: Config) -> io::Result<(Self, mpsc::Receiver<Vec<Event>>)> {
        unsupported()
    }
    /// Always fails on this platform.
    pub fn watch(&self, _path: impl AsRef<Path>) -> io::Result<()> {
        unsupported()
    }
    /// Always fails on this platform.
    pub fn unwatch(&self, _path: impl AsRef<Path>) -> io::Result<()> {
        unsupported()
    }
}
