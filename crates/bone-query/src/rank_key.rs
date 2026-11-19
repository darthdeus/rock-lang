//! The completion rank key: `Ord` drives the in-process top-N select and
//! `encode()` the LSP `sort_text`, from the same field order, so the two
//! orderings cannot disagree.

/// Field order is the ranking ladder; every field ranks lower-first, and
/// `encode` appends the lowercased name as the final tiebreak. `kind` sits
/// above `case`, so locality outranks smart case (`lot` -> `LOOT_OPEN` above
/// `loot_item`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub(crate) struct RankKey {
    pub exact: u8,
    pub type_rank: u8,
    pub band: u8,
    pub kind: u8,
    pub dist: u16,
    pub method: u8,
    pub case: u8,
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

    /// Gated on a real match: an empty prefix must not reorder by length.
    pub(crate) fn len_key(score: i32, name_len: usize) -> u16 {
        if score > 0 {
            name_len.min(999) as u16
        } else {
            0
        }
    }

    /// Tuple `Ord` on a clamped key agrees with the encoded-string order at any
    /// field value; `Cand` stores clamped keys.
    pub fn clamped(self) -> RankKey {
        RankKey {
            exact: self.exact.min(9),
            type_rank: self.type_rank.min(9),
            band: self.band.min(9),
            kind: self.kind.min(9),
            dist: self.dist.min(99),
            method: self.method.min(9),
            case: self.case.min(9),
            fine: self.fine.min(9999),
            prox: self.prox.min(9),
            freq: self.freq.min(9),
            rec: self.rec.min(9),
            len: self.len.min(999),
        }
    }

    /// The LSP `sort_text`; lexicographic order equals `(RankKey, ci name)` order.
    pub fn encode(&self, name: &str) -> String {
        format!(
            "{}{}{}{}{:02}{}{}{:04}{}{}{}{:03}{}",
            self.exact.min(9),
            self.type_rank.min(9),
            self.band.min(9),
            self.kind.min(9),
            self.dist.min(99),
            self.method.min(9),
            self.case.min(9),
            self.fine.min(9999),
            self.prox.min(9),
            self.freq.min(9),
            self.rec.min(9),
            self.len.min(999),
            name.to_lowercase(),
        )
    }
}

/// Decode a sort_text into labeled fields for `--explain`; None for
/// non-RankKey strings.
pub fn explain_sort_text(st: &str) -> Option<String> {
    let b = st.as_bytes();
    if b.len() < 18 || !b[..18].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let fine: u32 = st[8..12].parse().ok()?;
    Some(format!(
        "exact={} type_rank={} band={} kind={} dist={} method={} case={} score={} prox={} freq={} rec={} len={}",
        &st[0..1],
        &st[1..2],
        &st[2..3],
        &st[3..4],
        &st[4..6],
        &st[6..7],
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

    /// String order == struct order at every field's boundary values.
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
        keys.push(RankKey { fine: 9999, ..base });
        keys.push(RankKey {
            band: 1,
            fine: 9999,
            ..base
        });
        keys.push(RankKey {
            band: 2,
            fine: 0,
            ..base
        });
        keys.push(RankKey {
            dist: 12,
            method: 1,
            ..base
        });
        keys.push(RankKey {
            fine: 100,
            prox: 3,
            ..base
        });
        keys.push(RankKey {
            prox: 1,
            freq: 2,
            ..base
        });
        keys.push(RankKey {
            freq: 1,
            rec: 4,
            ..base
        });
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
        let a = RankKey {
            dist: 150,
            ..RankKey::default()
        };
        let b = RankKey {
            dist: 100,
            ..RankKey::default()
        };
        assert_eq!(
            a.clamped().cmp(&b.clamped()),
            a.encode("x").cmp(&b.encode("x"))
        );
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
            "exact=1 type_rank=2 band=3 kind=1 dist=04 method=1 case=0 score=1021 prox=2 freq=1 rec=0 len=022"
        );
        assert!(explain_sort_text("not-a-key").is_none());
    }
}
