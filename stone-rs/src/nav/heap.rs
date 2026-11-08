use super::*;

// One move of a sift-up of entry (f, g, n) from slot `i`: returns the parent
// slot the entry must keep climbing from, or -1 once it has been placed.
#[inline]
pub(super) fn nav_heap_up_step(
    hf: &mut IdxVec<int>,
    hg: &mut IdxVec<int>,
    hn: &mut IdxVec<int>,
    i: int,
    f: int,
    g: int,
    n: int,
) -> int {
    let mut next = -1;
    if i > 0 {
        let parent = (i - 1) / 2;
        let pf = hf[parent];
        let pg = hg[parent];
        let pn = hn[parent];
        if !(pf < f || (pf == f && (pg > g || (pg == g && pn < n)))) {
            hf[i] = pf;
            hg[i] = pg;
            hn[i] = pn;
            next = parent;
        }
    }
    if next < 0 {
        hf[i] = f;
        hg[i] = g;
        hn[i] = n;
    }
    next
}

// One move of a sift-down within `count` entries, or -1 after placement.
#[inline]
pub(super) fn nav_heap_down_step(
    hf: &mut IdxVec<int>,
    hg: &mut IdxVec<int>,
    hn: &mut IdxVec<int>,
    count: int,
    i: int,
    f: int,
    g: int,
    n: int,
) -> int {
    let mut next = -1;
    let mut child = i * 2 + 1;
    if child < count {
        let mut cf = hf[child];
        let mut cg = hg[child];
        let mut cn = hn[child];
        let r = child + 1;
        if r < count {
            let rf = hf[r];
            let rg = hg[r];
            let rn = hn[r];
            if rf < cf || (rf == cf && (rg > cg || (rg == cg && rn < cn))) {
                child = r;
                cf = rf;
                cg = rg;
                cn = rn;
            }
        }
        if !(f < cf || (f == cf && (g > cg || (g == cg && n <= cn)))) {
            hf[i] = cf;
            hg[i] = cg;
            hn[i] = cn;
            next = child;
        }
    }
    if next < 0 {
        hf[i] = f;
        hg[i] = g;
        hn[i] = n;
    }
    next
}

// Makes sure slot `h.count` exists; storage grows only past the peak.
pub(super) fn nav_heap_reserve_slot(h: &mut NavHeap) {
    if h.count == h.f.len() {
        h.f.push(0);
        h.g.push(0);
        h.n.push(0);
    }
}

pub(super) fn nav_heap_push(h: &mut NavHeap, f: int, g: int, n: int) {
    nav_heap_reserve_slot(h);
    let mut i = h.count;
    h.count += 1;
    let (hf, hg, hn) = (&mut h.f, &mut h.g, &mut h.n);
    while i >= 0 {
        i = nav_heap_up_step(hf, hg, hn, i, f, g, n);
    }
}
