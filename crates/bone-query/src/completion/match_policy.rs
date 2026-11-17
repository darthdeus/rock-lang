//! Completion admission controls which names enter ranking. Compare policies
//! with `rock lsp completion <position> <prefixes>... --policies <list>`.

use std::cell::Cell;

/// How permissive the completion admission gate is. Ranking is unaffected --
/// only which candidates survive to be ranked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MatchPolicy {
    /// Word-boundary / run subsequence only (`clean_match`). Drops mid-word
    /// scatter at the gate -- `lotop` does NOT reach `LOOT_OPEN`.
    Clean,
    /// Any case-insensitive subsequence. `lotop` reaches `LOOT_OPEN`; the band
    /// ranks the scatter last. fzf/VSCode feel.
    Subseq,
    /// `Clean` for queries under 3 chars (short queries stay tight, no flood),
    /// full subsequence at 3+ chars.
    Subseq3,
}

impl MatchPolicy {
    pub fn parse(s: &str) -> Option<MatchPolicy> {
        match s.trim().to_ascii_lowercase().as_str() {
            "clean" => Some(MatchPolicy::Clean),
            "subseq" => Some(MatchPolicy::Subseq),
            "subseq3" => Some(MatchPolicy::Subseq3),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            MatchPolicy::Clean => "clean",
            MatchPolicy::Subseq => "subseq",
            MatchPolicy::Subseq3 => "subseq3",
        }
    }
}

thread_local! {
    static POLICY: Cell<MatchPolicy> = const { Cell::new(MatchPolicy::Subseq3) };
}

/// Set the admission policy for the current thread (the completion query runs
/// single-threaded on the caller). Used by the tuning matrix to A/B one cursor.
pub fn set_match_policy(p: MatchPolicy) {
    POLICY.with(|c| c.set(p));
}

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

/// The one admission gate. `query` is already lowercased (the callers pass the
/// lowercased prefix, matching `clean_match`'s contract).
pub(crate) fn admit(cand: &[u8], query: &[u8]) -> bool {
    match POLICY.with(Cell::get) {
        MatchPolicy::Clean => crate::query::clean_match(cand, query),
        MatchPolicy::Subseq => is_subsequence(cand, query),
        // clean ⊆ subseq, so at 3+ chars this is just the subsequence test.
        MatchPolicy::Subseq3 => {
            if query.len() >= 3 {
                is_subsequence(cand, query)
            } else {
                crate::query::clean_match(cand, query)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_admit_scattered_matches_at_different_lengths() {
        for (policy, short, long) in [
            (MatchPolicy::Clean, false, false),
            (MatchPolicy::Subseq, true, true),
            (MatchPolicy::Subseq3, false, true),
        ] {
            set_match_policy(policy);
            assert!(admit(b"close_loot_panel", b""));
            assert_eq!(admit(b"close_loot_panel", b"op"), short);
            assert_eq!(admit(b"close_loot_panel", b"opl"), long);
        }
        set_match_policy(MatchPolicy::Subseq3);
    }
}
