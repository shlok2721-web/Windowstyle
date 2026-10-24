//! RFC 2396 style percent-encoding of raw path bytes (as the spec requires for `Path=`).

const HEX: &[u8; 16] = b"0123456789ABCDEF";

pub(crate) fn percent_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                s.push(b as char)
            }
            _ => {
                s.push('%');
                s.push(HEX[(b >> 4) as usize] as char);
                s.push(HEX[(b & 15) as usize] as char);
            }
        }
    }
    s
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = b.get(i + 1).copied().and_then(hex);
            let l = b.get(i + 2).copied().and_then(hex);
            if let (Some(h), Some(l)) = (h, l) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for raw in [&b"/home/me/a b/%x\xff\n"[..], b"plain/path.txt", b""] {
            assert_eq!(percent_decode(&percent_encode(raw)), raw);
        }
        assert_eq!(percent_encode(b"/a b"), "/a%20b");
        assert_eq!(percent_decode("100%"), b"100%");
        assert_eq!(percent_decode("%zz%4"), b"%zz%4");
    }
}
