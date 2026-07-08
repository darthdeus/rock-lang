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
///   3 variant, 4 test-scoped symbol outside test files; 0 (neutral)
///   where a path ranks one kind only.
/// - `dist`: reach hop distance; 0 for direct candidates.
/// - `method`: 1 for a callable (needs args / has side effects), 0 else.
/// - `fine`: `9999 - clamped fuzzy score` -- the within-band tiebreak.
/// - `prox`: definition proximity to the request file -- 0 same file,
///   1 same project root, 2 other workspace code (engine/deps), 3 stdlib/
///   builtins. Late PRIOR: decides only where match quality ties (the
///   whole empty-prefix browse, score ties at typed prefixes).
/// - `freq`: workspace reference count, log-bucketed (0 hot / 1 warm /
///   2 rare-or-unknown; FREQ_HOT/FREQ_WARM in the tuning block). Priors
///   land AFTER prox: "my file first, then the popular stuff", and
///   stdlib-internal call-count inflation stays contained in prox tier 3.
/// - `rec`: scope recency for LOCALS -- in-scope bindings ranked by
///   declaration position, closest above the cursor first (params carry
///   the earliest spans, so an inner `let` beats an enclosing param on
///   ties). 0 (neutral) for every other kind.
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
    pub prox: u8,
    pub freq: u8,
    pub rec: u8,
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

    /// The key with every field clamped to its `encode` digit width. Tuple
    /// `Ord` on a clamped key agrees with the encoded-string order at ANY
    /// field value (an unclamped `dist: 150` orders below `dist: 100` in
    /// `Ord` but both encode `99`); `Cand` stores clamped keys so the
    /// in-process select and the client's sort_text sort cannot disagree.
    pub fn clamped(self) -> RankKey {
        RankKey {
            exact: self.exact.min(9),
            type_rank: self.type_rank.min(9),
            band: self.band.min(9),
            case: self.case.min(9),
            kind: self.kind.min(9),
            dist: self.dist.min(99),
            method: self.method.min(9),
            fine: self.fine.min(9999),
            prox: self.prox.min(9),
            freq: self.freq.min(9),
            rec: self.rec.min(9),
            len: self.len.min(999),
        }
    }

    /// The LSP `sort_text`: the fields in `Ord` order, fixed-width, with the
    /// lowercased candidate appended as the final ci-alphabetical tiebreak.
    /// Lexicographic order of these strings == `(RankKey, ci name)` order.
    pub fn encode(&self, name: &str) -> String {
        format!(
            "{}{}{}{}{}{:02}{}{:04}{}{}{}{:03}{}",
            self.exact.min(9),
            self.type_rank.min(9),
            self.band.min(9),
            self.case.min(9),
            self.kind.min(9),
            self.dist.min(99),
            self.method.min(9),
            self.fine.min(9999),
            self.prox.min(9),
            self.freq.min(9),
            self.rec.min(9),
            self.len.min(999),
            name.to_lowercase(),
        )
    }
}

/// Decode an encoded sort_text into labeled fields -- the `--explain`
/// surface (`rock lsp completion <pos> --explain`): a live mis-rank reads as
/// named rungs instead of hand-decoded digit columns. None when the string
/// is not a RankKey encoding (foreign/legacy sort_texts).
pub fn explain_sort_text(st: &str) -> Option<String> {
    let b = st.as_bytes();
    if b.len() < 18 || !b[..18].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let fine: u32 = st[8..12].parse().ok()?;
    Some(format!(
        "exact={} type_rank={} band={} case={} kind={} dist={} method={} score={} prox={} freq={} rec={} len={}",
        &st[0..1],
        &st[1..2],
        &st[2..3],
        &st[3..4],
        &st[4..5],
        &st[5..7],
        &st[7..8],
        9999 - fine,
        &st[12..13],
        &st[13..14],
        &st[14..15],
        &st[15..18],
    ))
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
        for f in 0..12u8 {
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
                8 => k.prox = 3,
                9 => k.freq = 2,
                10 => k.rec = 9,
                11 => k.len = 999,
                _ => unreachable!(),
            }
            keys.push(k);
        }
        // Cross-field dominance pairs and a couple of composites.
        keys.push(RankKey { fine: 9999, ..base });
        keys.push(RankKey { band: 1, fine: 9999, ..base });
        keys.push(RankKey { band: 2, fine: 0, ..base });
        keys.push(RankKey { dist: 12, method: 1, ..base });
        keys.push(RankKey { fine: 100, prox: 3, ..base });
        keys.push(RankKey { prox: 1, freq: 2, ..base });
        keys.push(RankKey { freq: 1, rec: 4, ..base });
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
    fn clamped_ord_agrees_with_encode_beyond_widths() {
        // Unclamped Ord would order these; both encode dist as `99`.
        let a = RankKey { dist: 150, ..RankKey::default() };
        let b = RankKey { dist: 100, ..RankKey::default() };
        assert_eq!(a.clamped().cmp(&b.clamped()), a.encode("x").cmp(&b.encode("x")));
        // In-range values pass through untouched.
        let k = RankKey {
            exact: 1,
            type_rank: 4,
            band: 3,
            case: 9,
            kind: 3,
            dist: 99,
            method: 1,
            fine: 9999,
            prox: 9,
            freq: 9,
            rec: 9,
            len: 999,
        };
        assert_eq!(k.clamped(), k);
    }

    #[test]
    fn len_gated_on_real_match() {
        assert_eq!(RankKey::len_key(0, 40), 0);
        assert_eq!(RankKey::len_key(1, 40), 40);
        assert_eq!(RankKey::len_key(50, 5000), 999);
    }

    #[test]
    fn explain_round_trips_encode() {
        let k = RankKey {
            exact: 1,
            type_rank: 2,
            band: 3,
            case: 0,
            kind: 1,
            dist: 4,
            method: 1,
            fine: RankKey::fine(1021),
            prox: 2,
            freq: 1,
            rec: 0,
            len: 22,
        };
        let ex = explain_sort_text(&k.encode("target_grid")).expect("decodes");
        assert_eq!(
            ex,
            "exact=1 type_rank=2 band=3 case=0 kind=1 dist=04 method=1 score=1021 prox=2 freq=1 rec=0 len=022"
        );
        assert!(explain_sort_text("not-a-key").is_none());
    }
}
