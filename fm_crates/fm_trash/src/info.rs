//! The `.trashinfo` file format.

pub(crate) struct ParsedInfo {
    /// Percent-decoded `Path=` bytes.
    pub path: Vec<u8>,
    /// Raw `DeletionDate=` string, if present.
    pub deletion_date: Option<String>,
}

pub(crate) fn format(encoded_path: &str, deletion_date: &str) -> String {
    format!("[Trash Info]\nPath={encoded_path}\nDeletionDate={deletion_date}\n")
}

/// Per spec: first line must be `[Trash Info]`; ignore other lines; first occurrence of a key wins.
pub(crate) fn parse(text: &str) -> Option<ParsedInfo> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "[Trash Info]" {
        return None;
    }
    let mut path = None;
    let mut date = None;
    for line in lines {
        if path.is_none() {
            if let Some(v) = line.strip_prefix("Path=") {
                path = Some(crate::encode::percent_decode(v.trim_end()));
                continue;
            }
        }
        if date.is_none() {
            if let Some(v) = line.strip_prefix("DeletionDate=") {
                date = Some(v.trim_end().to_string());
            }
        }
    }
    Some(ParsedInfo {
        path: path?,
        deletion_date: date,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip_and_rules() {
        let t = format("/home/me/a%20b", "2026-10-16T10:20:30");
        let p = parse(&t).unwrap();
        assert_eq!(p.path, b"/home/me/a b");
        assert_eq!(p.deletion_date.as_deref(), Some("2026-10-16T10:20:30"));
        assert!(parse("Path=/x\n").is_none());
        let p = parse("[Trash Info]\nPath=/a\nPath=/b\nJunk=1\n").unwrap();
        assert_eq!(p.path, b"/a");
        assert!(p.deletion_date.is_none());
    }
}
