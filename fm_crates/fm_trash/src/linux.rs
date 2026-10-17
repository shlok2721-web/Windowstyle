//! Linux implementation of the freedesktop.org Trash spec 1.0.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs::{self, DirBuilder, OpenOptions, Permissions},
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
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

        mkdir_private(&self.home.join("files")).map_err(|e| Error::io(&self.home, e))?;
        mkdir_private(&self.home.join("info")).map_err(|e| Error::io(&self.home, e))?;
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

        let stored_name = claim_and_move(&trash_dir, &target, &name, &text)?;
        Ok(TrashItem {
            trash_dir,
            stored_name,
            original_path: target,
            deleted_at: Some(SystemTime::now()),
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
        let mut items = Vec::new();
        for (dir, top) in self.all_trash_dirs() {
            items.extend(read_items(&dir, top.as_deref()));
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
        let mut count = 0;
        let mut first_err: Option<Error> = None;
        for (dir, _) in self.all_trash_dirs() {
            for sub in ["files", "info"] {
                let Ok(rd) = fs::read_dir(dir.join(sub)) else {
                    continue;
                };
                for e in rd.flatten() {
                    let p = e.path();
                    let r = if sub == "files" {
                        remove_any(&p)
                    } else {
                        fs::remove_file(&p)
                    };
                    match r {
                        Ok(()) if sub == "files" => count += 1,
                        Ok(()) => {}
                        Err(err) => {
                            first_err.get_or_insert(Error::io(p, err));
                        }
                    }
                }
            }
            let _ = fs::remove_file(dir.join("directorysizes"));
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
            if let Ok(m) = fs::metadata(&dir) {
                if m.is_dir() && seen.insert((m.dev(), m.ino())) {
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
    let n = item.stored_name.as_bytes();
    !n.is_empty() && !n.contains(&b'/') && n != b"." && n != b".."
}

pub(crate) fn restore(item: &TrashItem) -> Result<PathBuf, Error> {
    if !valid_stored_name(item) {
        return Err(Error::InvalidItem(item.info_path()));
    }
    let from = item.trashed_path();
    let to = &item.original_path;
    if fs::symlink_metadata(&from).is_err() {
        return Err(Error::NotFound(from));
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
    let data = item.trashed_path();
    remove_any(&data).map_err(|e| Error::io(&data, e))?;
    match fs::remove_file(item.info_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(item.info_path(), e)),
    }
    forget_dirsize(&item.trash_dir, &item.stored_name);
    Ok(())
}

/// Best effort: drop this name from the optional `directorysizes` cache.
fn forget_dirsize(trash_dir: &Path, stored_name: &OsStr) {
    let path = trash_dir.join("directorysizes");
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
    let tmp = trash_dir.join(format!("directorysizes.tmp{}", std::process::id()));
    let mut body = kept.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    if fs::write(&tmp, body).is_ok() && fs::rename(&tmp, &path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

// ---------------------------------------------------------------------------
// reading a trash directory
// ---------------------------------------------------------------------------

fn read_items(trash_dir: &Path, topdir: Option<&Path>) -> Vec<TrashItem> {
    let Ok(rd) = fs::read_dir(trash_dir.join("info")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let fname = e.file_name();
        let Some(stem) = fname.as_bytes().strip_suffix(b".trashinfo") else {
            continue;
        };
        let stored_name = OsString::from_vec(stem.to_vec());
        if fs::symlink_metadata(trash_dir.join("files").join(&stored_name)).is_err() {
            continue; // info without data
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

fn mkdir_private(p: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(p)
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
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() && m.uid() == uid => {}
        _ => return false,
    }
    mkdir_private(&p.join("files")).is_ok() && mkdir_private(&p.join("info")).is_ok()
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
                let _ = fs::remove_file(&info_path);
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
            if fs::symlink_metadata(to).is_ok() {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            if let Err(e) = copy_recursive(from, to) {
                let _ = remove_any(to);
                return Err(e);
            }
            remove_any(from)
        }
        other => other,
    }
}

fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        // Filesystem without RENAME_NOREPLACE support.
        Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP) => {
            if fs::symlink_metadata(to).is_ok() {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(from, to)
        }
        Err(e) => Err(e.into()),
    }
}

/// Copy a file, symlink or directory tree. Permissions are applied after writing
/// (spec: a read-only dir must not block copying its own children).
fn copy_recursive(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else if ft.is_dir() {
        DirBuilder::new().mode(0o700).create(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
        fs::set_permissions(to, Permissions::from_mode(meta.mode() & 0o7777))
    } else if ft.is_file() {
        fs::copy(from, to).map(|_| ())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot copy special file (socket, fifo, device)",
        ))
    }
}

/// Remove a file, symlink or whole directory tree (never follows symlinks).
fn remove_any(p: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return fs::remove_file(p);
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
    let Ok(meta) = fs::symlink_metadata(p) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let _ = fs::set_permissions(p, Permissions::from_mode(meta.mode() & 0o7777 | 0o700));
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            make_dirs_writable(&e.path());
        }
    }
}
