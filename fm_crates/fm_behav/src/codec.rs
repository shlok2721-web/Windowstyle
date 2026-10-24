//! Deterministic, line-based text encoding helpers. Paths are escaped so that no path can
//! contain a tab or newline, and so that non-UTF-8 paths survive a round trip.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

#[cfg(unix)]
fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    Cow::Borrowed(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
    Cow::Owned(path.to_string_lossy().into_owned().into_bytes())
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

fn push_percent(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(char::from(HEX[usize::from(byte >> 4)]));
    out.push(char::from(HEX[usize::from(byte & 0x0f)]));
}

/// Escape a path: `%`, control characters and (for non-UTF-8 paths) every non-ASCII byte
/// become `%XX`. Valid UTF-8 text is kept readable.
pub(crate) fn escape(path: &Path) -> String {
    let mut out = String::with_capacity(path.as_os_str().len());
    match path.to_str() {
        Some(text) => {
            for c in text.chars() {
                if c == '%' || c.is_control() {
                    let mut buf = [0_u8; 4];
                    for &b in c.encode_utf8(&mut buf).as_bytes() {
                        push_percent(&mut out, b);
                    }
                } else {
                    out.push(c);
                }
            }
        }
        None => {
            let bytes = path_bytes(path);
            for &b in bytes.iter() {
                if b == b'%' || !(0x20..0x7f).contains(&b) {
                    push_percent(&mut out, b);
                } else {
                    out.push(char::from(b));
                }
            }
        }
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Inverse of [`escape`]. `None` for a malformed `%` sequence or an empty result.
pub(crate) fn unescape(text: &str) -> Option<PathBuf> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_value(*bytes.get(i + 1)?)?;
            let lo = hex_value(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(path_from_bytes(out))
    }
}

/// Parse a finite, non-negative float.
pub(crate) fn parse_score(text: &str) -> Option<f32> {
    let v: f32 = text.parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awkward_utf8_names_round_trip() {
        for name in [
            "/plain",
            "/with\ttab",
            "/with\nnewline",
            "/100% done",
            "/ünïcode ✓",
            "/a b",
        ] {
            let p = Path::new(name);
            let e = escape(p);
            assert!(!e.contains('\t') && !e.contains('\n'), "{e:?}");
            assert_eq!(unescape(&e).unwrap(), p);
        }
    }

    #[test]
    fn readable_text_stays_readable() {
        assert_eq!(escape(Path::new("/home/u/Docs")), "/home/u/Docs");
        assert_eq!(escape(Path::new("/ünï")), "/ünï");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_round_trip() {
        use std::os::unix::ffi::OsStrExt;
        let p = Path::new(std::ffi::OsStr::from_bytes(b"/bad\xff\xfename"));
        assert!(p.to_str().is_none());
        assert_eq!(unescape(&escape(p)).unwrap(), p);
    }

    #[test]
    fn malformed_escapes_and_scores_are_rejected() {
        assert!(unescape("/x%2").is_none());
        assert!(unescape("/x%zz").is_none());
        assert!(unescape("").is_none());
        assert!(parse_score("nan").is_none());
        assert!(parse_score("-1").is_none());
        assert_eq!(parse_score("2.500"), Some(2.5));
    }
}
