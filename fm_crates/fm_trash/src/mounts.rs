//! Enumerate mounted volumes from `/proc/self/mountinfo` (needed to find per-volume trashes).

use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

pub(crate) struct Mount {
    pub point: PathBuf,
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

pub(crate) fn parse(text: &str) -> Vec<Mount> {
    let mut v = Vec::new();
    for line in text.lines() {
        let Some((pre, post)) = line.split_once(" - ") else {
            continue;
        };
        let Some(point) = pre.split(' ').nth(4) else {
            continue;
        };
        let fstype = post.split(' ').next().unwrap_or("");
        if PSEUDO.contains(&fstype) {
            continue;
        }
        v.push(Mount {
            point: PathBuf::from(OsString::from_vec(unescape(point))),
        });
    }
    v
}

pub(crate) fn read() -> Vec<Mount> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| parse(&t))
        .unwrap_or_default()
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
}
