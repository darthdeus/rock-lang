//! Rank completion names and share fuzzy scoring across call-through candidates.

use crate::query::fz_boundary;
use crate::rank_key::RankKey;

const MISS: i32 = -1_000_000;
const BASE: i32 = 1;
const BOUNDARY: i32 = 10;
const CONSECUTIVE: i32 = 10;
const FIRST: i32 = 10;

/// A contiguous lead outranks scattered boundary matches in the same band.
pub(crate) fn leading_run_len(cand: &[u8], query: &[u8]) -> usize {
    let mut k = 0;
    while k < query.len() && k < cand.len() && cand[k].to_ascii_lowercase() == query[k] {
        k += 1;
    }
    k
}

pub(crate) fn fuzzy_score(cand: &[u8], query: &[u8]) -> i32 {
    let score = fuzzy_scores::<false>(cand, query, &mut []);
    if score > MISS
        && !query.is_empty()
        && cand
            .get(..query.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(query))
    {
        score + 1000
    } else {
        score
    }
}

pub(crate) fn fuzzy_prefix_scores(cand: &[u8], query: &[u8], out: &mut Vec<i32>) {
    out.resize(query.len() + 1, 0);
    out[0] = 0;
    fuzzy_scores::<false>(cand, query, &mut out[1..]);
}

pub(crate) fn fuzzy_suffix_scores(cand: &[u8], query: &[u8], out: &mut Vec<i32>) {
    out.resize(query.len() + 1, 0);
    out[query.len()] = 0;
    fuzzy_scores::<true>(cand, query, &mut out[..query.len()]);
}

// The '(' in a synthesized call breaks consecutive runs, so the name scores
// left-to-right and the argument right-to-left; every query split reuses them.
fn fuzzy_scores<const BACKWARD: bool>(cand: &[u8], query: &[u8], scores: &mut [i32]) -> i32 {
    if query.is_empty() {
        return 0;
    }
    thread_local! {
        static ROWS: std::cell::RefCell<(Vec<i32>, Vec<i32>)> =
            const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
    }
    ROWS.with(|rows| {
        let (prev, cur) = &mut *rows.borrow_mut();
        prev.resize(cand.len(), MISS);
        prev.fill(MISS);
        cur.resize(cand.len(), MISS);
        let mut best = MISS;
        for step in 0..query.len() {
            let i = if BACKWARD {
                query.len() - step - 1
            } else {
                step
            };
            let qc = query[i];
            let mut earlier = MISS;
            let mut adjacent = MISS;
            for (j, cell) in cur.iter_mut().enumerate() {
                let k = if BACKWARD { cand.len() - j - 1 } else { j };
                let previous = if step == 0 {
                    0
                } else if adjacent != MISS {
                    earlier.max(adjacent + CONSECUTIVE)
                } else {
                    earlier
                };
                *cell = if cand[k].to_ascii_lowercase() == qc && previous != MISS {
                    previous
                        + BASE
                        + if fz_boundary(cand, k) { BOUNDARY } else { 0 }
                        + if !BACKWARD && k == 0 { FIRST } else { 0 }
                } else {
                    MISS
                };
                adjacent = prev[j];
                earlier = earlier.max(adjacent);
            }
            if !scores.is_empty() || step + 1 == query.len() {
                best = cur.iter().copied().max().unwrap_or(MISS);
            }
            if !scores.is_empty() {
                scores[i] = best;
            }
            std::mem::swap(prev, cur);
        }
        best
    })
}

/// Both vectors are `q.len()+1` long; `MISS` entries are skipped.
pub(crate) fn combine_scores(prefix: &[i32], suffix: &[i32]) -> i32 {
    let mut best = MISS;
    for s in 0..prefix.len() {
        if prefix[s] > MISS && suffix[s] > MISS {
            best = best.max(prefix[s] + suffix[s]);
        }
    }
    best
}

/// A gentle penalty per `.` hop and `(` call, so a clean short path wins a
/// near-tie over a deep chain that `fuzzy_score` over-rewards.
pub(crate) fn arg_path_penalty(arg: &str) -> i32 {
    const HOP: i32 = 1;
    const CALL: i32 = 6;
    let bytes = arg.as_bytes();
    let hops = bytes.iter().filter(|&&b| b == b'.').count() as i32;
    let calls = bytes.iter().filter(|&&b| b == b'(').count() as i32;
    hops * HOP + calls * CALL
}

pub(crate) fn closed_set_rank(name: &str, prefix: &[u8], tier: u8, decl_idx: usize) -> RankKey {
    arg_rank_key(tier, 0, name, prefix, decl_idx.min(99) as u16, 0, 0)
}

pub(crate) fn arg_rank_key(
    type_rank: u8,
    kind: u8,
    cand: &str,
    query: &[u8],
    dist: u16,
    method: u8,
    score_bonus: i32,
) -> RankKey {
    // `query` is pre-lowercased.
    let score = fuzzy_score(cand.as_bytes(), query) + score_bonus;
    let (exact, band) = match_band_exact(cand, query, query);
    RankKey {
        exact,
        type_rank,
        band,
        kind,
        dist,
        method,
        fine: RankKey::fine(score),
        len: RankKey::len_key(score, cand.len()),
        ..Default::default()
    }
}

/// `exact` is 0 iff case-exact equal; `band` is 0 cs prefix, 1 ci prefix, 2
/// word-boundary subsequence, 3 scattered. Variants also match their bare tail.
pub(crate) fn match_band_exact(cand: &str, q_lc: &[u8], q_raw: &[u8]) -> (u8, u8) {
    if q_raw.is_empty() {
        return (1, 0);
    }
    let classify = |c: &str| -> (u8, u8) {
        let cb = c.as_bytes();
        let n = q_raw.len();
        let exact = u8::from(cb != q_raw);
        let band = if cb.len() >= n && cb[..n] == *q_raw {
            0
        } else if cb.len() >= n && cb[..n].eq_ignore_ascii_case(q_raw) {
            1
        } else if crate::query::clean_match(cb, q_lc) {
            2
        } else {
            3
        };
        (exact, band)
    };
    let (e, b) = classify(cand);
    if let Some(idx) = cand.rfind("::") {
        let (e2, b2) = classify(&cand[idx + 2..]);
        return (e.min(e2), b.min(b2));
    }
    (e, b)
}

/// Smart-case sort digit: case mismatches along a case-insensitive alignment, capped at 9.
pub(crate) fn case_mismatch_rank(cand: &[u8], query: &[u8]) -> u8 {
    let mut qi = 0;
    let mut miss = 0u32;
    for &cc in cand {
        if qi >= query.len() {
            break;
        }
        let qc = query[qi];
        if cc.eq_ignore_ascii_case(&qc) {
            miss += u32::from(cc != qc);
            qi += 1;
        }
    }
    miss.min(9) as u8
}

/// Both `query` and `name_lc` are already lowercased.
pub(crate) fn prefix_subseq_len(query: &[u8], name_lc: &str) -> usize {
    let mut qi = 0;
    for &c in name_lc.as_bytes() {
        if qi < query.len() && query[qi] == c {
            qi += 1;
        }
    }
    qi
}

/// Must match the lowercased suffix `sort_text` appends, so the tuple sort
/// selects the same survivors a sort_text string sort would.
pub(crate) fn ci_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let n = a.len().min(b.len());
    for i in 0..n {
        let (x, y) = (a[i].to_ascii_lowercase(), b[i].to_ascii_lowercase());
        if x != y {
            return x.cmp(&y);
        }
    }
    a.len().cmp(&b.len())
}
