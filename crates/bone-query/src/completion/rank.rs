//! Completion ranking primitives (completion-v2 stage C): the shared fuzzy
//! DP, the coarse match bands, and the `RankKey` constructors every source
//! ranks through. `clean_match`/`fz_boundary` stay in `crate::query` -- they
//! are the centralized matchers (docs/lsp-architecture.md).

use crate::query::fz_boundary;
use crate::rank_key::RankKey;

// Shared fuzzy-scoring constants + boundary rule -- used by `fuzzy_score` and by
// the call-through partition decomposition (`fuzzy_prefix_scores` /
// `fuzzy_suffix_scores`), which MUST score identically to `fuzzy_score` for the
// decomposition `score = max_s(prefix[s] + suffix[s])` to stay byte-exact.
pub(crate) const FZ_MISS: i32 = -1_000_000;
pub(crate) const FZ_BASE: i32 = 1;
pub(crate) const FZ_BOUNDARY: i32 = 10; // matched char at a word start
pub(crate) const FZ_CONSEC: i32 = 8; // matched char right after the previous match (run)
pub(crate) const FZ_FIRST: i32 = 10; // extra for matching at absolute position 0


pub(crate) fn fuzzy_score(cand: &[u8], query: &[u8]) -> i32 {
    const MISS: i32 = FZ_MISS;
    const BASE: i32 = FZ_BASE;
    const BOUNDARY: i32 = FZ_BOUNDARY;
    const CONSEC: i32 = FZ_CONSEC;
    const FIRST: i32 = FZ_FIRST;
    if query.is_empty() {
        return 0;
    }
    let n = cand.len();
    if n == 0 {
        return MISS;
    }
    let boundary = |j: usize| fz_boundary(cand, j);
    // The two DP rows (`prev`/`cur`, each `n` wide) are reused from a thread-
    // local scratch -- `fuzzy_score` is called hundreds of times per keystroke
    // (clean_match per candidate fn, arg_sort_text per survivor, the general
    // ranking), and a per-call `vec![MISS; n]` x2 was the dominant short-prefix
    // cost. `resize` reuses the existing capacity, so steady state is alloc-free.
    thread_local! {
        static SCRATCH: std::cell::RefCell<(Vec<i32>, Vec<i32>)> =
            const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
    }
    SCRATCH.with(|sc| {
        let (prev, cur) = &mut *sc.borrow_mut();
        prev.clear();
        prev.resize(n, MISS); // best score with query[i-1] ending at each cand pos
        cur.clear();
        cur.resize(n, MISS);
        for (i, &qc) in query.iter().enumerate() {
            // `max_prev` = best prev[k] for k < j (query[i-1] somewhere before j).
            let mut max_prev = MISS;
            for j in 0..n {
                cur[j] = MISS;
                if cand[j].to_ascii_lowercase() == qc {
                    let mut b = BASE;
                    if boundary(j) {
                        b += BOUNDARY;
                    }
                    if j == 0 {
                        b += FIRST;
                    }
                    if i == 0 {
                        cur[j] = b;
                    } else {
                        let mut s = MISS;
                        if max_prev > MISS {
                            s = max_prev + b;
                        }
                        // Consecutive: query[i-1] ended exactly at j-1 -> extend the run.
                        if j > 0 && prev[j - 1] > MISS {
                            s = s.max(prev[j - 1] + b + CONSEC);
                        }
                        cur[j] = s;
                    }
                }
                if prev[j] > max_prev {
                    max_prev = prev[j];
                }
            }
            std::mem::swap(prev, cur);
        }
        let mut best = prev.iter().copied().max().unwrap_or(MISS);
        // A true whole-string prefix (typing the name/root from its start) is the
        // strongest signal -- it must beat any boundary-aligned partial match
        // elsewhere (`play` -> `player...` over `pixel_LAYer...`). Dominant bonus.
        if best > MISS && cand.len() >= query.len() && cand[..query.len()].eq_ignore_ascii_case(query) {
            best += 1000;
        }
        best
    })
}

// --- Call-through partition decomposition (lever A) ---------------------------
//
// `fuzzy_score(name + "(" + arg + ")", q)` is a max over alignments of `q`. Each
// alignment splits `q` at some index `s`: `q[..s]` matches inside the name,
// `q[s..]` inside the arg. The `(` between them breaks any consecutive-run bonus,
// so the two halves score INDEPENDENTLY:
//
//     score(name, arg, q) = max over s of [ prefix_name[s] + suffix_arg[s] ]
//
// `prefix_name[s]` depends only on (name, q); `suffix_arg[s]` only on (arg, q) --
// so the arg half is computed ONCE and shared by every fn in a group, and the
// per-(fn,arg) cost drops from a full DP to a length-`|q|` combine. EXACT: the
// `+1000` whole-query-prefix bonus never applies to call-through (a passing fn's
// name cannot contain all of `q`, by the noise guard `k < q.len()`), so it is
// intentionally omitted here.

/// `out[s]` = best `fuzzy_score`-style score of the prefix `q[..s]` matched as a
/// subsequence of `cand` (`s = 0..=q.len()`), WITHOUT the `+1000` bonus. `out[0]
/// = 0`; `out[s] = FZ_MISS` when `q[..s]` is not a subsequence. The forward DP is
/// `fuzzy_score`'s, recording the row max after each query char.
pub(crate) fn fuzzy_prefix_scores(cand: &[u8], query: &[u8], out: &mut Vec<i32>) {
    out.clear();
    out.push(0); // s = 0: empty prefix
    let m = query.len();
    let n = cand.len();
    if m == 0 {
        return;
    }
    if n == 0 {
        out.resize(m + 1, FZ_MISS);
        return;
    }
    let mut prev = vec![FZ_MISS; n];
    let mut cur = vec![FZ_MISS; n];
    for (i, &qc) in query.iter().enumerate() {
        let mut max_prev = FZ_MISS;
        let mut rowmax = FZ_MISS;
        for j in 0..n {
            cur[j] = FZ_MISS;
            if cand[j].to_ascii_lowercase() == qc {
                let mut b = FZ_BASE;
                if fz_boundary(cand, j) {
                    b += FZ_BOUNDARY;
                }
                if j == 0 {
                    b += FZ_FIRST;
                }
                if i == 0 {
                    cur[j] = b;
                } else {
                    let mut s = FZ_MISS;
                    if max_prev > FZ_MISS {
                        s = max_prev + b;
                    }
                    if j > 0 && prev[j - 1] > FZ_MISS {
                        s = s.max(prev[j - 1] + b + FZ_CONSEC);
                    }
                    cur[j] = s;
                }
            }
            if prev[j] > max_prev {
                max_prev = prev[j];
            }
            if cur[j] > rowmax {
                rowmax = cur[j];
            }
        }
        out.push(rowmax); // best score of q[..=i] = prefix length i+1
        std::mem::swap(&mut prev, &mut cur);
    }
}

/// `out[s]` = best `fuzzy_score`-style score of the suffix `q[s..]` matched as a
/// subsequence of `cand`, scored AS IF `cand` sits right after a `(`: `cand[0]`
/// is a word boundary (it is -- preceded by `(`) but NOT absolute position 0, so
/// it gets `FZ_BOUNDARY` but NOT `FZ_FIRST`. `out[q.len()] = 0`; `out[s] =
/// FZ_MISS` when `q[s..]` is not a subsequence. Backward DP: `next[j]` = best
/// score of `q[i+1..]` with `q[i+1]` starting at position `j`.
pub(crate) fn fuzzy_suffix_scores(cand: &[u8], query: &[u8], out: &mut Vec<i32>) {
    let m = query.len();
    out.clear();
    out.resize(m + 1, FZ_MISS);
    out[m] = 0; // empty suffix
    let n = cand.len();
    if m == 0 || n == 0 {
        return;
    }
    let mut next = vec![FZ_MISS; n]; // matchStart for q[i+1..]
    let mut cur = vec![FZ_MISS; n];
    for i in (0..m).rev() {
        let qc = query[i];
        let last = i + 1 == m;
        // `suffmax` = max next[j'] for j' > current j (scanning j descending).
        let mut suffmax = FZ_MISS;
        let mut rowmax = FZ_MISS;
        for j in (0..n).rev() {
            cur[j] = FZ_MISS;
            if cand[j].to_ascii_lowercase() == qc {
                let mut b = FZ_BASE;
                if fz_boundary(cand, j) {
                    b += FZ_BOUNDARY; // boundary; never FZ_FIRST (not at absolute 0)
                }
                let cont = if last {
                    0 // last query char: empty continuation
                } else {
                    // q[i+1] at some j' > j; +CONSEC only when adjacent (j'==j+1).
                    let mut c = suffmax;
                    if j + 1 < n && next[j + 1] > FZ_MISS {
                        c = c.max(next[j + 1] + FZ_CONSEC);
                    }
                    c
                };
                if cont > FZ_MISS {
                    cur[j] = b + cont;
                }
            }
            if next[j] > suffmax {
                suffmax = next[j];
            }
            if cur[j] > rowmax {
                rowmax = cur[j];
            }
        }
        out[i] = rowmax;
        std::mem::swap(&mut next, &mut cur);
    }
}

/// `max over s of prefix[s] + suffix[s]` -- the call-through partition combine.
/// Both vectors are `q.len()+1` long; `FZ_MISS` entries (non-subsequence splits)
/// are skipped. Returns `FZ_MISS` if no split is viable.
pub(crate) fn combine_scores(prefix: &[i32], suffix: &[i32]) -> i32 {
    let mut best = FZ_MISS;
    for s in 0..prefix.len() {
        if prefix[s] > FZ_MISS && suffix[s] > FZ_MISS {
            best = best.max(prefix[s] + suffix[s]);
        }
    }
    best
}

/// Ranking penalty for a call-through arg by PATH COMPLEXITY: `.` hops and `(`
/// method calls in the arg string. A direct value or enum variant has neither
/// (0); `player.movable.grid` pays for 2 field hops; `x.foo().image_dims()` pays
/// 2 hops + 2 calls. The boundary-rich `fuzzy_score` over-rewards long segmented
/// args (a scattered match on a deep method chain can out-score a tight match on
/// a clean short path -- e.g. `tile_center(PIXEL_EDITOR.active_layer_image().image_dims())`
/// beating `tile_center(player.movable.grid)`), so subtract this so a direct
/// value / clean field path wins on a near-tie. It's gentle (a call costs less
/// than one boundary bonus), so a genuinely strong fuzzy match still wins.
pub(crate) fn arg_path_penalty(arg: &str) -> i32 {
    const HOP: i32 = 1;
    const CALL: i32 = 6;
    let bytes = arg.as_bytes();
    let hops = bytes.iter().filter(|&&b| b == b'.').count() as i32;
    let calls = bytes.iter().filter(|&&b| b == b'(').count() as i32;
    hops * HOP + calls * CALL
}


/// Build the arg-position / reach-path / member-default `sort_text`: a
/// case- and kind-neutral `RankKey` with the caller's `type_rank` (0
/// value-of-expected-type / 1 fn-returning-it / 2 other), `dist` (shorter
/// paths first) and `method` (field/var before method/fn).
pub(crate) fn arg_sort_text(type_rank: u8, kind: u8, cand: &str, query: &[u8], dist: u16, method: u8) -> String {
    arg_rank_key(type_rank, kind, cand, query, dist, method).encode(cand)
}

/// sort_text for a CLOSED-SET completion (struct-literal / record-variant
/// field names, enum-qualified variants): match band + fine against the
/// typed prefix lead, `tier` rides `type_rank` (struct-literal REQUIRED
/// fields tier above defaulted ones; 0 where the set has no tiers), and the
/// DECLARATION index rides `dist` -- for a closed set the author's
/// declaration order is the browse order, so it breaks ties instead of the
/// alphabetical name suffix.
pub(crate) fn closed_set_sort_text(name: &str, prefix: &[u8], tier: u8, decl_idx: usize) -> String {
    let score = fuzzy_score(name.as_bytes(), prefix);
    let (exact, band) = match_band_exact(name, prefix, prefix);
    RankKey {
        exact,
        type_rank: tier,
        band,
        dist: decl_idx.min(99) as u16,
        fine: RankKey::fine(score),
        len: RankKey::len_key(score, name.len()),
        ..Default::default()
    }
    .encode(name)
}

/// The `RankKey` behind `arg_sort_text` -- the reach source emits it as a
/// `Cand` fact and defers the encode to materialization.
pub(crate) fn arg_rank_key(type_rank: u8, kind: u8, cand: &str, query: &[u8], dist: u16, method: u8) -> RankKey {
    // `fuzzy_score` lowercases the candidate inline; `query` is pre-lowercased.
    let score = fuzzy_score(cand.as_bytes(), query);
    // Arg path is case-neutral, so the pre-lowercased query stands in for the raw
    // one (cs/ci prefix collapse to one band -- band is tertiary here anyway).
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

/// `(exact, band)` match-quality classification for the ranking ladder
/// (`docs/completion-ranking-design.md`). `exact` = 0 iff `cand` equals the
/// query case-exact (a whole-name hit outranks everything). `band` is the COARSE
/// match tier -- 0 case-exact prefix, 1 ci prefix, 2 clean word-boundary
/// subsequence, 3 scattered -- so the fine `fuzzy_score` no longer leads the
/// sort (a boundary-richer longer name can't outrank a same-tier local on raw
/// points). A variant `Enum::Variant` is classified against BOTH the qualified
/// name and the bare tail (the user types either), taking the better of each.
/// Empty query -> `(1, 0)`: no match signal, kind/alpha decide as before.
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

/// Smart-case leading sort digit. Greedily align `query` into `cand` as a
/// case-INSENSITIVE subsequence (first match per query char), then count the
/// matched positions whose CASE differs from the query char. A lowercase query
/// over an all-caps name (`jlog` vs `JIT_..LOG..`) counts every char as a
/// mismatch and is demoted; a case-clean match (`jlog`/`journal_log`, or an
/// uppercase query over the caps name) scores 0. Capped at 9 (one sort digit).
/// Both slices are ORIGINAL case; an empty query yields 0 (no reordering).
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

/// Length of the longest prefix of `query` (already lowercased) that is a
/// subsequence of `name_lc` (the function name, ALREADY lowercased -- callers
/// pass `CtFn::name_lc` so this never re-allocates a `to_lowercase` per fn on
/// the keystroke hot path) -- how many leading query chars the function name can
/// absorb. Used to prefilter call-through candidates: a fn whose name can't
/// absorb the start of the query is not a call the user is typing.
pub(crate) fn prefix_subseq_len(query: &[u8], name_lc: &str) -> usize {
    let mut qi = 0;
    for &c in name_lc.as_bytes() {
        if qi < query.len() && query[qi] == c {
            qi += 1;
        }
    }
    qi
}

/// Case-insensitive ASCII byte-slice ordering -- the alloc-free equivalent of
/// `a.to_lowercase().cmp(&b.to_lowercase())` for the (ASCII) identifier
/// tiebreak in completion ranking. Matches the lowercased `lc` suffix that
/// `sort_text` appends, so the tuple sort selects the same survivors a
/// sort_text string sort would.
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

/// Member sort_text used ONLY in a bool context (`if`/`while <chain>`), where no
/// reach-path items coexist. A `bool` member rides the type-relevance rung
/// (rank 0), everything else 1 -- the bool-context analogue of an expected
/// type. Banded like every other path: a prefix-tier match outranks a
/// boundary-richer higher-scoring one.
pub(crate) fn member_bool_sort_text(name: &str, pfx: &[u8], is_bool: bool, method: u8) -> String {
    let score = fuzzy_score(name.as_bytes(), pfx);
    let (exact, band) = match_band_exact(name, pfx, pfx);
    let key = RankKey {
        exact,
        type_rank: u8::from(!is_bool),
        band,
        dist: 1,
        method,
        fine: RankKey::fine(score),
        len: RankKey::len_key(score, name.len()),
        ..Default::default()
    };
    key.encode(name)
}

/// Member sort_text used ONLY in a call-arg position (`callee(.., recv.<prefix>)`
/// with a known expected param type). A member whose type MATCHES the expected
/// param takes type-relevance rank 0 and DOMINATES (mirrors the global path,
/// where a value of the expected type outranks a better-prefix candidate of the
/// wrong type). The prefix filter still removes wrong-prefix candidates first,
/// so this only reorders genuine matches.
pub(crate) fn member_arg_sort_text(name: &str, pfx: &[u8], type_match: bool, method: u8, dist: u16) -> String {
    let score = fuzzy_score(name.as_bytes(), pfx);
    let (exact, band) = match_band_exact(name, pfx, pfx);
    let key = RankKey {
        exact,
        type_rank: u8::from(!type_match),
        band,
        dist,
        method,
        fine: RankKey::fine(score),
        len: RankKey::len_key(score, name.len()),
        ..Default::default()
    };
    key.encode(name)
}
