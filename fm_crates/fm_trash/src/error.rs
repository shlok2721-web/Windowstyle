use std::{fmt, io, path::PathBuf};

/// Errors returned by this crate.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// This platform has no supported trash implementation (Linux only).
    Unsupported,
    /// Neither `$XDG_DATA_HOME` nor `$HOME` is usable, so the home trash can't be located.
    NoHome,
    /// The path to trash does not exist.
    NotFound(PathBuf),
    /// The path has no file name (e.g. `/` or `foo/..`).
    InvalidPath(PathBuf),
    /// The path is a mount point and can't be trashed.
    MountPoint(PathBuf),
    /// The directory contains a mount point and can't safely be moved or deleted as a tree.
    ContainsMountPoint(PathBuf),
    /// The path is part of, or contains, a trash directory.
    OverlapsTrash(PathBuf),
    /// Restoring: something already exists at the original location.
    AlreadyExists(PathBuf),
    /// Restoring/purging: the item's record is malformed.
    InvalidItem(PathBuf),
    /// An I/O error, with the path it concerns.
    Io {
        /// Path the operation was working on.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported => write!(f, "trash is not supported on this platform"),
            Error::NoHome => write!(
                f,
                "cannot locate the home trash ($XDG_DATA_HOME / $HOME unset)"
            ),
            Error::NotFound(p) => write!(f, "{}: no such file or directory", p.display()),
            Error::InvalidPath(p) => write!(f, "{}: path has no file name", p.display()),
            Error::MountPoint(p) => write!(f, "{}: is a mount point", p.display()),
            Error::ContainsMountPoint(p) => write!(
                f,
                "{}: contains a mount point; refusing to move or delete the directory tree",
                p.display()
            ),
            Error::OverlapsTrash(p) => write!(
                f,
                "{}: is part of, or contains, a trash directory",
                p.display()
            ),
            Error::AlreadyExists(p) => write!(f, "{}: already exists", p.display()),
            Error::InvalidItem(p) => write!(f, "{}: invalid trash item", p.display()),
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
