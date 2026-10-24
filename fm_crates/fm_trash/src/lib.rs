//! Move files to the desktop trash instead of deleting them permanently.
//!
//! Implements the [freedesktop.org Trash specification 1.0] (the same storage
//! GNOME, KDE, Thunar, `gio trash` and `trash-cli` use), so items trashed here can
//! be restored from any other file manager and vice versa.
//!
//! This crate has **no dependency on egui**; it only deals with `Path`s.
//! Linux only: on other platforms every call returns [`Error::Unsupported`].
//!
//! ```no_run
//! let item = fm_trash::trash("/home/me/old.txt")?;   // Delete
//! println!("trashed at {:?}", item.deleted_at());
//! item.restore()?;                                    // Undo (Ctrl+Z)
//! # Ok::<(), fm_trash::Error>(())
//! ```
//!
//! [freedesktop.org Trash specification 1.0]: https://specifications.freedesktop.org/trash/latest/
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod encode;
mod error;
mod info;
mod item;
#[cfg(target_os = "linux")]
mod mounts;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod imp;
#[cfg(not(target_os = "linux"))]
#[path = "unsupported.rs"]
mod imp;

use std::path::Path;

pub use error::Error;
pub use imp::Trash;
pub use item::TrashItem;

/// Move `path` to the trash (uses `$XDG_DATA_HOME/Trash`, or a per-volume trash).
pub fn trash(path: impl AsRef<Path>) -> Result<TrashItem, Error> {
    Trash::new()?.trash(path)
}

/// Trash several paths. The outer `Result` fails only if the trash can't be located;
/// each path gets its own result, in input order, and one failure doesn't stop the rest.
pub fn trash_all<I, P>(paths: I) -> Result<Vec<Result<TrashItem, Error>>, Error>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    Ok(Trash::new()?.trash_all(paths))
}

/// List everything in the user's trash (home trash and all mounted volumes), newest first.
pub fn list() -> Result<Vec<TrashItem>, Error> {
    Trash::new()?.list()
}

/// Permanently delete everything in the trash. Returns the number of items removed.
pub fn empty() -> Result<usize, Error> {
    Trash::new()?.empty()
}
