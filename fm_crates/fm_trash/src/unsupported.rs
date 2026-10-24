//! Stub for platforms without a supported implementation.

use std::path::{Path, PathBuf};

use crate::{Error, TrashItem};

/// Handle to the user's trash.
#[derive(Debug, Clone)]
pub struct Trash {
    home: PathBuf,
}

impl Trash {
    /// Always fails on this platform.
    pub fn new() -> Result<Self, Error> {
        Err(Error::Unsupported)
    }
    /// Use an explicit home-trash directory.
    pub fn with_home_trash(dir: impl Into<PathBuf>) -> Self {
        Self { home: dir.into() }
    }
    /// The home trash directory.
    pub fn home_trash_dir(&self) -> &Path {
        &self.home
    }
    /// Always fails on this platform.
    pub fn trash(&self, _path: impl AsRef<Path>) -> Result<TrashItem, Error> {
        Err(Error::Unsupported)
    }
    /// Always fails on this platform.
    pub fn trash_all<I, P>(&self, paths: I) -> Vec<Result<TrashItem, Error>>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        paths.into_iter().map(|_| Err(Error::Unsupported)).collect()
    }
    /// Always fails on this platform.
    pub fn list(&self) -> Result<Vec<TrashItem>, Error> {
        Err(Error::Unsupported)
    }
    /// Always fails on this platform.
    pub fn empty(&self) -> Result<usize, Error> {
        Err(Error::Unsupported)
    }
}

pub(crate) fn restore(_: &TrashItem) -> Result<PathBuf, Error> {
    Err(Error::Unsupported)
}
pub(crate) fn purge(_: &TrashItem) -> Result<(), Error> {
    Err(Error::Unsupported)
}
