//! Enumerate mounted volumes from `/proc/self/mountinfo` (needed to find per-volume trashes).

use std::{
    ffi::OsString,
    io,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

pub(crate) struct Mount {
    pub point: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MountRelation {
    Exact,
    ContainsDescendant,
}

/// Filesystems that never hold user files.
const PSEUDO: &[&str] = &[
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devpts",
    "devtmpfs",
    "securityfs",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "pstore",
    "bpf",
    "mqueue",
    "hugetlbfs",
    "autofs",
    "binfmt_misc",
    "selinuxfs",
    "efivarfs",
    "rpc_pipefs",
    "nsfs",
];

fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = (oct[0] - b'0') as u32 * 64
                    + (oct[1] - b'0') as u32 * 8
                    + (oct[2] - b'0') as u32;
                if v <= 255 {
                    out.push(v as u8);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn parse_all(text: &str) -> Vec<(PathBuf, &str)> {
    let mut v = Vec::new();
    for line in text.lines() {
        let Some((pre, post)) = line.split_once(" - ") else {
            continue;
        };
        let Some(point) = pre.split(' ').nth(4) else {
            continue;
        };
        let fstype = post.split(' ').next().unwrap_or("");
        v.push((PathBuf::from(OsString::from_vec(unescape(point))), fstype));
    }
    v
}

pub(crate) fn parse(text: &str) -> Vec<Mount> {
    let mut v = Vec::new();
    for (point, fstype) in parse_all(text) {
        if PSEUDO.contains(&fstype) {
            continue;
        }
        v.push(Mount { point });
    }
    v
}

pub(crate) fn read() -> Vec<Mount> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| parse(&t))
        .unwrap_or_default()
}

/// Find whether `path` is itself a mount point or contains one. Unlike `read()`, this
/// examines pseudo filesystems too: deleting a directory tree must never recurse into
/// a nested mount, regardless of its filesystem type.
pub(crate) fn relation(path: &Path) -> io::Result<Option<MountRelation>> {
    let points = read_all_points()?;
    Ok(relation_from_points(
        path,
        points.iter().map(PathBuf::as_path),
    ))
}

pub(crate) fn read_all_points() -> io::Result<Vec<PathBuf>> {
    let text = std::fs::read_to_string("/proc/self/mountinfo")?;
    Ok(parse_all(&text)
        .into_iter()
        .map(|(point, _)| point)
        .collect())
}

pub(crate) fn relation_from_points<'a>(
    path: &Path,
    points: impl IntoIterator<Item = &'a Path>,
) -> Option<MountRelation> {
    let mut contains_descendant = false;
    for point in points {
        if point == path {
            return Some(MountRelation::Exact);
        }
        if point.starts_with(path) {
            contains_descendant = true;
        }
    }
    contains_descendant.then_some(MountRelation::ContainsDescendant)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_filters() {
        let t = "22 1 8:2 / / rw - ext4 /dev/sda2 rw\n\
                 23 22 0:5 / /proc rw - proc proc rw\n\
                 24 22 8:3 / /media/my\\040usb rw - vfat /dev/sdb1 rw\n";
        let m = parse(t);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].point, PathBuf::from("/"));
        assert_eq!(m[1].point, PathBuf::from("/media/my usb"));
    }

    #[test]
    fn mount_relation_includes_pseudo_filesystems_and_is_component_aware() {
        let t = "22 1 8:2 / / rw - ext4 /dev/sda2 rw\n\
                 23 22 0:5 / /proc rw - proc proc rw\n\
                 24 22 8:3 / /media/my\\040usb rw - vfat /dev/sdb1 rw\n";
        let all = parse_all(t);
        let points: Vec<_> = all.iter().map(|(p, _)| p.as_path()).collect();
        assert!(points.contains(&Path::new("/proc")));
        assert!(points.contains(&Path::new("/media/my usb")));
        assert!(!points.contains(&Path::new("/media/my")));

        let mount_points = [
            Path::new("/"),
            Path::new("/proc"),
            Path::new("/media/my usb"),
        ];
        assert_eq!(
            relation_from_points(Path::new("/proc"), mount_points),
            Some(MountRelation::Exact)
        );
        assert_eq!(
            relation_from_points(Path::new("/media"), mount_points),
            Some(MountRelation::ContainsDescendant)
        );
        assert_eq!(
            relation_from_points(Path::new("/media/my"), mount_points),
            None
        );
    }
}
