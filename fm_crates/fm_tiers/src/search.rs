//! Name matching for the hot-tier-only, in-RAM search. No index file exists: the hot
//! listings that are already cached *are* the index.

/// Whitespace-separated, case-insensitive terms that must **all** match. A term containing
/// `/` is matched against the full path, any other term against the entry name only.
#[derive(Debug)]
pub(crate) struct Matcher {
    terms: Vec<Term>,
    pub needs_path: bool,
    needs_unicode_name: bool,
}

#[derive(Debug)]
struct Term {
    lower: String,
    ascii: bool,
    on_path: bool,
}

impl Matcher {
    /// `None` when the query contains no terms.
    pub fn new(query: &str) -> Option<Self> {
        let terms: Vec<Term> = query
            .split_whitespace()
            .map(|t| {
                let lower = t.to_lowercase();
                Term {
                    ascii: lower.is_ascii(),
                    on_path: lower.contains('/'),
                    lower,
                }
            })
            .collect();
        if terms.is_empty() {
            return None;
        }
        let needs_path = terms.iter().any(|t| t.on_path);
        let needs_unicode_name = terms.iter().any(|t| !t.on_path && !t.ascii);
        Some(Self {
            terms,
            needs_path,
            needs_unicode_name,
        })
    }

    /// Whether an entry called `name` inside `dir` matches every term. `dir` is only read
    /// when [`needs_path`](Self::needs_path) is true.
    pub fn matches(&self, name: &str, dir: &str) -> bool {
        // Lowercase each value at most once per entry, rather than once per term. ASCII-only
        // name terms retain the allocation-free byte matcher.
        let lower_name = self.needs_unicode_name.then(|| name.to_lowercase());
        let lower_path = self
            .needs_path
            .then(|| format!("{dir}/{name}").to_lowercase());

        self.terms.iter().all(|t| {
            if t.on_path {
                lower_path
                    .as_deref()
                    .is_some_and(|full| full.contains(&t.lower))
            } else if t.ascii {
                contains_ascii_ci(name.as_bytes(), t.lower.as_bytes())
            } else {
                lower_name
                    .as_deref()
                    .is_some_and(|lower| lower.contains(&t.lower))
            }
        })
    }
}

/// Case-insensitive substring search for an already-lowercase ASCII needle, without
/// allocating a lowercase copy of the haystack.
fn contains_ascii_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_has_no_matcher() {
        assert!(Matcher::new("   ").is_none());
    }

    #[test]
    fn all_terms_must_match_case_insensitively() {
        let m = Matcher::new("rep PDF").unwrap();
        assert!(m.matches("Annual Report.pdf", ""));
        assert!(!m.matches("Annual Report.txt", ""));
    }

    #[test]
    fn slash_terms_match_the_full_path() {
        let m = Matcher::new("docs/rep").unwrap();
        assert!(m.needs_path);
        assert!(m.matches("report.md", "/home/u/Docs"));
        assert!(!m.matches("report.md", "/home/u/Music"));
    }

    #[test]
    fn non_ascii_terms_fall_back_to_lowercasing() {
        let m = Matcher::new("ÉCOLE").unwrap();
        assert!(m.matches("école.txt", ""));
    }
}
