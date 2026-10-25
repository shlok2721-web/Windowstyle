use std::path::PathBuf;

/// A filesystem change, after coalescing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A file or folder appeared (created, or moved in from outside the watched folder).
    Created {
        /// Full path of the new entry.
        path: PathBuf,
        /// Whether it is a directory.
        is_dir: bool,
    },
    /// A file or folder disappeared (deleted, or moved out of the watched folder).
    Removed {
        /// Full path of the removed entry.
        path: PathBuf,
        /// Whether it was a directory.
        is_dir: bool,
    },
    /// Contents, size, timestamps or permissions changed.
    Modified {
        /// Full path of the entry.
        path: PathBuf,
    },
    /// An entry was renamed or moved within the watched folder.
    Renamed {
        /// Old path.
        from: PathBuf,
        /// New path.
        to: PathBuf,
        /// Whether it is a directory.
        is_dir: bool,
    },
    /// The watched folder itself was deleted, moved or unmounted; the watch is gone.
    WatchRemoved {
        /// The path that was being watched.
        path: PathBuf,
    },
    /// The kernel queue overflowed and events were lost: re-read everything.
    Rescan,
}
