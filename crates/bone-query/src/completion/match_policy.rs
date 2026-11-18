/// Case-insensitive subsequence: is every byte of `query` (already lowercased)
/// present in `cand` (original case) in order? Empty query matches.
pub(crate) fn is_subsequence(cand: &[u8], query: &[u8]) -> bool {
    let mut qi = 0;
    if qi == query.len() {
        return true;
    }
    for &c in cand {
        if c.to_ascii_lowercase() == query[qi] {
            qi += 1;
            if qi == query.len() {
                return true;
            }
        }
    }
    false
}

/// Short prefixes match at word boundaries or within a run. At three bytes
/// or more, a case-insensitive subsequence suffices. The query is lowercase.
pub(crate) fn admit(cand: &[u8], query: &[u8]) -> bool {
    if query.len() >= 3 {
        is_subsequence(cand, query)
    } else {
        crate::query::clean_match(cand, query)
    }
}

#[cfg(test)]
mod tests {
    use super::admit;

    #[test]
    fn three_byte_prefix_admits_scattered_matches() {
        assert!(admit(b"close_loot_panel", b""));
        assert!(!admit(b"close_loot_panel", b"op"));
        assert!(admit(b"close_loot_panel", b"opl"));
    }
}
