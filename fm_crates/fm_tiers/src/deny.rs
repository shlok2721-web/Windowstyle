//! Default deny list: directories that are never cached or watched (always "cold").

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Directories excluded outright from caching and watching. Navigation into them still
/// works; their listings are simply read live every time and then dropped.
#[derive(Clone, Debug, Default)]
pub struct DenyList {
    names: HashSet<OsString>,
    suffixes: Vec<Vec<OsString>>,
    prefixes: Vec<PathBuf>,
}

impl DenyList {
    /// An empty list (nothing denied).
    pub fn empty() -> Self {
        Self::default()
    }

    /// The built-in rules: `.cache`, `node_modules`, `target`, `.git/objects`, `~/.var`,
    /// Steam libraries, `/proc`, `/sys` (and `/dev`, `/run` which are never worth caching).
    pub fn defaults(home: Option<&Path>) -> Self {
        let mut d = Self::empty();
        for n in [".cache", "node_modules", "target"] {
            d.deny_name(n);
        }
        d.deny_suffix(".git/objects");
        for p in ["/proc", "/sys", "/dev", "/run"] {
            d.deny_prefix(p);
        }
        if let Some(home) = home {
            d.deny_prefix(home.join(".var"));
            d.deny_prefix(home.join(".steam"));
            d.deny_prefix(home.join(".local/share/Steam"));
        }
        d
    }

    /// Deny any path that has a component with this exact name.
    pub fn deny_name(&mut self, name: impl Into<OsString>) {
        self.names.insert(name.into());
    }

    /// Deny any path containing this relative component sequence (e.g. `.git/objects`).
    pub fn deny_suffix(&mut self, rel: &str) {
        let seq: Vec<OsString> = rel
            .split('/')
            .filter(|s| !s.is_empty())
            .map(OsString::from)
            .collect();
        if !seq.is_empty() {
            self.suffixes.push(seq);
        }
    }

    /// Deny a directory and everything below it.
    pub fn deny_prefix(&mut self, prefix: impl Into<PathBuf>) {
        self.prefixes.push(prefix.into());
    }

    /// Add a user-editable rule: an absolute path is a prefix rule, a string containing `/`
    /// is a component-sequence rule, anything else is a component-name rule.
    pub fn add_rule(&mut self, rule: &str) {
        let rule = rule.trim();
        if rule.is_empty() {
            return;
        }
        if rule.starts_with('/') {
            self.deny_prefix(rule);
        } else if rule.contains('/') {
            self.deny_suffix(rule);
        } else {
            self.deny_name(rule);
        }
    }

    /// Whether `path` (or any ancestor of it) is denied.
    pub fn is_denied(&self, path: &Path) -> bool {
        if self.prefixes.iter().any(|p| path.starts_with(p)) {
            return true;
        }
        let comps: Vec<&std::ffi::OsStr> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n),
                _ => None,
            })
            .collect();
        if comps.iter().any(|c| self.names.contains(*c)) {
            return true;
        }
        self.suffixes.iter().any(|seq| {
            comps
                .windows(seq.len())
                .any(|w| w.iter().zip(seq).all(|(a, b)| *a == b.as_os_str()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_deny_the_noisy_places() {
        let d = DenyList::defaults(Some(Path::new("/home/u")));
        for p in [
            "/home/u/.cache",
            "/home/u/.cache/mozilla",
            "/home/u/proj/node_modules/x",
            "/home/u/proj/target",
            "/home/u/r/.git/objects/ab",
            "/proc/1",
            "/sys/class",
            "/home/u/.var/app/x",
            "/home/u/.local/share/Steam/steamapps",
        ] {
            assert!(d.is_denied(Path::new(p)), "{p} should be denied");
        }
        for p in [
            "/home/u",
            "/home/u/Documents",
            "/home/u/r/.git",
            "/home/u/targets",
            "/home/u/.local/bin",
        ] {
            assert!(!d.is_denied(Path::new(p)), "{p} should be allowed");
        }
    }

    #[test]
    fn user_rules_cover_all_three_shapes() {
        let mut d = DenyList::empty();
        d.add_rule("/mnt/slow");
        d.add_rule("build/out");
        d.add_rule("tmp");
        d.add_rule("   ");
        assert!(d.is_denied(Path::new("/mnt/slow/a")));
        assert!(d.is_denied(Path::new("/x/build/out/y")));
        assert!(d.is_denied(Path::new("/x/tmp")));
        assert!(!d.is_denied(Path::new("/x/build/in")));
    }
}
