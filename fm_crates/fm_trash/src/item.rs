use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::Error;

/// One item in the trash. Returned by `trash()` (so it can be undone) and by `list()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashItem {
    pub(crate) trash_dir: PathBuf,
    /// Unique name inside `<trash_dir>/files` (not necessarily the original name).
    pub(crate) stored_name: OsString,
    pub(crate) original_path: PathBuf,
    pub(crate) deleted_at: Option<SystemTime>,
    /// `false` when `trash_dir` is a per-volume can (`<mount>/.Trash-$uid` or
    /// `<mount>/.Trash/$uid`) rather than the user's home trash. Purging the last item
    /// out of a volume can removes that scaffold directory too, so an emptied removable
    /// drive is not left with a stray `.Trash-*` folder; the home trash is never removed.
    pub(crate) is_home: bool,
}

impl TrashItem {
    /// Where the item lived before it was trashed.
    pub fn original_path(&self) -> &Path {
        &self.original_path
    }

    /// The original file name (last component of [`original_path`](Self::original_path)).
    pub fn file_name(&self) -> &OsStr {
        self.original_path
            .file_name()
            .unwrap_or(self.stored_name.as_os_str())
    }

    /// When the item was trashed (local time recorded in the `.trashinfo`), if known.
    pub fn deleted_at(&self) -> Option<SystemTime> {
        self.deleted_at
    }

    /// Where the data currently lives inside the trash (useful for previews / sizes).
    pub fn trashed_path(&self) -> PathBuf {
        self.trash_dir.join("files").join(&self.stored_name)
    }

    /// The `.trashinfo` record of this item.
    pub fn info_path(&self) -> PathBuf {
        let mut n = self.stored_name.clone();
        n.push(".trashinfo");
        self.trash_dir.join("info").join(n)
    }

    /// Move the item back to its original location (parent folders are re-created).
    /// Never overwrites: fails with [`Error::AlreadyExists`] if the location is taken.
    pub fn restore(&self) -> Result<PathBuf, Error> {
        crate::imp::restore(self)
    }

    /// Permanently delete this item from the trash.
    pub fn purge(&self) -> Result<(), Error> {
        crate::imp::purge(self)
    }
}
