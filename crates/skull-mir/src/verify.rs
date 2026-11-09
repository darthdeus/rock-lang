//! A structural MIR verifier: the invariants every pass and backend assumes,
//! checked between passes so a broken body is caught at the pass that broke
//! it instead of as a panic (or wrong code) deep in a backend.
//!
//! The walk covers what a backend renders: the blocks reachable from `entry`,
//! from each `coro_defers` entry and from the entry of every `Inst::Lambda`
//! (or `CoroHook` lambda) those blocks name. The arena itself is not walked: a
//! splice drops a consumed inst from its block but leaves it in the arena, and
//! a hoisted body is a clone of its parent's whole arena.

use crate::mir::{Block, Cell, Inst, Lit, MirFunc, Term};
use comfy_la_arena::{Arena, Idx};
use std::sync::atomic::{AtomicU8, Ordering};
use ustr::Ustr;

const CHECKS_BUILD_DEFAULT: u8 = 0;
const CHECKS_ON: u8 = 1;
const CHECKS_OFF: u8 = 2;

/// Whether the consistency checks that only ever catch compiler bugs run in
/// this process: this verifier between passes, and lowering's audit of the
/// concrete plans it was handed. The build default is on with debug
/// assertions, so every test exercises them; the `rock` binary turns them
/// off for an ordinary compile (they are a measurable share of every body's
/// time in a dev build) unless `ROCK_MIR_CHECKS` asks for them.
static CHECKS: AtomicU8 = AtomicU8::new(CHECKS_BUILD_DEFAULT);

pub fn checks_enabled() -> bool {
    match CHECKS.load(Ordering::Relaxed) {
        CHECKS_ON => true,
        CHECKS_OFF => false,
        _ => cfg!(debug_assertions),
    }
}

/// Force the checks on or off for the rest of the process; `None` restores
/// the build default.
pub fn set_checks(enabled: Option<bool>) {
    let state = match enabled {
        Some(true) => CHECKS_ON,
        Some(false) => CHECKS_OFF,
        None => CHECKS_BUILD_DEFAULT,
    };
    CHECKS.store(state, Ordering::Relaxed);
}

/// The setting a `ROCK_MIR_CHECKS` value asks for: unset leaves the choice
/// to the caller, `0`, `off` or `false` disable, anything else enables.
pub fn checks_requested(setting: Option<&str>) -> Option<bool> {
    let value = setting?.trim();
    Some(!matches!(
        value.to_ascii_lowercase().as_str(),
        "0" | "off" | "false" | "no"
    ))
}

/// What the passes run so far promise. Each flag forbids the instructions the
/// pass that sets it removes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stage {
    /// Closure conversion ran: no `Inst::Lambda` remains in any body.
    pub closures_converted: bool,
    /// The residual-bound-call gate passed: no `Inst::CallBoundMulti` remains.
    pub bound_calls_resolved: bool,
}

impl Stage {
    pub const LOWERED: Stage = Stage {
        closures_converted: false,
        bound_calls_resolved: false,
    };
    pub const CONVERTED: Stage = Stage {
        closures_converted: true,
        bound_calls_resolved: false,
    };
    pub const EMIT_READY: Stage = Stage {
        closures_converted: true,
        bound_calls_resolved: true,
    };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The body (root or nested) the violation is in.
    pub func: Ustr,
    pub block: Option<Idx<Block>>,
    pub inst: Option<Idx<Inst>>,
    pub message: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}`", self.func)?;
        if let Some(b) = self.block {
            write!(f, " bb{}", b.into_raw().into_u32())?;
        }
        if let Some(i) = self.inst {
            write!(f, " v{}", i.into_raw().into_u32())?;
        }
        write!(f, ": {}", self.message)
    }
}

/// Check `mf` and every body in its `nested_fns` tree against the structural
/// invariants and the postconditions `stage` promises.
pub fn verify(mf: &MirFunc, stage: Stage) -> Vec<Violation> {
    let mut out = Vec::new();
    verify_into(mf, stage, &mut out);
    out
}

/// [`verify`], panicking with every violation and `what` (the pass that just
/// ran) when there is any. For debug-build pass hooks.
pub fn assert_verified(mf: &MirFunc, stage: Stage, what: &dyn std::fmt::Display) {
    let violations = verify(mf, stage);
    if violations.is_empty() {
        return;
    }
    let mut message = format!(
        "MIR verification failed after {what} ({} violation(s)):",
        violations.len()
    );
    for v in violations.iter().take(20) {
        message.push_str("\n  ");
        message.push_str(&v.to_string());
    }
    panic!("{message}");
}

fn verify_into(mf: &MirFunc, stage: Stage, out: &mut Vec<Violation>) {
    Verifier::new(mf, stage, out).run();
    for nested in &mf.nested_fns {
        verify_into(nested, stage, out);
    }
}

fn raw<T>(idx: Idx<T>) -> usize {
    idx.into_raw().into_u32() as usize
}

fn in_bounds<T>(arena: &Arena<T>, idx: Idx<T>) -> bool {
    raw(idx) < arena.len()
}

fn get<T>(arena: &Arena<T>, idx: Idx<T>) -> Option<&T> {
    in_bounds(arena, idx).then(|| &arena[idx])
}

const UNPLACED: u32 = u32::MAX;

struct Verifier<'a> {
    mf: &'a MirFunc,
    stage: Stage,
    out: &'a mut Vec<Violation>,
    /// The live blocks, in visit order.
    live: Vec<Idx<Block>>,
    /// Per block: the index of the region (root, defer or lambda body) it
    /// was reached from, or `UNPLACED`.
    region: Vec<u32>,
    /// Per inst: the raw id of the block that lists it, or `UNPLACED`.
    placed: Vec<u32>,
    /// Per inst: whether the use check has passed its definition yet.
    defined: Vec<bool>,
}

impl<'a> Verifier<'a> {
    fn new(mf: &'a MirFunc, stage: Stage, out: &'a mut Vec<Violation>) -> Self {
        Verifier {
            mf,
            stage,
            out,
            live: Vec::new(),
            region: vec![UNPLACED; mf.blocks.len()],
            placed: vec![UNPLACED; mf.insts.len()],
            defined: vec![false; mf.insts.len()],
        }
    }

    fn report(&mut self, block: Option<Idx<Block>>, inst: Option<Idx<Inst>>, message: String) {
        self.out.push(Violation {
            func: self.mf.name,
            block,
            inst,
            message,
        });
    }

    fn run(&mut self) {
        let mf = self.mf;
        for &p in &mf.params {
            self.check_cell(None, None, p, "parameter");
        }
        if let Some(cell) = mf.resume_arg {
            self.check_cell(None, None, cell, "resume argument");
        }
        if !in_bounds(&mf.blocks, mf.entry) {
            self.report(None, None, "entry block does not exist".into());
            return;
        }
        let mut regions = vec![mf.entry];
        for &(flag, entry) in &mf.coro_defers {
            self.check_cell(None, None, flag, "defer flag");
            if in_bounds(&mf.blocks, entry) {
                regions.push(entry);
            } else {
                self.report(None, None, "defer entry block does not exist".into());
            }
        }
        for &header in &mf.for_iter_headers {
            if !in_bounds(&mf.blocks, header) {
                self.report(None, None, "for-iter header block does not exist".into());
            }
        }
        self.place(regions);
        let live = std::mem::take(&mut self.live);
        for &bid in &live {
            self.check_block(bid);
        }
    }

    /// Visit every live region, recording which block lists each inst.
    fn place(&mut self, mut regions: Vec<Idx<Block>>) {
        let mf = self.mf;
        let mut stack = Vec::new();
        let mut next_region = 0;
        while let Some(entry) = regions.pop() {
            if self.region[raw(entry)] != UNPLACED {
                continue;
            }
            let region = next_region;
            next_region += 1;
            self.region[raw(entry)] = region;
            stack.push(entry);
            while let Some(bid) = stack.pop() {
                self.live.push(bid);
                let block = &mf.blocks[bid];
                for &iid in &block.insts {
                    if !in_bounds(&mf.insts, iid) {
                        self.report(
                            Some(bid),
                            None,
                            "block lists an inst that does not exist".into(),
                        );
                        continue;
                    }
                    let slot = &mut self.placed[raw(iid)];
                    if *slot != UNPLACED {
                        self.report(Some(bid), Some(iid), "inst is listed more than once".into());
                        continue;
                    }
                    *slot = raw(bid) as u32;
                    let lambda = match &mf.insts[iid] {
                        Inst::CoroHook { lambda, .. } if in_bounds(&mf.insts, *lambda) => *lambda,
                        _ => iid,
                    };
                    if let Inst::Lambda { entry, .. } = &mf.insts[lambda] {
                        if in_bounds(&mf.blocks, *entry) {
                            regions.push(*entry);
                        }
                    }
                }
                let mut missing = false;
                let mut shared = false;
                crate::cfg::successors(&block.term, |s| {
                    if !in_bounds(&mf.blocks, s) {
                        missing = true;
                        return;
                    }
                    let slot = &mut self.region[raw(s)];
                    if *slot == UNPLACED {
                        *slot = region;
                        stack.push(s);
                    } else if *slot != region {
                        shared = true;
                    }
                });
                if missing {
                    self.report(
                        Some(bid),
                        None,
                        "terminator targets a block that does not exist".into(),
                    );
                }
                if shared {
                    self.report(
                        Some(bid),
                        None,
                        "terminator branches into another region".into(),
                    );
                }
            }
        }
    }

    fn check_cell(
        &mut self,
        block: Option<Idx<Block>>,
        inst: Option<Idx<Inst>>,
        cell: Idx<Cell>,
        what: &str,
    ) {
        if !in_bounds(&self.mf.cells, cell) {
            self.report(block, inst, format!("{what} cell does not exist"));
        }
    }

    /// An operand must name a value-producing inst listed in a live block of
    /// the same region (a lambda body's values are its own), and earlier when
    /// that block is the user's.
    fn check_operand(&mut self, bid: Idx<Block>, user: Option<Idx<Inst>>, operand: Idx<Inst>) {
        let mf = self.mf;
        let o = raw(operand);
        if o >= mf.insts.len() {
            self.report(Some(bid), user, format!("operand v{o} does not exist"));
            return;
        }
        let placed = self.placed[o];
        if placed == UNPLACED {
            self.report(
                Some(bid),
                user,
                format!("operand v{o} is not listed in any live block"),
            );
        } else if placed != raw(bid) as u32 {
            if self.region[placed as usize] != self.region[raw(bid)] {
                self.report(
                    Some(bid),
                    user,
                    format!("operand v{o} is defined in bb{placed}, in another region"),
                );
            }
        } else if !self.defined[o] {
            self.report(
                Some(bid),
                user,
                format!("operand v{o} is used before it is defined"),
            );
        }
        if !mf.insts[operand].produces_value() {
            self.report(
                Some(bid),
                user,
                format!("operand v{o} is an effect with no value"),
            );
        }
    }

    fn check_block(&mut self, bid: Idx<Block>) {
        let mf = self.mf;
        let block = &mf.blocks[bid];
        for &iid in &block.insts {
            if !in_bounds(&mf.insts, iid) || self.placed[raw(iid)] != raw(bid) as u32 {
                continue;
            }
            let inst = &mf.insts[iid];
            let mut operands = Vec::new();
            inst.uses_each(&mut |o| operands.push(o));
            for &o in &operands {
                self.check_operand(bid, Some(iid), o);
            }
            let mut cells = Vec::new();
            inst.cells_each(&mut |c| cells.push(c));
            for c in cells {
                self.check_cell(Some(bid), Some(iid), c, "operand");
            }
            self.check_inst(bid, iid, inst);
            self.defined[raw(iid)] = true;
        }
        let mut operands = Vec::new();
        block.term.uses_each(&mut |o| operands.push(o));
        for o in operands {
            self.check_operand(bid, None, o);
        }
        match &block.term {
            Term::Yield { .. } if !mf.is_coroutine => {
                self.report(
                    Some(bid),
                    None,
                    "`yield` outside an untransformed co fn".into(),
                );
            }
            Term::Branch { cond, .. } => {
                if let Some(lit) = self.const_lit(*cond) {
                    // A folded absent presence test branches on nil, which
                    // both dynamic targets read as false.
                    if !matches!(lit, Lit::Bool(_) | Lit::Nil) {
                        self.report(
                            Some(bid),
                            None,
                            format!("branch on a non-bool constant {lit:?}"),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn const_lit(&self, iid: Idx<Inst>) -> Option<&'a Lit> {
        match get(&self.mf.insts, iid)? {
            Inst::Const(lit) => Some(lit),
            _ => None,
        }
    }

    fn check_inst(&mut self, bid: Idx<Block>, iid: Idx<Inst>, inst: &Inst) {
        let mf = self.mf;
        let here = (Some(bid), Some(iid));
        match inst {
            Inst::Lambda { entry, .. } => {
                if self.stage.closures_converted {
                    self.report(here.0, here.1, "lambda survived closure conversion".into());
                }
                if !in_bounds(&mf.blocks, *entry) {
                    self.report(here.0, here.1, "lambda entry block does not exist".into());
                }
            }
            Inst::CoroHook { lambda, .. } => {
                if !mf.is_coroutine {
                    self.report(
                        here.0,
                        here.1,
                        "lifecycle hook outside an untransformed co fn".into(),
                    );
                }
                if !matches!(get(&mf.insts, *lambda), Some(Inst::Lambda { .. })) {
                    self.report(
                        here.0,
                        here.1,
                        "lifecycle hook does not name a lambda".into(),
                    );
                }
            }
            Inst::CoroSetInner(_) if !mf.is_coroutine => {
                self.report(
                    here.0,
                    here.1,
                    "await bookkeeping outside an untransformed co fn".into(),
                );
            }
            Inst::CallBoundMulti { .. } if self.stage.bound_calls_resolved => {
                self.report(
                    here.0,
                    here.1,
                    "placeholder bound call reached emission".into(),
                );
            }
            _ => self.check_literal_kinds(bid, iid, inst),
        }
    }

    /// A typed arithmetic or comparison op over a literal of another kind.
    /// Only literal operands are checked: anything else needs a type lookup.
    fn check_literal_kinds(&mut self, bid: Idx<Block>, iid: Idx<Inst>, inst: &Inst) {
        #[derive(Clone, Copy, Debug)]
        enum Kind {
            Int,
            Uint,
            Float,
        }
        let (kind, a, b) = match *inst {
            Inst::IntAdd(a, b)
            | Inst::IntSub(a, b)
            | Inst::IntMul(a, b)
            | Inst::IntDiv(a, b)
            | Inst::IntMod(a, b)
            | Inst::IntLt(a, b)
            | Inst::IntLeq(a, b)
            | Inst::IntGt(a, b)
            | Inst::IntGeq(a, b) => (Kind::Int, a, b),
            Inst::UintAdd(a, b)
            | Inst::UintSub(a, b)
            | Inst::UintMul(a, b)
            | Inst::UintDiv(a, b)
            | Inst::UintMod(a, b)
            | Inst::UintLt(a, b)
            | Inst::UintLeq(a, b)
            | Inst::UintGt(a, b)
            | Inst::UintGeq(a, b) => (Kind::Uint, a, b),
            Inst::FloatAdd(a, b)
            | Inst::FloatSub(a, b)
            | Inst::FloatMul(a, b)
            | Inst::FloatDiv(a, b)
            | Inst::FloatMod(a, b)
            | Inst::FloatLt(a, b)
            | Inst::FloatLeq(a, b)
            | Inst::FloatGt(a, b)
            | Inst::FloatGeq(a, b) => (Kind::Float, a, b),
            _ => return,
        };
        for operand in [a, b] {
            let Some(lit) = self.const_lit(operand) else {
                continue;
            };
            // Nil is allowed: copy propagation forwards an absent optional
            // into the arm that only runs when it is present.
            let fits = match kind {
                Kind::Int => matches!(lit, Lit::Int(_) | Lit::Nil),
                Kind::Uint => matches!(lit, Lit::Uint(_) | Lit::Nil),
                Kind::Float => matches!(lit, Lit::Float(_) | Lit::Nil),
            };
            if !fits {
                self.report(
                    Some(bid),
                    Some(iid),
                    format!("{kind:?} op over the literal {lit:?}"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::Lit;

    fn body() -> MirFunc {
        let mut mf = MirFunc::new(Ustr::from("f"));
        let entry = mf.entry();
        let a = mf.emit(entry, Inst::Const(Lit::Int(1)));
        let b = mf.emit(entry, Inst::Const(Lit::Int(2)));
        let sum = mf.emit(entry, Inst::IntAdd(a, b));
        mf.set_term(entry, Term::Return(vec![sum]));
        mf
    }

    fn messages(mf: &MirFunc, stage: Stage) -> Vec<String> {
        verify(mf, stage).into_iter().map(|v| v.message).collect()
    }

    #[test]
    fn checks_setting_parses_off_and_on() {
        assert_eq!(checks_requested(None), None);
        assert_eq!(checks_requested(Some("0")), Some(false));
        assert_eq!(checks_requested(Some("off")), Some(false));
        assert_eq!(checks_requested(Some("1")), Some(true));
        assert_eq!(checks_requested(Some("full")), Some(true));
    }

    #[test]
    fn a_well_formed_body_verifies() {
        assert_eq!(messages(&body(), Stage::EMIT_READY), Vec::<String>::new());
    }

    #[test]
    fn a_value_may_cross_blocks_within_its_region() {
        let mut mf = body();
        let entry = mf.entry();
        let next = mf.new_block();
        let value = mf.blocks[entry].insts[2];
        mf.set_term(entry, Term::Goto(next));
        mf.set_term(next, Term::Return(vec![value]));
        assert_eq!(messages(&mf, Stage::LOWERED), Vec::<String>::new());
    }

    #[test]
    fn a_lambda_body_cannot_use_an_enclosing_value() {
        let mut mf = body();
        let entry = mf.entry();
        let value = mf.blocks[entry].insts[2];
        let lambda_entry = mf.new_block();
        mf.set_term(lambda_entry, Term::Return(vec![value]));
        let lambda = mf.alloc_inst(Inst::Lambda {
            params: vec![],
            entry: lambda_entry,
        });
        mf.blocks[entry].insts.insert(0, lambda);
        let v = raw(value);
        assert_eq!(
            messages(&mf, Stage::LOWERED),
            vec![format!("operand v{v} is defined in bb0, in another region")]
        );
    }

    #[test]
    fn a_use_before_its_definition_is_rejected() {
        let mut mf = body();
        let entry = mf.entry();
        mf.blocks[entry].insts.swap(1, 2);
        let b = raw(mf.blocks[entry].insts[2]);
        assert_eq!(
            messages(&mf, Stage::LOWERED),
            vec![format!("operand v{b} is used before it is defined")]
        );
    }

    #[test]
    fn a_missing_target_is_rejected() {
        let mut mf = body();
        let entry = mf.entry();
        let gone = Idx::from_raw(comfy_la_arena::RawIdx::from_u32(99));
        mf.set_term(entry, Term::Goto(gone));
        assert_eq!(
            messages(&mf, Stage::LOWERED),
            vec!["terminator targets a block that does not exist".to_string()]
        );
    }

    #[test]
    fn an_effect_has_no_value() {
        let mut mf = body();
        let entry = mf.entry();
        let cell = mf.new_cell(None);
        let one = mf.blocks[entry].insts[0];
        let store = mf.emit(entry, Inst::Store { cell, value: one });
        mf.set_term(entry, Term::Return(vec![store]));
        let s = raw(store);
        assert_eq!(
            messages(&mf, Stage::LOWERED),
            vec![format!("operand v{s} is an effect with no value")]
        );
    }

    #[test]
    fn stage_postconditions_reject_residual_lambdas_and_coroutine_ops() {
        let mut mf = body();
        let entry = mf.entry();
        let lambda_entry = mf.new_block();
        let lambda = mf.emit(
            entry,
            Inst::Lambda {
                params: vec![],
                entry: lambda_entry,
            },
        );
        mf.emit(entry, Inst::CoroSetInner(lambda));
        let resume = mf.new_block();
        mf.set_term(
            entry,
            Term::Yield {
                values: vec![],
                resume,
            },
        );
        assert_eq!(
            messages(&mf, Stage::LOWERED),
            vec![
                "await bookkeeping outside an untransformed co fn".to_string(),
                "`yield` outside an untransformed co fn".to_string(),
            ]
        );
        mf.is_coroutine = true;
        assert_eq!(messages(&mf, Stage::LOWERED), Vec::<String>::new());
        assert_eq!(
            messages(&mf, Stage::CONVERTED),
            vec!["lambda survived closure conversion".to_string()]
        );
    }

    #[test]
    fn a_literal_of_the_wrong_kind_is_rejected() {
        let mut mf = body();
        let entry = mf.entry();
        let a = mf.blocks[entry].insts[0];
        mf.insts[a] = Inst::Const(Lit::Float(1.0));
        assert_eq!(
            messages(&mf, Stage::LOWERED),
            vec!["Int op over the literal Float(1.0)".to_string()]
        );
    }
}
