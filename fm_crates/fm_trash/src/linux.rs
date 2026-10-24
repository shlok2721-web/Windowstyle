//! Linux implementation of the freedesktop.org Trash spec 1.0.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs::{self, DirBuilder, OpenOptions, Permissions, ReadDir},
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

use chrono::{Local, NaiveDateTime, TimeZone};
use rustix::{
    fs::{CWD, RenameFlags, renameat_with},
    io::Errno,
};

use crate::{Error, TrashItem, encode, info, mounts};

/// Max bytes of the original name kept in the trash (leaves room for ".N" and ".trashinfo").
const MAX_NAME_BYTES: usize = 230;
const MAX_NAME_TRIES: u32 = 100_000;
const MAX_TEMP_TRIES: usize = 128;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Handle to the user's trash.
#[derive(Debug, Clone)]
pub struct Trash {
    home: PathBuf,
    uid: u32,
}

impl Trash {
    /// Locate the home trash: `$XDG_DATA_HOME/Trash`, or `~/.local/share/Trash`.
    pub fn new() -> Result<Self, Error> {
        let base = match std::env::var_os("XDG_DATA_HOME") {
            Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v),
            _ => {
                let home = std::env::var_os("HOME")
                    .filter(|h| !h.is_empty())
                    .ok_or(Error::NoHome)?;
                PathBuf::from(home).join(".local/share")
            }
        };
        Ok(Self::with_home_trash(base.join("Trash")))
    }

    /// Use an explicit home-trash directory (for tests or sandboxes).
    pub fn with_home_trash(dir: impl Into<PathBuf>) -> Self {
        Self {
            home: dir.into(),
            uid: rustix::process::getuid().as_raw(),
        }
    }

    /// The home trash directory in use.
    pub fn home_trash_dir(&self) -> &Path {
        &self.home
    }

    /// Move `path` to the trash. Symlinks are trashed as links (never followed).
    pub fn trash(&self, path: impl AsRef<Path>) -> Result<TrashItem, Error> {
        let path = path.as_ref();
        let abs = std::path::absolute(path).map_err(|e| Error::io(path, e))?;
        let name = abs
            .file_name()
            .ok_or_else(|| Error::InvalidPath(path.to_path_buf()))?
            .to_owned();
        let parent = abs
            .parent()
            .ok_or_else(|| Error::InvalidPath(path.to_path_buf()))?;
        let parent = fs::canonicalize(parent).map_err(|e| Error::io(parent, e))?;
        let target = parent.join(&name);

        let meta = fs::symlink_metadata(&target).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                Error::NotFound(target.clone())
            } else {
                Error::io(&target, e)
            }
        })?;
        let parent_meta = fs::metadata(&parent).map_err(|e| Error::io(&parent, e))?;
        if parent_meta.dev() != meta.dev() {
            return Err(Error::MountPoint(target));
        }

        // Device-number checks miss bind mounts on the same filesystem. Refuse both
        // mount-point roots and directories containing mounted descendants; otherwise
        // a cross-device move or later purge could traverse and delete mounted data.
        if meta.is_dir() {
            match mounts::relation(&target).map_err(|e| Error::io("/proc/self/mountinfo", e))? {
                Some(mounts::MountRelation::Exact) => return Err(Error::MountPoint(target)),
                Some(mounts::MountRelation::ContainsDescendant) => {
                    return Err(Error::ContainsMountPoint(target));
                }
                None => {}
            }
        }

        ensure_trash_layout(&self.home, self.uid).map_err(|e| Error::io(&self.home, e))?;
        let home = fs::canonicalize(&self.home).map_err(|e| Error::io(&self.home, e))?;
        if target.starts_with(&home) || home.starts_with(&target) {
            return Err(Error::OverlapsTrash(target));
        }
        let home_dev = fs::metadata(&home).map_err(|e| Error::io(&home, e))?.dev();

        let (trash_dir, topdir) = if meta.dev() == home_dev {
            (home, None)
        } else {
            let top = topdir_of(&target, meta.dev());
            if let Ok(rel) = target.strip_prefix(&top) {
                let first = rel.components().next().map(|c| c.as_os_str().to_owned());
                if first.as_deref() == Some(OsStr::new(".Trash"))
                    || first.as_deref()
                        == Some(OsString::from(format!(".Trash-{}", self.uid)).as_os_str())
                {
                    return Err(Error::OverlapsTrash(target));
                }
            }
            match volume_trash_dir(&top, self.uid) {
                Some(d) => (d, Some(top)),
                // Spec: if neither works, trash into the home trash (costly copy across devices).
                None => (home, None),
            }
        };

        let stored_path: &Path = match &topdir {
            Some(t) => target.strip_prefix(t).unwrap_or(&target),
            None => &target,
        };
        let encoded = encode::percent_encode(stored_path.as_os_str().as_bytes());
        let date = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let text = info::format(&encoded, &date);

        let is_home = topdir.is_none();
        let stored_name = claim_and_move(&trash_dir, &target, &name, &text)?;
        Ok(TrashItem {
            trash_dir,
            stored_name,
            original_path: target,
            deleted_at: Some(SystemTime::now()),
            is_home,
        })
    }

    /// Trash several paths; one result per input, in order.
    pub fn trash_all<I, P>(&self, paths: I) -> Vec<Result<TrashItem, Error>>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        paths.into_iter().map(|p| self.trash(p)).collect()
    }

    /// List all trashed items (home trash + `.Trash/$uid` + `.Trash-$uid` of every mount), newest first.
    pub fn list(&self) -> Result<Vec<TrashItem>, Error> {
        validate_existing_home_layout(&self.home, self.uid)
            .map_err(|e| Error::io(&self.home, e))?;
        let mut items = Vec::new();
        for (dir, top) in self.all_trash_dirs() {
            items.extend(read_items(&dir, top.as_deref(), self.uid));
        }
        items.sort_by(|a, b| {
            b.deleted_at
                .cmp(&a.deleted_at)
                .then_with(|| a.stored_name.cmp(&b.stored_name))
        });
        Ok(items)
    }

    /// Permanently delete everything in every trash directory. Keeps going after
    /// errors and returns the first one at the end; `Ok(n)` is the number of items removed.
    pub fn empty(&self) -> Result<usize, Error> {
        validate_existing_home_layout(&self.home, self.uid)
            .map_err(|e| Error::io(&self.home, e))?;
        let mut count = 0;
        let mut first_err: Option<Error> = None;
        let mount_points = match mounts::read_all_points() {
            Ok(points) => Some(points),
            Err(err) => {
                first_err = Some(Error::io("/proc/self/mountinfo", err));
                None
            }
        };
        for (dir, top) in self.all_trash_dirs() {
            let files_dir = dir.join("files");
            let info_dir = dir.join("info");
            let mut files_dir_safe = true;
            // Preflight metadata before deleting anything; later cleanup revalidates it.
            let info_dir_safe = match validate_trash_subdir(&info_dir, self.uid) {
                Ok(exists) => exists,
                Err(err) => {
                    first_err.get_or_insert(Error::io(&info_dir, err));
                    false
                }
            };

            // Remove payload first. Its record remains while the payload still exists,
            // including when removal fails part-way through a directory tree.
            match read_trash_subdir(&files_dir, self.uid) {
                Ok(Some(rd)) => {
                    for entry in rd {
                        let entry = match entry {
                            Ok(e) => e,
                            Err(err) => {
                                files_dir_safe = false;
                                first_err.get_or_insert(Error::io(&files_dir, err));
                                continue;
                            }
                        };
                        let data_path = entry.path();
                        match remove_any_with_snapshot(&data_path, mount_points.as_deref()) {
                            Ok(()) => count += 1,
                            Err(err) => {
                                first_err.get_or_insert(Error::io(&data_path, err));
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    files_dir_safe = false;
                    first_err.get_or_insert(Error::io(&files_dir, err));
                }
            }

            // Delete only orphaned metadata after revalidating both directory roots.
            // Any record whose data still exists is retained for recovery/listing.
            let files_root_safe = match validate_trash_subdir(&files_dir, self.uid) {
                Ok(_) => true,
                Err(err) => {
                    first_err.get_or_insert(Error::io(&files_dir, err));
                    false
                }
            };
            if files_dir_safe && files_root_safe && info_dir_safe {
                match read_trash_subdir(&info_dir, self.uid) {
                    Ok(Some(rd)) => {
                        for entry in rd {
                            let entry = match entry {
                                Ok(e) => e,
                                Err(err) => {
                                    first_err.get_or_insert(Error::io(&info_dir, err));
                                    continue;
                                }
                            };
                            let record_path = entry.path();
                            let file_name = entry.file_name();
                            if let Some(stem) = file_name.as_bytes().strip_suffix(b".trashinfo") {
                                if valid_stored_name_bytes(stem) {
                                    let data_path =
                                        files_dir.join(OsString::from_vec(stem.to_vec()));
                                    match fs::symlink_metadata(&data_path) {
                                        Ok(_) => continue,
                                        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                                        Err(err) => {
                                            first_err.get_or_insert(Error::io(&data_path, err));
                                            continue;
                                        }
                                    }
                                }
                            }
                            remove_optional_file(&record_path, &mut first_err);
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        first_err.get_or_insert(Error::io(&info_dir, err));
                    }
                }
            }
            let _ = fs::remove_file(dir.join("directorysizes"));
            if top.is_some() {
                remove_volume_trash_if_empty(&dir);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(count),
        }
    }

    fn all_trash_dirs(&self) -> Vec<(PathBuf, Option<PathBuf>)> {
        let mut seen = HashSet::new();
        let mut v = Vec::new();
        let mut add = |dir: PathBuf, top: Option<PathBuf>| {
            if let Ok(m) = fs::symlink_metadata(&dir) {
                if m.is_dir()
                    && !m.file_type().is_symlink()
                    && m.uid() == self.uid
                    && safe_optional_subdir(&dir.join("files"), self.uid)
                    && safe_optional_subdir(&dir.join("info"), self.uid)
                    && seen.insert((m.dev(), m.ino()))
                {
                    v.push((dir, top));
                }
            }
        };
        add(self.home.clone(), None);
        for m in mounts::read() {
            if shared_trash_ok(&m.point) {
                add(
                    m.point.join(".Trash").join(self.uid.to_string()),
                    Some(m.point.clone()),
                );
            }
            let own = m.point.join(format!(".Trash-{}", self.uid));
            if fs::symlink_metadata(&own)
                .map(|md| md.is_dir())
                .unwrap_or(false)
            {
                add(own, Some(m.point.clone()));
            }
        }
        v
    }
}

// ---------------------------------------------------------------------------
// restore / purge
// ---------------------------------------------------------------------------

fn valid_stored_name(item: &TrashItem) -> bool {
    valid_stored_name_bytes(item.stored_name.as_bytes())
}

fn valid_stored_name_bytes(n: &[u8]) -> bool {
    !n.is_empty() && !n.contains(&b'/') && n != b"." && n != b".."
}

/// Refuse to operate through a replaced trash directory or symlinked `files`/`info`.
fn validate_item_layout(item: &TrashItem) -> Result<(), Error> {
    let files = item.trash_dir.join("files");
    let info = item.trash_dir.join("info");
    let uid = rustix::process::getuid().as_raw();
    validate_owned_real_dir(&item.trash_dir, uid).map_err(|e| Error::io(&item.trash_dir, e))?;
    for path in [&files, &info] {
        validate_owned_real_dir(path, uid).map_err(|e| Error::io(path, e))?;
        let canonical = fs::canonicalize(path).map_err(|e| Error::io(path, e))?;
        match mounts::relation(&canonical).map_err(|e| Error::io("/proc/self/mountinfo", e))? {
            Some(mounts::MountRelation::Exact) => {
                return Err(Error::io(
                    path,
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "refusing to operate through a mounted trash subdirectory",
                    ),
                ));
            }
            Some(mounts::MountRelation::ContainsDescendant) | None => {}
        }
    }
    Ok(())
}

pub(crate) fn restore(item: &TrashItem) -> Result<PathBuf, Error> {
    if !valid_stored_name(item) {
        return Err(Error::InvalidItem(item.info_path()));
    }
    validate_item_layout(item)?;
    let from = item.trashed_path();
    let to = &item.original_path;
    let from_meta = match fs::symlink_metadata(&from) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::NotFound(from)),
        Err(e) => return Err(Error::io(&from, e)),
    };
    if from_meta.is_dir() {
        let canonical = fs::canonicalize(&from).map_err(|e| Error::io(&from, e))?;
        match mounts::relation(&canonical).map_err(|e| Error::io("/proc/self/mountinfo", e))? {
            Some(mounts::MountRelation::Exact) => return Err(Error::MountPoint(from)),
            Some(mounts::MountRelation::ContainsDescendant) => {
                return Err(Error::ContainsMountPoint(from));
            }
            None => {}
        }
    }
    match fs::symlink_metadata(to) {
        Ok(_) => return Err(Error::AlreadyExists(to.clone())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(to, e)),
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    match move_noreplace(&from, to) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(Error::AlreadyExists(to.clone()));
        }
        Err(e) => return Err(Error::io(to, e)),
    }
    let _ = fs::remove_file(item.info_path());
    forget_dirsize(&item.trash_dir, &item.stored_name);
    Ok(to.clone())
}

pub(crate) fn purge(item: &TrashItem) -> Result<(), Error> {
    if !valid_stored_name(item) {
        return Err(Error::InvalidItem(item.info_path()));
    }
    validate_item_layout(item)?;
    let data = item.trashed_path();
    remove_any(&data).map_err(|e| Error::io(&data, e))?;
    match fs::remove_file(item.info_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(item.info_path(), e)),
    }
    forget_dirsize(&item.trash_dir, &item.stored_name);
    if !item.is_home {
        remove_volume_trash_if_empty(&item.trash_dir);
    }
    Ok(())
}

/// After the last item is gone from a per-volume trash can (`<mount>/.Trash-$uid` or
/// `<mount>/.Trash/$uid`), remove the now-useless scaffold directory so an emptied
/// removable drive is not left with a stray `.Trash-*` folder forever. Best effort and
/// conservative: anything that doesn't look exactly like an empty, plain, own-uid trash
/// can (still has entries, is a symlink, belongs to someone else, a read failed, ...) is
/// left alone rather than risk deleting something else. Never called for the home trash.
fn remove_volume_trash_if_empty(trash_dir: &Path) {
    let uid = rustix::process::getuid().as_raw();
    let is_own_plain_dir = |path: &Path| -> Option<bool> {
        match fs::symlink_metadata(path) {
            Ok(m) => Some(m.is_dir() && !m.file_type().is_symlink() && m.uid() == uid),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(_) => Some(false),
        }
    };
    match is_own_plain_dir(trash_dir) {
        Some(true) => {}
        _ => return,
    }
    for sub in ["files", "info"] {
        let path = trash_dir.join(sub);
        match is_own_plain_dir(&path) {
            None => continue, // already absent
            Some(false) => return,
            Some(true) => match fs::read_dir(&path) {
                Ok(mut rd) => if rd.next().is_none() {},
                _ => return, // still has entries, or could not be read: leave it
            },
        }
    }
    let _ = fs::remove_dir_all(trash_dir);
}

/// Best effort: drop this name from the optional `directorysizes` cache.
fn forget_dirsize(trash_dir: &Path, stored_name: &OsStr) {
    let path = trash_dir.join("directorysizes");
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
        _ => return,
    }
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let enc = encode::percent_encode(stored_name.as_bytes());
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| l.rsplit(' ').next() != Some(enc.as_str()))
        .collect();
    if kept.len() == text.lines().count() {
        return;
    }
    let mut body = kept.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }

    for _ in 0..MAX_TEMP_TRIES {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = trash_dir.join(format!(
            ".fm-trash-directorysizes-{}-{id}",
            std::process::id()
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return,
        };
        if file.write_all(body.as_bytes()).is_err() {
            drop(file);
            let _ = fs::remove_file(&tmp);
            return;
        }
        drop(file);
        if fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
        return;
    }
}

fn remove_optional_file(path: &Path, first_err: &mut Option<Error>) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            first_err.get_or_insert(Error::io(path, err));
        }
    }
}

// ---------------------------------------------------------------------------
// reading a trash directory
// ---------------------------------------------------------------------------

fn read_items(trash_dir: &Path, topdir: Option<&Path>, uid: u32) -> Vec<TrashItem> {
    // Validate both access roots before using paths below them. In particular, don't
    // interpret a mounted `files/` directory as this trash's payload.
    if validate_trash_subdir(&trash_dir.join("files"), uid).is_err() {
        return Vec::new();
    }
    let Ok(Some(rd)) = read_trash_subdir(&trash_dir.join("info"), uid) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let fname = e.file_name();
        let Some(stem) = fname.as_bytes().strip_suffix(b".trashinfo") else {
            continue;
        };
        if !valid_stored_name_bytes(stem) {
            continue;
        }
        let stored_name = OsString::from_vec(stem.to_vec());
        match fs::symlink_metadata(trash_dir.join("files").join(&stored_name)) {
            Ok(_) => {}
            Err(_) => continue, // no accessible data for this record
        }
        let Ok(text) = fs::read_to_string(e.path()) else {
            continue;
        };
        let Some(parsed) = info::parse(&text) else {
            continue;
        };

        let mut original = PathBuf::from(OsString::from_vec(parsed.path));
        if original.is_relative() {
            if original
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                continue; // spec: relative paths MUST NOT contain ".."
            }
            match topdir.or(trash_dir.parent()) {
                Some(base) => original = base.join(original),
                None => continue,
            }
        }
        let deleted_at = parsed.deletion_date.as_deref().and_then(parse_local_date);
        out.push(TrashItem {
            trash_dir: trash_dir.to_path_buf(),
            stored_name,
            original_path: original,
            deleted_at,
            is_home: topdir.is_none(),
        });
    }
    out
}

fn parse_local_date(s: &str) -> Option<SystemTime> {
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok()?;
    let local = Local.from_local_datetime(&naive).earliest()?;
    Some(SystemTime::from(local))
}

// ---------------------------------------------------------------------------
// trash directory selection
// ---------------------------------------------------------------------------

fn ensure_trash_layout(path: &Path, uid: u32) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            DirBuilder::new().recursive(true).mode(0o700).create(path)?;
        }
        Err(e) => return Err(e),
    }
    ensure_private_owned_dir(path, uid)?;
    ensure_private_trash_subdir(&path.join("files"), uid)?;
    ensure_private_trash_subdir(&path.join("info"), uid)
}

/// Trash data and metadata directories must not themselves be mount points.
fn ensure_private_trash_subdir(path: &Path, uid: u32) -> io::Result<()> {
    // Validate before changing permissions, then repeat after creation to narrow races.
    validate_trash_subdir(path, uid)?;
    ensure_private_owned_dir(path, uid)?;
    validate_trash_subdir(path, uid)?;
    Ok(())
}

/// Verify a trash directory without following its final path component. Existing
/// directories are made private only when they are owned by this user.
fn ensure_private_owned_dir(path: &Path, uid: u32) -> io::Result<()> {
    for _ in 0..2 {
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                if !meta.is_dir() || meta.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "trash path is not a real directory",
                    ));
                }
                if meta.uid() != uid {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "trash directory is not owned by the current user",
                    ));
                }
                if meta.mode() & 0o7777 != 0o700 {
                    fs::set_permissions(path, Permissions::from_mode(0o700))?;
                }
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                match DirBuilder::new().mode(0o700).create(path) {
                    Ok(()) => continue,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(
        "trash directory changed during validation",
    ))
}

fn validate_owned_real_dir(path: &Path, uid: u32) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trash path is not a real directory",
        ));
    }
    if meta.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trash directory is not owned by the current user",
        ));
    }
    Ok(())
}

fn safe_optional_subdir(path: &Path, uid: u32) -> bool {
    match fs::symlink_metadata(path) {
        Ok(meta) => meta.is_dir() && !meta.file_type().is_symlink() && meta.uid() == uid,
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn validate_existing_home_layout(path: &Path, uid: u32) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() && meta.uid() == uid => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "home trash is not a real directory owned by the current user",
            ));
        }
    }
    for sub in ["files", "info"] {
        let child = path.join(sub);
        match fs::symlink_metadata(&child) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() && meta.uid() == uid => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("home trash {sub}/ is not a real directory owned by the current user"),
                ));
            }
        }
    }
    Ok(())
}

/// Open an optional trash subdirectory only if it is a real directory owned by the
/// current user. Never follow a replaced `files` or `info` symlink during list/empty.
fn validate_trash_subdir(path: &Path, uid: u32) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() && meta.uid() == uid => {
            let canonical = fs::canonicalize(path)?;
            if matches!(
                mounts::relation(&canonical)?,
                Some(mounts::MountRelation::Exact)
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing to traverse a mounted trash subdirectory",
                ));
            }
            Ok(true)
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to traverse an unsafe trash subdirectory",
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn read_trash_subdir(path: &Path, uid: u32) -> io::Result<Option<ReadDir>> {
    if validate_trash_subdir(path, uid)? {
        fs::read_dir(path).map(Some)
    } else {
        Ok(None)
    }
}

/// `$topdir/.Trash` must be a real directory (not a symlink) with the sticky bit.
fn shared_trash_ok(top: &Path) -> bool {
    match fs::symlink_metadata(top.join(".Trash")) {
        Ok(m) => m.is_dir() && !m.file_type().is_symlink() && m.permissions().mode() & 0o1000 != 0,
        Err(_) => false,
    }
}

/// Create (if needed) a private trash dir owned by us, with `files` and `info` inside.
fn own_trash_dir(p: &Path, uid: u32) -> bool {
    match DirBuilder::new().mode(0o700).create(p) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return false,
    }
    ensure_private_owned_dir(p, uid).is_ok()
        && ensure_private_trash_subdir(&p.join("files"), uid).is_ok()
        && ensure_private_trash_subdir(&p.join("info"), uid).is_ok()
}

/// Spec section 5: method (1) `.Trash/$uid`, falling back to (2) `.Trash-$uid`.
fn volume_trash_dir(top: &Path, uid: u32) -> Option<PathBuf> {
    if shared_trash_ok(top) {
        let d = top.join(".Trash").join(uid.to_string());
        if own_trash_dir(&d, uid) {
            return Some(d);
        }
    }
    let d = top.join(format!(".Trash-{uid}"));
    own_trash_dir(&d, uid).then_some(d)
}

/// Highest ancestor on the same device, i.e. the mount point of `target`'s filesystem.
fn topdir_of(target: &Path, dev: u64) -> PathBuf {
    let mut top = target.parent().unwrap_or(target).to_path_buf();
    for anc in target.ancestors().skip(1) {
        match fs::metadata(anc) {
            Ok(m) if m.dev() == dev => top = anc.to_path_buf(),
            _ => break,
        }
    }
    top
}

// ---------------------------------------------------------------------------
// moving files around
// ---------------------------------------------------------------------------

fn candidate_name(base: &OsStr, n: u32) -> OsString {
    let mut b = base.as_bytes();
    if b.len() > MAX_NAME_BYTES {
        b = &b[..MAX_NAME_BYTES];
    }
    if n == 1 {
        return OsString::from_vec(b.to_vec());
    }
    let (stem, ext) = match b.iter().rposition(|&c| c == b'.') {
        Some(i) if i > 0 => (&b[..i], &b[i..]),
        _ => (b, &b[b.len()..]),
    };
    let mut v = stem.to_vec();
    v.extend_from_slice(format!(".{n}").as_bytes());
    v.extend_from_slice(ext);
    OsString::from_vec(v)
}

/// Spec: create the `.trashinfo` first, atomically (O_EXCL); that claims the name.
fn claim_and_move(
    trash_dir: &Path,
    from: &Path,
    base: &OsStr,
    info_text: &str,
) -> Result<OsString, Error> {
    for n in 1..=MAX_NAME_TRIES {
        let stored = candidate_name(base, n);
        let mut info_name = stored.clone();
        info_name.push(".trashinfo");
        let info_path = trash_dir.join("info").join(info_name);

        let mut f = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&info_path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::io(info_path, e)),
        };
        if let Err(e) = f.write_all(info_text.as_bytes()) {
            drop(f);
            let _ = fs::remove_file(&info_path);
            return Err(Error::io(info_path, e));
        }
        drop(f);

        let dest = trash_dir.join("files").join(&stored);
        if fs::symlink_metadata(&dest).is_ok() {
            let _ = fs::remove_file(&info_path); // orphan data with this name
            continue;
        }
        match move_noreplace(from, &dest) {
            Ok(()) => return Ok(stored),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&info_path);
            }
            Err(e) => {
                // A cross-filesystem move can finish copying but fail while removing
                // the source. Keep the record whenever destination state is uncertain,
                // so a complete copy is not left invisible in files/.
                if matches!(
                    fs::symlink_metadata(&dest),
                    Err(ref check) if check.kind() == io::ErrorKind::NotFound
                ) {
                    let _ = fs::remove_file(&info_path);
                }
                return Err(Error::io(from, e));
            }
        }
    }
    Err(Error::io(
        trash_dir,
        io::Error::new(io::ErrorKind::AlreadyExists, "too many name collisions"),
    ))
}

/// `rename(2)` that refuses to overwrite; falls back to copy+delete across filesystems.
fn move_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    match rename_noreplace(from, to) {
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            // Copying is itself no-clobber. If source removal fails after a complete
            // copy, retain the destination rather than risking loss of the only full
            // copy (directory removal may already have partially progressed).
            copy_recursive(from, to)?;
            remove_any(from)
        }
        other => other,
    }
}

fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        // Never fall back to check-then-rename: a concurrent creator could win
        // between the check and rename, causing fs::rename to overwrite their data.
        Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem does not support atomic no-replace rename",
        )),
        Err(e) => Err(e.into()),
    }
}

/// Copy a file, symlink or directory tree without overwriting any destination.
/// Directory traversal is iterative to avoid stack overflow on deep trees. Directory
/// copies are staged beside the final path and atomically published only when complete,
/// so a concurrent list/restore never observes a half-copied top-level directory.
fn copy_recursive(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        return std::os::unix::fs::symlink(fs::read_link(from)?, to);
    }
    if ft.is_file() {
        return copy_file_noreplace(from, to, meta.mode() & 0o7777);
    }
    if !ft.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot copy special file (socket, fifo, device)",
        ));
    }
    let canonical_source = fs::canonicalize(from)?;
    match mounts::relation(&canonical_source)? {
        Some(mounts::MountRelation::Exact) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to copy a mount point",
            ));
        }
        Some(mounts::MountRelation::ContainsDescendant) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to copy a directory tree containing a mount point",
            ));
        }
        None => {}
    }

    match fs::symlink_metadata(to) {
        Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let parent = to.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        )
    })?;
    let temporary = create_temp_dir(parent, to)?;
    match copy_directory_contents(from, &temporary, meta.mode() & 0o7777) {
        Ok(()) => match rename_noreplace(&temporary, to) {
            Ok(()) => Ok(()),
            Err(err) => Err(cleanup_partial_copy(&temporary, err)),
        },
        Err(err) => Err(cleanup_partial_copy(&temporary, err)),
    }
}

fn create_temp_dir(parent: &Path, final_path: &Path) -> io::Result<PathBuf> {
    for _ in 0..MAX_TEMP_TRIES {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".fm-trash-dir-copy-{}-{id}", std::process::id()));
        if temporary.as_path() == final_path {
            continue;
        }
        match DirBuilder::new().mode(0o700).create(&temporary) {
            Ok(()) => {
                if let Err(err) = fs::set_permissions(&temporary, Permissions::from_mode(0o700)) {
                    let _ = fs::remove_dir(&temporary);
                    return Err(err);
                }
                return Ok(temporary);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary directory name",
    ))
}

fn copy_directory_contents(from: &Path, staging: &Path, root_mode: u32) -> io::Result<()> {
    struct DirectoryTask {
        source: PathBuf,
        destination: PathBuf,
        mode: u32,
    }

    // Process each directory's entries completely before descending into child
    // directories. This keeps only one ReadDir alive at a time, avoids recursive
    // call-stack growth, and does not allocate a name list for large flat folders.
    let mut pending = vec![DirectoryTask {
        source: from.to_path_buf(),
        destination: staging.to_path_buf(),
        mode: root_mode,
    }];
    let mut restore_modes: Vec<(PathBuf, u32)> = Vec::new();

    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory.source)? {
            let entry = entry?;
            let source = entry.path();
            let destination = directory.destination.join(entry.file_name());
            let child_meta = fs::symlink_metadata(&source)?;
            let child_type = child_meta.file_type();
            if child_type.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(&source)?, &destination)?;
            } else if child_type.is_file() {
                copy_file_noreplace(&source, &destination, child_meta.mode() & 0o7777)?;
            } else if child_type.is_dir() {
                DirBuilder::new().mode(0o700).create(&destination)?;
                fs::set_permissions(&destination, Permissions::from_mode(0o700))?;
                pending.push(DirectoryTask {
                    source,
                    destination,
                    mode: child_meta.mode() & 0o7777,
                });
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "cannot copy special file (socket, fifo, device)",
                ));
            }
        }
        // Keep directories writable until all descendants have been copied. Modes
        // are restored deepest-first after traversal so parent mode 000 is safe too.
        restore_modes.push((directory.destination, directory.mode));
    }

    for (path, mode) in restore_modes.into_iter().rev() {
        fs::set_permissions(path, Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Use a temporary sibling plus atomic no-replace rename so the data copy never
/// truncates a destination created by another operation. `fs::copy` retains its
/// platform-optimized copy path for large files.
fn copy_file_noreplace(from: &Path, to: &Path, mode: u32) -> io::Result<()> {
    let parent = to.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        )
    })?;

    // For a shared/writable destination parent, put the temporary inside a newly
    // created 0700 directory. This prevents another local user from replacing the
    // reserved temporary path with a symlink while fs::copy opens it by name. Inside
    // our own staged directory tree the parent is already private, so avoid an extra
    // mkdir/rmdir pair for every copied child file.
    let private_parent = fs::symlink_metadata(parent)
        .map(|meta| {
            meta.is_dir()
                && !meta.file_type().is_symlink()
                && meta.uid() == rustix::process::getuid().as_raw()
                && meta.mode() & 0o777 == 0o700
        })
        .unwrap_or(false);
    let staging_dir = if private_parent {
        None
    } else {
        Some(create_temp_dir(parent, to)?)
    };
    let temporary_parent = staging_dir.as_deref().unwrap_or(parent);

    for _ in 0..MAX_TEMP_TRIES {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = if staging_dir.is_some() {
            temporary_parent.join("data")
        } else {
            temporary_parent.join(format!(".fm-trash-copy-{}-{id}", std::process::id()))
        };
        if temporary.as_path() == to {
            continue;
        }
        let reserved = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists && staging_dir.is_none() => {
                continue;
            }
            Err(err) => {
                if let Some(stage) = &staging_dir {
                    let _ = fs::remove_dir(stage);
                }
                return Err(err);
            }
        };
        drop(reserved);
        if let Err(err) = fs::set_permissions(&temporary, Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&temporary);
            if let Some(stage) = &staging_dir {
                let _ = fs::remove_dir(stage);
            }
            return Err(err);
        }

        let result = (|| {
            fs::copy(from, &temporary)?;
            fs::set_permissions(&temporary, Permissions::from_mode(mode))?;
            rename_noreplace(&temporary, to)
        })();
        match result {
            Ok(()) => {
                if let Some(stage) = &staging_dir {
                    fs::remove_dir(stage)?;
                }
                return Ok(());
            }
            Err(err) => {
                let file_cleanup = fs::remove_file(&temporary);
                let stage_cleanup = staging_dir.as_ref().map(fs::remove_dir).transpose();
                let file_cleanup_error = file_cleanup
                    .err()
                    .filter(|e| e.kind() != io::ErrorKind::NotFound);
                let stage_cleanup_error = stage_cleanup.err();
                if let Some(cleanup) = file_cleanup_error.or(stage_cleanup_error) {
                    return Err(io::Error::other(format!(
                        "{err}; failed to clean temporary copy near {}: {cleanup}",
                        to.display()
                    )));
                }
                return Err(err);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary copy name",
    ))
}

fn cleanup_partial_copy(path: &Path, cause: io::Error) -> io::Error {
    match remove_any(path) {
        Ok(()) => cause,
        Err(cleanup) => io::Error::other(format!(
            "{cause}; failed to remove partial copy {}: {cleanup}",
            path.display()
        )),
    }
}

/// Remove a file, symlink or whole directory tree (never follows symlinks).
fn remove_any(p: &Path) -> io::Result<()> {
    remove_any_with_snapshot(p, None)
}

/// Remove with a shared mountinfo snapshot when processing many entries (e.g. empty()).
/// Callers pass only mount points that were successfully read; `None` asks for a fresh
/// snapshot and fails closed if mountinfo cannot be read.
fn remove_any_with_snapshot(p: &Path, snapshot: Option<&[PathBuf]>) -> io::Result<()> {
    let meta = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return fs::remove_file(p);
    }

    let canonical = fs::canonicalize(p)?;
    let relation = match snapshot {
        Some(points) => {
            mounts::relation_from_points(&canonical, points.iter().map(PathBuf::as_path))
        }
        None => mounts::relation(&canonical)?,
    };
    match relation {
        Some(mounts::MountRelation::Exact) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to remove a mount point",
            ));
        }
        Some(mounts::MountRelation::ContainsDescendant) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to remove a directory tree containing a mount point",
            ));
        }
        None => {}
    }
    match fs::remove_dir_all(p) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            make_dirs_writable(p);
            fs::remove_dir_all(p)
        }
        other => other,
    }
}

fn make_dirs_writable(p: &Path) {
    let mut pending = vec![p.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        let _ = fs::set_permissions(&path, Permissions::from_mode(meta.mode() & 0o7777 | 0o700));
        if let Ok(rd) = fs::read_dir(&path) {
            for entry in rd.flatten() {
                pending.push(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_file_never_overwrites_an_existing_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.txt");
        let destination = tmp.path().join("destination.txt");
        fs::write(&source, b"new data").unwrap();
        fs::write(&destination, b"existing data").unwrap();

        let err = copy_file_noreplace(&source, &destination, 0o600).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&destination).unwrap(), b"existing data");
    }

    #[test]
    fn recursive_copy_never_overwrites_an_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("new.txt"), b"new").unwrap();
        fs::write(destination.join("old.txt"), b"old").unwrap();

        let err = copy_recursive(&source, &destination).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(!destination.join("new.txt").exists());
        assert_eq!(fs::read(destination.join("old.txt")).unwrap(), b"old");
    }

    #[test]
    fn recursive_copy_handles_deep_directory_trees_iteratively() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        let mut nested = source.clone();
        for _ in 0..100 {
            nested.push("d");
        }
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("leaf.txt"), b"leaf").unwrap();

        copy_recursive(&source, &destination).unwrap();
        let mut copied_nested = destination;
        for _ in 0..100 {
            copied_nested.push("d");
        }
        assert_eq!(fs::read(copied_nested.join("leaf.txt")).unwrap(), b"leaf");
    }

    #[test]
    fn trash_layout_rejects_symlinked_data_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = tmp.path().join("Trash");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&trash).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, trash.join("files")).unwrap();

        let err = ensure_trash_layout(&trash, rustix::process::getuid().as_raw()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(outside.is_dir());
    }

    #[test]
    fn remove_refuses_a_directory_tree_with_a_mount_point_descendant() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("tree");
        let nested_mount = tree.join("mounted");
        fs::create_dir_all(&nested_mount).unwrap();
        let payload = nested_mount.join("keep.txt");
        fs::write(&payload, b"keep").unwrap();

        // A synthetic mountinfo snapshot makes this deterministic and does not need
        // mount privileges on the test machine.
        let points = vec![nested_mount];
        let err = remove_any_with_snapshot(&tree, Some(&points)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(payload).unwrap(), b"keep");
    }
}
