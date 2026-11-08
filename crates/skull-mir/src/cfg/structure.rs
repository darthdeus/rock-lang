//! CFG structuring analysis for the relooper backends (GLSL, JS): immediate
//! post-dominators, natural loops, and range-`for` recognition. Both backends
//! need structured goto-free output and see the same reducible CFGs skull-mir
//! lowers for a tier-0 fn, so the simple iterative algorithms suffice (tens of
//! blocks). What differs per target is passed in as [`LoopShapes`] and
//! [`CountedBound`]; the structured walk over this analysis is
//! [`relooper`](super::relooper).

use super::dense::{bi, bx, collect_loop_body, dominates, idoms, pred_list, rpo, succ_list};
use crate::mir::{Block, Cell, Inst, Lit, MirFunc, Term};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

/// Which natural loops a backend can structure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopShapes {
    /// Only loops with a header-condition test (`while cond`/`for`). A test-less
    /// loop is left unregistered, so the walker rejects it as unstructured.
    TestedOnly,
    /// Also `loop {}` shapes with no dominating test, emitted as a bare
    /// `while (true) { <body> }` whose exits are inline `break`s.
    AllowHeaderless,
}

/// Which range-`for` bounds a backend can render in its loop header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CountedBound {
    /// Any bound value.
    Any,
    /// Only a `Load` (of a hoisted preheader cell) or a const: the counted path
    /// skips the header block, so a bound computed there would reference a temp
    /// that is never emitted.
    InlineOnly,
}

/// One natural loop: its header (back-edge target), the set of blocks in the
/// loop body, and its single exit (the block outside the loop that a body block
/// branches to). Tier-0 `while`/`for` loops are single-exit by construction.
#[derive(Clone)]
pub struct LoopRegion {
    pub header: Idx<Block>,
    pub body: HashSet<Idx<Block>>,
    pub exit: Idx<Block>,
    /// The header-condition test block of a `while cond`/`for` shape: the body
    /// block that dominates every latch and has a lone out-of-loop successor.
    /// `None` for a `loop {}` whose exits are inline `break`s (no test dominates
    /// the back edges, e.g. a `continue` precedes the `break`); registered only
    /// under [`LoopShapes::AllowHeaderless`].
    pub test: Option<Idx<Block>>,
}

pub struct Structure {
    /// Immediate post-dominator per block; `None` = the virtual exit (a diverging
    /// branch has no real join).
    ipdom: Vec<Option<Idx<Block>>>,
    pub loops: HashMap<Idx<Block>, LoopRegion>,
}

impl Structure {
    pub fn ipdom(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        self.ipdom[bx(b)]
    }

    /// A no-analysis placeholder for a walker that never consults ipdoms or
    /// loops (JS trampoline mode: the post-coroutine-transform CFG is
    /// irreducible by construction, so these analyses don't apply).
    pub fn empty(n: usize) -> Structure {
        Structure {
            ipdom: vec![None; n],
            loops: HashMap::default(),
        }
    }
}

pub fn analyze(f: &MirFunc, shapes: LoopShapes) -> Structure {
    let n = f.blocks.len();
    let succ = succ_list(f);
    let preds = pred_list(n, &succ);

    let fwd_rpo = rpo(n, bx(f.entry), &succ);
    let idom = idoms(n, bx(f.entry), &preds, &fwd_rpo);

    let ipdom = post_idoms(f, n, &succ);
    let loops = natural_loops(f, n, &succ, &preds, &idom, shapes);

    Structure { ipdom, loops }
}

/// Immediate post-dominators: dominators on the reversed CFG rooted at a virtual
/// exit (node `n`) that every `Return` block flows into. `Unreachable` blocks do
/// NOT connect to the exit: they mark control the compiler proved never runs (an
/// exhaustive-`match` default, code after a diverging call), so counting them as
/// an exit path would defeat post-dominance of the REAL join -- an exhaustive
/// match's `if/else` chain would then double-walk its join block and reject as
/// unstructured control flow.
fn post_idoms(f: &MirFunc, n: usize, succ: &[Vec<usize>]) -> Vec<Option<Idx<Block>>> {
    let exit = n;
    let nn = n + 1;
    // Reverse graph: edge b->s becomes s->b; every `Return` block gets an edge
    // from the virtual exit.
    let mut rev_adj = vec![Vec::new(); nn];
    let mut rev_preds = vec![Vec::new(); nn];
    for b in 0..n {
        for &s in &succ[b] {
            rev_adj[s].push(b);
        }
        // Reverse predecessors of b are its forward successors.
        rev_preds[b].extend_from_slice(&succ[b]);
        if matches!(f.blocks[bi(b)].term, Term::Return(_)) {
            rev_adj[exit].push(b);
            rev_preds[b].push(exit);
        }
    }

    let rev_rpo = rpo(nn, exit, &rev_adj);
    let pidom = idoms(nn, exit, &rev_preds, &rev_rpo);

    let mut out = vec![None; n];
    for b in 0..n {
        out[b] = match pidom[b] {
            Some(x) if x != exit => Some(bi(x)),
            _ => None,
        };
    }
    out
}

fn natural_loops(
    f: &MirFunc,
    n: usize,
    succ: &[Vec<usize>],
    preds: &[Vec<usize>],
    idom: &[Option<usize>],
    shapes: LoopShapes,
) -> HashMap<Idx<Block>, LoopRegion> {
    // Back edges grouped by header, keeping each latch (back-edge source).
    let mut latches: HashMap<usize, Vec<usize>> = HashMap::default();
    for latch in 0..n {
        if idom[latch].is_none() && latch != bx(f.entry) {
            continue; // unreachable
        }
        for &h in &succ[latch] {
            if dominates(idom, h, latch) {
                latches.entry(h).or_default().push(latch);
            }
        }
    }

    let mut out = HashMap::default();
    for (h, hlatches) in latches {
        let mut body: HashSet<usize> = HashSet::default();
        for &l in &hlatches {
            collect_loop_body(h, l, preds, &mut body);
        }
        // The loop's exit is the out-of-loop successor of the condition test: the
        // shallowest body block that dominates every latch and leaves the loop.
        // (Early `return`/`break` blocks branch out too, but don't dominate the
        // latch or aren't the shallowest, so they are not mistaken for the exit.)
        let mut candidates: Vec<usize> = body
            .iter()
            .copied()
            .filter(|&b| {
                succ[b].iter().any(|s| !body.contains(s))
                    && hlatches.iter().all(|&l| dominates(idom, b, l))
            })
            .collect();
        candidates.sort_by_key(|&b| b);
        let single_test = candidates
            .iter()
            .copied()
            .find(|&c| candidates.iter().all(|&c2| dominates(idom, c, c2)))
            .and_then(|test| {
                let mut exits = succ[test].iter().copied().filter(|s| !body.contains(s));
                let e = exits.next()?;
                exits.next().is_none().then_some((test, e))
            });
        let (exit, test) = match (single_test, shapes) {
            (Some((test, exit)), _) => (exit, Some(bi(test))),
            (None, LoopShapes::TestedOnly) => continue, // the walker rejects
            (None, LoopShapes::AllowHeaderless) => {
                // Structurable only if every out-of-loop edge across the body
                // targets ONE exit block; a multi-exit loop is dropped so the
                // walker rejects cleanly.
                let mut exit_targets: Vec<usize> = body
                    .iter()
                    .flat_map(|&b| succ[b].iter().copied().filter(|s| !body.contains(s)))
                    .collect();
                exit_targets.sort_unstable();
                exit_targets.dedup();
                let [exit] = exit_targets[..] else {
                    continue;
                };
                (exit, None)
            }
        };
        out.insert(
            bi(h),
            LoopRegion {
                header: bi(h),
                body: body.iter().map(|&x| bi(x)).collect(),
                exit: bi(exit),
                test,
            },
        );
    }
    out
}

/// A recognized range-`for` loop (the shape `lower_for_range` emits).
#[derive(Clone, Copy)]
pub struct CountedLoop {
    pub slot: Idx<Cell>,
    pub bound: Idx<Inst>,
    pub inclusive: bool,
    pub body_entry: Idx<Block>,
    pub latch: Idx<Block>,
    pub exit: Idx<Block>,
}

/// Split `st.loops` into range-`for` loops (emitted as a counted `for`) and the
/// rest (the general `while` path). The counted path is valid only when the
/// header's own `Branch` IS the loop test (its else is the real exit): a
/// `while i < N && ..` header also looks like `i < N` with an `i + 1` latch, but
/// its exit lies inside the loop, so that one takes the general path.
pub fn split_loops(
    f: &MirFunc,
    st: &Structure,
    bound: CountedBound,
) -> (
    HashMap<Idx<Block>, CountedLoop>,
    HashMap<Idx<Block>, LoopRegion>,
) {
    let mut counted = HashMap::default();
    let mut general = HashMap::default();
    for (&h, region) in &st.loops {
        match match_counted_loop(f, h, bound) {
            Some(li) if li.exit == region.exit && region.body.contains(&li.latch) => {
                counted.insert(h, li);
            }
            _ => {
                general.insert(h, region.clone());
            }
        }
    }
    (counted, general)
}

/// `Some(CountedLoop)` iff `h` is a `for i in lo..hi` header: a `Branch` on
/// `IntLt/IntLeq(load slot, bound)`, entered by a pure-increment latch
/// (`load slot; 1; add; store slot; goto h`). The `slot` cell must be
/// `loop_binding` -- desugar provenance that only `lower_for_range` sets, so a
/// `while i < n { ..; i = i + 1 }` (manual counter) is NOT mistaken for a
/// range-for and takes the general path instead.
fn match_counted_loop(f: &MirFunc, h: Idx<Block>, bound_rule: CountedBound) -> Option<CountedLoop> {
    let Term::Branch {
        cond,
        then_bb,
        else_bb,
    } = f.blocks[h].term
    else {
        return None;
    };
    let (slot, bound, inclusive) = match &f.insts[cond] {
        Inst::IntLt(a, b) => (load_cell(f, *a)?, *b, false),
        Inst::IntLeq(a, b) => (load_cell(f, *a)?, *b, true),
        _ => return None,
    };
    if !f.cells[slot].loop_binding {
        return None;
    }
    if bound_rule == CountedBound::InlineOnly
        && !matches!(f.insts[bound], Inst::Load(_) | Inst::Const(_))
    {
        return None;
    }
    let mut latch = None;
    for (bid, blk) in f.blocks.iter() {
        if blk.term == Term::Goto(h) && is_incr_latch(f, bid, slot) {
            latch = Some(bid);
        }
    }
    Some(CountedLoop {
        slot,
        bound,
        inclusive,
        body_entry: then_bb,
        latch: latch?,
        exit: else_bb,
    })
}

fn load_cell(f: &MirFunc, iid: Idx<Inst>) -> Option<Idx<Cell>> {
    match f.insts[iid] {
        Inst::Load(c) => Some(c),
        _ => None,
    }
}

/// `p`: `load slot; 1; slot + 1; store slot; goto h` -- a pure `slot += 1` latch.
fn is_incr_latch(f: &MirFunc, p: Idx<Block>, slot: Idx<Cell>) -> bool {
    for &iid in &f.blocks[p].insts {
        if let Inst::Store { cell, value } = f.insts[iid] {
            if cell == slot {
                if let Inst::IntAdd(a, b) = f.insts[value] {
                    let is_one = |x: Idx<Inst>| matches!(f.insts[x], Inst::Const(Lit::Int(1)));
                    let loads_slot =
                        |x: Idx<Inst>| matches!(f.insts[x], Inst::Load(c) if c == slot);
                    return (loads_slot(a) && is_one(b)) || (loads_slot(b) && is_one(a));
                }
                return false;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use comfy_la_arena::RawIdx;
    use ustr::Ustr;

    /// Build a MirFunc with `n` blocks (0 = entry) and the given terminators.
    /// Branch conds are a single shared dummy bool const.
    fn mk(n: usize, terms: Vec<Term>) -> MirFunc {
        let mut f = MirFunc::new(Ustr::from("t"));
        for _ in 1..n {
            f.new_block();
        }
        let _dummy = f.insts.alloc(Inst::Const(Lit::Bool(true)));
        for (i, t) in terms.into_iter().enumerate() {
            f.blocks[bi(i)].term = t;
        }
        f
    }

    fn cond() -> Idx<Inst> {
        Idx::from_raw(RawIdx::from_u32(0))
    }

    fn branch(then_bb: usize, else_bb: usize) -> Term {
        Term::Branch {
            cond: cond(),
            then_bb: bi(then_bb),
            else_bb: bi(else_bb),
        }
    }

    #[test]
    fn diamond() {
        // 0 -> {1, 2} -> 3(return)
        let f = mk(
            4,
            vec![
                branch(1, 2),
                Term::Goto(bi(3)),
                Term::Goto(bi(3)),
                Term::Return(vec![]),
            ],
        );
        let s = analyze(&f, LoopShapes::TestedOnly);
        assert_eq!(s.ipdom(bi(0)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(1)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(2)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(3)), None); // virtual exit
        assert!(s.loops.is_empty());
    }

    #[test]
    fn diverged_arm() {
        // 0 -> {1(return), 2}; 2 -> 3(return). ipdom(0) collapses to virtual exit.
        let f = mk(
            4,
            vec![
                branch(1, 2),
                Term::Return(vec![]),
                Term::Goto(bi(3)),
                Term::Return(vec![]),
            ],
        );
        let s = analyze(&f, LoopShapes::TestedOnly);
        assert_eq!(s.ipdom(bi(0)), None);
        assert_eq!(s.ipdom(bi(2)), Some(bi(3)));
        assert!(s.loops.is_empty());
    }

    #[test]
    fn unreachable_arm_is_not_an_exit_path() {
        // An exhaustive `match` as an expression: both arms store one result
        // cell and jump to a real join, while the dead default arm is a
        // REACHABLE `Unreachable` block (0 -> {1, 2}; 2 -> {3, 4-unreachable};
        // 1 and 3 -> 5; 5 returns). The dead arm must not count as an exit path:
        // the outer branch keeps the real join, and the last test's ipdom is its
        // LIVE arm. Counting the dead arm instead collapses both branches to the
        // virtual exit, both arms absorb the continuation, and the join
        // double-walks into `unstructured control flow`. (`diverged_arm` above
        // is the control: a `Return` arm still collapses.)
        let f = mk(
            6,
            vec![
                branch(1, 2),
                Term::Goto(bi(5)),
                branch(3, 4),
                Term::Goto(bi(5)),
                Term::Unreachable,
                Term::Return(vec![]),
            ],
        );
        let s = analyze(&f, LoopShapes::TestedOnly);
        assert_eq!(s.ipdom(bi(0)), Some(bi(5)));
        assert_eq!(s.ipdom(bi(1)), Some(bi(5)));
        assert_eq!(s.ipdom(bi(2)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(3)), Some(bi(5)));
        assert_eq!(s.ipdom(bi(5)), None); // virtual exit
        assert!(s.loops.is_empty());
    }

    #[test]
    fn nested_loop() {
        // 0 -> 1(outer header) -> {2(inner header), 6(exit,return)}
        // 2 -> {3, 5}; 3 -> 4; 4 -> 2(inner back edge); 5 -> 1(outer back edge)
        let f = mk(
            7,
            vec![
                Term::Goto(bi(1)),
                branch(2, 6),
                branch(3, 5),
                Term::Goto(bi(4)),
                Term::Goto(bi(2)),
                Term::Goto(bi(1)),
                Term::Return(vec![]),
            ],
        );
        for shapes in [LoopShapes::TestedOnly, LoopShapes::AllowHeaderless] {
            let s = analyze(&f, shapes);
            let outer = s.loops.get(&bi(1)).expect("outer loop");
            assert_eq!(outer.exit, bi(6));
            assert_eq!(outer.test, Some(bi(1)));
            assert!(
                outer.body.contains(&bi(1))
                    && outer.body.contains(&bi(5))
                    && outer.body.contains(&bi(2))
            );
            assert!(!outer.body.contains(&bi(6)));
            let inner = s.loops.get(&bi(2)).expect("inner loop");
            assert_eq!(inner.exit, bi(5));
            assert_eq!(inner.test, Some(bi(2)));
            assert!(inner.body.contains(&bi(2)) && inner.body.contains(&bi(4)));
            assert!(!inner.body.contains(&bi(1)) && !inner.body.contains(&bi(5)));
        }
    }

    #[test]
    fn headerless_loop_registers_only_when_allowed() {
        // `loop { if c { continue } ; if d { break } }`:
        // 0 -> 1(header); 1 -> {1(continue), 2}; 2 -> {3(exit), 1}; 3 returns.
        // Two latches (1 and 2), and no block dominating both leaves the loop
        // except 1, whose successors stay inside, so there is no single test.
        let f = mk(
            4,
            vec![
                Term::Goto(bi(1)),
                branch(1, 2),
                branch(3, 1),
                Term::Return(vec![]),
            ],
        );
        assert!(analyze(&f, LoopShapes::TestedOnly).loops.is_empty());
        let s = analyze(&f, LoopShapes::AllowHeaderless);
        let l = s.loops.get(&bi(1)).expect("headerless loop");
        assert_eq!(l.exit, bi(3));
        assert_eq!(l.test, None);
    }
}
