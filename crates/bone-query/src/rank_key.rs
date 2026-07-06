//! The ONE completion rank key (docs/completion-v2-design.md, M1). Every
//! completion path expresses its ranking as a `RankKey`: `Ord` drives the
//! in-process top-N select and `encode()` produces the LSP `sort_text`
//! string, from the same field order -- so the two orderings cannot
//! disagree, and a ranking rule added here exists in every path at once.

/// Field order IS the ranking ladder (docs/completion-ranking-design.md):
/// exact whole-name match, expected-type relevance, coarse match band,
/// smart case, locality/kind, reach distance, non-fn-before-fn, fine fuzzy
/// score, shorter-first. The ci name tiebreak lives outside: `encode`
/// appends the lowercased name; tuple-compare callers chain `ci_cmp`.
///
/// Field semantics (all "lower ranks first"):
/// - `exact`: 0 iff the candidate name equals the query case-exact.
/// - `type_rank`: expected-type relevance when a slot type is known
///   (0 right-type value/variant .. 4 wrong-type synth); with NO expected
///   type it is the coarse bucket only (0 direct, 4 call-through synth).
/// - `band`: coarse match tier -- 0 case-exact prefix, 1 ci prefix,
///   2 clean word-boundary subsequence, 3 scattered; 0 on an empty query.
/// - `case`: smart-case mismatch count (0-9); 0 (neutral) in typed slots.
/// - `kind`: locality ladder -- 0 local/param, 1 global, 2 fn/type,
///   3 variant; 0 (neutral) where a path ranks one kind only.
/// - `dist`: reach hop distance; 0 for direct candidates.
/// - `method`: 1 for a callable (needs args / has side effects), 0 else.
/// - `fine`: `9999 - clamped fuzzy score` -- the within-band tiebreak.
/// - `len`: candidate length when there IS a match (score > 0), else 0 --
///   shorter-first among equals, but an empty prefix must NOT reorder by
///   length (short stdlib names would bury longer user symbols).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub(crate) struct RankKey {
    pub exact: u8,
    pub type_rank: u8,
    pub band: u8,
    pub case: u8,
    pub kind: u8,
    pub dist: u16,
    pub method: u8,
    pub fine: u16,
    pub len: u16,
}

impl RankKey {
    /// `fine` from a raw fuzzy score (higher score -> lower key).
    pub fn fine(score: i32) -> u16 {
        (9999 - score.clamp(0, 9999)) as u16
    }

    /// Shorter-first tiebreak, gated on a real match (score > 0).
    pub fn len_key(score: i32, name_len: usize) -> u16 {
        if score > 0 {
            name_len.min(999) as u16
        } else {
            0
        }
    }

    /// The LSP `sort_text`: the fields in `Ord` order, fixed-width, with the
    /// lowercased candidate appended as the final ci-alphabetical tiebreak.
    /// Lexicographic order of these strings == `(RankKey, ci name)` order.
    pub fn encode(&self, name: &str) -> String {
        format!(
            "{}{}{}{}{}{:02}{}{:04}{:03}{}",
            self.exact.min(9),
            self.type_rank.min(9),
            self.band.min(9),
            self.case.min(9),
            self.kind.min(9),
            self.dist.min(99),
            self.method.min(9),
            self.fine.min(9999),
            self.len.min(999),
            name.to_lowercase(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing property: string order == struct order, across every
    /// field at its boundary values, including width-clamped ones.
    #[test]
    fn encode_order_agrees_with_ord() {
        let base = RankKey::default();
        let mut keys = vec![base];
        for f in 0..9u8 {
            let mut k = base;
            match f {
                0 => k.exact = 1,
                1 => k.type_rank = 4,
                2 => k.band = 3,
                3 => k.case = 9,
                4 => k.kind = 3,
                5 => k.dist = 99,
                6 => k.method = 1,
                7 => k.fine = 9999,
                8 => k.len = 999,
                _ => unreachable!(),
            }
            keys.push(k);
        }
        // Cross-field dominance pairs and a couple of composites.
        keys.push(RankKey { fine: 9999, ..base });
        keys.push(RankKey { band: 1, fine: 9999, ..base });
        keys.push(RankKey { band: 2, fine: 0, ..base });
        keys.push(RankKey { dist: 12, method: 1, ..base });
        for a in &keys {
            for b in &keys {
                let by_struct = a.cmp(b);
                let by_string = a.encode("x").cmp(&b.encode("x"));
                assert_eq!(
                    by_struct, by_string,
                    "Ord and encode disagree: {a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn len_gated_on_real_match() {
        assert_eq!(RankKey::len_key(0, 40), 0);
        assert_eq!(RankKey::len_key(1, 40), 40);
        assert_eq!(RankKey::len_key(50, 5000), 999);
    }
}
