//! The completion ADMISSION knob. Every "does this candidate's name match the
//! typed query well enough to appear at all" gate routes through `admit`, whose
//! policy is a process/thread-wide switch -- so the whole completion surface
//! flips between matchers at once for A/B tuning (`rock lsp completion-matrix`).
//!
//! Admission is deliberately SEPARATE from ranking: `match_band_exact`
//! (`rank.rs`) still classifies every survivor into a coarse band (0 exact
//! prefix .. 3 scattered) and `fuzzy_score` still rewards runs/boundaries. A
//! run/boundary is a rank BOOST; a scatter is a rank DEMOTION; only "not a
//! subsequence at all" is a rejection under the permissive policies. The strict
//! `Clean` policy additionally rejects scatter at the gate (the historical
//! behavior). Default is `Subseq3`: `Clean` under 3 chars (no short-query
//! flood), full subsequence at 3+ (so `lotop` reaches `LOOT_OPEN`), scatter
//! always demoted to band 3 below the prefix/clean matches.

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

    /// The policy a fresh thread starts with (env override, else `Subseq3`).
    pub fn default_policy() -> MatchPolicy {
        MatchPolicy::from_env()
    }

    fn from_env() -> MatchPolicy {
        std::env::var("ROCK_LSP_MATCH_POLICY")
            .ok()
            .and_then(|v| MatchPolicy::parse(&v))
            .unwrap_or(MatchPolicy::Subseq3)
    }
}

thread_local! {
    static POLICY: Cell<MatchPolicy> = Cell::new(MatchPolicy::from_env());
}

/// Set the admission policy for the current thread (the completion query runs
/// single-threaded on the caller). Used by the tuning matrix to A/B one cursor.
pub fn set_match_policy(p: MatchPolicy) {
    POLICY.with(|c| c.set(p));
}

pub fn match_policy() -> MatchPolicy {
    POLICY.with(Cell::get)
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
    match match_policy() {
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

    fn under<T>(p: MatchPolicy, f: impl FnOnce() -> T) -> T {
        set_match_policy(p);
        let r = f();
        set_match_policy(MatchPolicy::Subseq3);
        r
    }

    #[test]
    fn subsequence_is_order_preserving_and_ci() {
        assert!(is_subsequence(b"LOOT_OPEN", b"lotop")); // scattered, in order
        assert!(is_subsequence(b"LOOT_OPEN", b"loop"));
        assert!(!is_subsequence(b"LOOT_OPEN", b"lox")); // x absent
        assert!(!is_subsequence(b"LOOT_OPEN", b"olo")); // out of order
        assert!(is_subsequence(b"anything", b"")); // empty matches
    }

    #[test]
    fn clean_drops_scatter_that_subseq_admits() {
        // The canonical bug: `lotop` skips the 2nd `O`, landing `t` mid-word.
        assert!(!crate::query::clean_match(b"LOOT_OPEN", b"lotop"));
        assert!(under(MatchPolicy::Subseq, || admit(b"LOOT_OPEN", b"lotop")));
        assert!(under(MatchPolicy::Subseq3, || admit(b"LOOT_OPEN", b"lotop")));
        assert!(!under(MatchPolicy::Clean, || admit(b"LOOT_OPEN", b"lotop")));
    }

    #[test]
    fn subseq3_stays_clean_below_three_chars() {
        // 2-char scatter: subseq admits the flood, subseq3 (== clean here) rejects.
        assert!(!crate::query::clean_match(b"close_loot_panel", b"op"));
        assert!(under(MatchPolicy::Subseq, || admit(b"close_loot_panel", b"op")));
        assert!(!under(MatchPolicy::Subseq3, || admit(b"close_loot_panel", b"op")));
        // At 3 chars subseq3 opens up.
        assert!(under(MatchPolicy::Subseq3, || admit(b"close_loot_panel", b"opl")));
    }
}
