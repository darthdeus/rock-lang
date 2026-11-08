//! LuaJIT's two PARSE-time frame limits, modelled over the emitter's own final
//! local and slot assignment.
//!
//! Valid Rock can emit a Lua function LuaJIT refuses to LOAD. Both caps live in
//! `lj_parse.c` and fire before a single byte of the program runs, so without
//! this check the user sees a Lua chunk position and no Rock location:
//!
//! | cap | constant | check | message |
//! |---|---|---|---|
//! | frame registers | `LJ_MAX_SLOTS` 250 | `bcreg_bump`: `freereg + n >= 250` | `function or expression too complex near '<r>'` |
//! | active locals | `LJ_MAX_LOCVAR` 200 | `var_new`: `nactvar + n >= 200` | `function at line N has more than 200 local variables` |
//!
//! The third parse cap, `LJ_MAX_UPVAL` (60), is NOT reachable from valid Rock:
//! the only upvalues an emitted body has are its bound callees plus the global
//! storage cell, and [`crate::MAX_BOUND_CALLEES`] already truncates that list
//! to 60 (59 when the storage cell is captured) by construction, leaving the
//! rest as global lookups.
//!
//! # What this models, and in which direction it errs
//!
//! Exactness in ONE direction is mandatory: every program LuaJIT loads today
//! must still compile. This is therefore a proven LOWER BOUND of LuaJIT's own
//! count -- it can MISS an overflow (which keeps today's load-time error) and
//! can never invent one.
//!
//! The bound rests on two facts about LuaJIT's parser:
//!
//! 1. At the start of a statement `freereg == nactvar`, the number of locals in
//!    scope there. Every register a sub-expression takes is reserved ABOVE
//!    that, so a statement's frame high-water mark is at least
//!    `nactvar + <what the statement reserves>`.
//! 2. A call `f(a1..an)` reserves, in order, one register for the callee
//!    (`expr_primary` -> `expr_tonextreg`), one `LJ_FR2` frame-link register on
//!    a 64-bit build, and one per argument (`expr_list` -> `expr_tonextreg`).
//!    A call nested inside an expression only starts higher, so
//!    `nactvar + 2 + n` is a lower bound for ANY call with `n` fixed arguments.
//!
//! So the statement forms counted here are the ones whose width SCALES with a
//! wide value: a call's argument list, a multi-return receive (`assign_adjust`
//! reserves `nvars - 1` above the call base, peaking at `nactvar + nvars`), and
//! a multi-value `return`. Every other emitted form contributes nothing; that
//! only loses precision.
//!
//! `nactvar` itself comes from the emitter's own declaration decisions -- the
//! parameter list and the single function-top `local` line it just wrote, plus,
//! per block, the four locals a numeric `for` adds (three hidden control
//! variables and the visible one) and that loop's body-scoped `local r<N>`
//! line. Nothing else this printer emits opens a scope or declares a local, so
//! this is `nactvar` itself rather than a second estimate of it.
//!
//! The model is validated against the bundled LuaJIT rather than argued:
//! `crates/skull/tests/wide_value_slot_budget.rs` walks families of real
//! emitted shapes across each boundary and asserts the compiler's verdict
//! equals `loadfile`'s on every member.

use comfy_la_arena::Idx;
use skull_mir::mir::{Block, Inst, MirFunc, Term};

/// `LJ_MAX_SLOTS` (`lj_def.h`). `bcreg_bump` rejects at `sz >= LJ_MAX_SLOTS`,
/// so a frame of 249 registers is the widest that loads.
pub(crate) const LJ_MAX_SLOTS: usize = 250;

/// `LJ_MAX_LOCVAR` (`lj_def.h`). `var_new` checks `nactvar + n` while
/// declaring the `n`-th name of a group, so the widest group ending at `A`
/// active locals checks `A - 1`: 200 locals load and 201 do not.
pub(crate) const LJ_MAX_LOCVAR: usize = 200;

/// Registers a call reserves before its first argument: the callee slot plus
/// the `LJ_FR2` frame-link slot. Every LuaJIT this repository builds against is
/// 64-bit (`LJ_FR2 = 1`); a 32-bit build reserves one less and would turn this
/// term into an upper bound, which is why the assumption is written down here
/// instead of inlined as a magic 2.
pub(crate) const CALL_BASE_SLOTS: usize = 2;

/// Active locals a numeric `for` adds inside its block: `(for index)`,
/// `(for limit)`, `(for step)` and the visible variable (`parse_for_num`).
pub(crate) const FOR_CONTROL_LOCALS: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FrameLimit {
    /// `LJ_MAX_SLOTS` -- "function or expression too complex".
    Slots,
    /// `LJ_MAX_LOCVAR` -- "function at line N has more than 200 local variables".
    Locals,
}

impl FrameLimit {
    pub(crate) fn constant(self) -> &'static str {
        match self {
            FrameLimit::Slots => "LJ_MAX_SLOTS",
            FrameLimit::Locals => "LJ_MAX_LOCVAR",
        }
    }

    /// The widest count that still loads.
    pub(crate) fn max_loadable(self) -> usize {
        match self {
            FrameLimit::Slots => LJ_MAX_SLOTS - 1,
            FrameLimit::Locals => LJ_MAX_LOCVAR,
        }
    }

    pub(crate) fn luajit_message(self) -> &'static str {
        match self {
            FrameLimit::Slots => "function or expression too complex",
            FrameLimit::Locals => "function at line N has more than 200 local variables",
        }
    }

    pub(crate) fn subject(self) -> &'static str {
        match self {
            FrameLimit::Slots => "register slots",
            FrameLimit::Locals => "active local variables",
        }
    }
}

/// What the widest emitted statement costs above the locals in scope there.
#[derive(Clone, Copy, Default)]
pub(crate) struct StatementCost {
    /// Registers the statement's own expressions reserve.
    pub(crate) slots: usize,
    /// Fixed arguments of the statement's call, for the message.
    pub(crate) call_arity: usize,
    /// Values the statement receives into locals or returns, for the message.
    pub(crate) wire_values: usize,
}

/// A frame limit the emitted function breaks, with what the emitter counted.
pub(crate) struct FrameOverflow {
    pub(crate) limit: FrameLimit,
    /// The emitter's own count: register slots, or active locals.
    pub(crate) count: usize,
    /// Locals in scope at the offending statement.
    pub(crate) actives: usize,
    /// The instruction whose statement reaches `count`. `None` when the
    /// declarations alone are over the cap, which is what the locals limit
    /// always is.
    pub(crate) at: Option<Idx<Inst>>,
    pub(crate) cost: StatementCost,
    /// The function's widest emitted call statements, widest first. These are
    /// what a wide value costs wherever it crosses a call, so they are the
    /// contributors worth naming even when the overflow is the declaration
    /// line rather than any one statement.
    pub(crate) wide_calls: Vec<Idx<Inst>>,
}

/// Per-instruction statement shape the frame model needs from the emitter, so
/// it reads the emitter's decisions instead of re-deriving them.
pub(crate) struct EmittedShape<'a> {
    /// Instructions that emit no text at their definition site: a const-hoisted
    /// value constructor (a module-scope global is referenced instead), and
    /// every instruction of a counted loop's header and latch (subsumed by the
    /// `for` line).
    pub(crate) emits_nothing: &'a [bool],
    /// Multi-return calls whose results are NOT assigned to locals: the
    /// producer of a call-argument forward (spliced into the consumer's
    /// argument list) and the call a tail forward returns directly.
    pub(crate) results_unassigned: &'a [bool],
    /// Calls whose emitted argument list is shorter than `args`: a
    /// call-argument-forward consumer spells `f(prefix.., g(..))`, so only
    /// `prefix` are fixed arguments of `f`.
    pub(crate) fixed_arity_override: &'a [Option<usize>],
    /// Blocks whose `Term::Return` is replaced by a tail call, so the returned
    /// values never occupy registers of their own.
    pub(crate) return_replaced: &'a [bool],
}

fn raw<T>(i: Idx<T>) -> usize {
    u32::from(i.into_raw()) as usize
}

/// An UPPER bound on the slots one statement can reserve above the locals in
/// scope, ignoring every emitter decision that only makes a statement narrower
/// (a spliced constructor, a forwarded producer, a suppressed return). Used by
/// [`fits_without_measuring`], which is the allocation-free fast path: every
/// body in the corpus is far enough under both caps that the exact walk never
/// has to run.
fn statement_slots_upper_bound(inst: &Inst) -> usize {
    match inst {
        Inst::Call { args, .. } | Inst::CallValue { args, .. } => CALL_BASE_SLOTS + args.len(),
        Inst::CallMulti { args, dests, .. } | Inst::CallValueMulti { args, dests, .. } => {
            (CALL_BASE_SLOTS + args.len()).max(dests.len())
        }
        Inst::MakeValueStruct { fields, .. } | Inst::MaterializeValueStruct { fields, .. } => {
            CALL_BASE_SLOTS + fields.len()
        }
        Inst::MapNext { .. } => CALL_BASE_SLOTS + 2,
        _ => 0,
    }
}

/// Whether this function is so far under both caps that the exact model cannot
/// possibly reject it. One allocation-free pass over the instructions, against
/// an upper bound of what [`frame_overflow`] would compute:
/// `actives_upper` is the whole loop nest's declarations summed as if every
/// loop enclosed every block, and the widest statement is taken without any of
/// the narrowing the emitter applies.
///
/// This is what keeps the check off the compile-time budget: a large project emits
/// tens of thousands of bodies and none of them comes near 200 locals, so the
/// side tables [`frame_overflow`] needs are never allocated.
pub(crate) fn fits_without_measuring(
    func: &MirFunc,
    top_locals: usize,
    loop_count: usize,
    loop_scoped_locals: usize,
) -> bool {
    let actives_upper = top_locals + loop_count * FOR_CONTROL_LOCALS + loop_scoped_locals;
    if actives_upper > LJ_MAX_LOCVAR {
        return false;
    }
    let mut widest = 0usize;
    for (_, inst) in func.insts.iter() {
        let slots = statement_slots_upper_bound(inst);
        if slots > widest {
            widest = slots;
        }
    }
    for (_, block) in func.blocks.iter() {
        if let Term::Return(values) = &block.term
            && values.len() > widest
        {
            widest = values.len();
        }
    }
    actives_upper + widest <= FrameLimit::Slots.max_loadable()
}

/// The fixed arguments the emitter writes for `id`, when it renders as a Lua
/// CALL. `None` for every other form -- an unlisted form only makes the bound
/// looser, never wrong.
fn call_arity(func: &MirFunc, shape: &EmittedShape<'_>, id: Idx<Inst>) -> Option<usize> {
    if let Some(fixed) = shape.fixed_arity_override[raw(id)] {
        return Some(fixed);
    }
    Some(match &func.insts[id] {
        Inst::Call { args, .. }
        | Inst::CallMulti { args, .. }
        | Inst::CallValue { args, .. }
        | Inst::CallValueMulti { args, .. } => args.len(),
        // `<Name>_ct(f0, .., fN)` / `<Name>_vt(..)`: the constructor's arity is
        // the value's leaf count, which is exactly what scales.
        Inst::MakeValueStruct { fields, .. } | Inst::MaterializeValueStruct { fields, .. } => {
            fields.len()
        }
        // `k, v = next(t, k)`.
        Inst::MapNext { .. } => 2,
        _ => return None,
    })
}

/// Values a multi-assign statement receives into locals (`d0, .., dN = f(..)`).
fn receive_width(func: &MirFunc, shape: &EmittedShape<'_>, id: Idx<Inst>) -> usize {
    if shape.results_unassigned[raw(id)] {
        return 0;
    }
    match &func.insts[id] {
        Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. } => dests.len(),
        Inst::MapNext { .. } => 2,
        _ => 0,
    }
}

/// The frame limit this emitted function breaks, if any. `actives[raw(block)]`
/// is the number of Lua locals in scope inside that block, from the emitter's
/// own declarations; `blocks` are the blocks it emits, in emission order.
///
/// When both limits are broken the LOCALS one is reported: the function-top
/// `local` line is parsed before any statement, so that is the error LuaJIT
/// would print.
pub(crate) fn frame_overflow(
    func: &MirFunc,
    blocks: &[Idx<Block>],
    actives: &[usize],
    shape: &EmittedShape<'_>,
) -> Option<FrameOverflow> {
    let mut locals: Option<FrameOverflow> = None;
    let mut slots: Option<FrameOverflow> = None;
    let mut wide: Vec<(usize, Idx<Inst>)> = Vec::new();
    let keep = |slot: &mut Option<FrameOverflow>, candidate: FrameOverflow| {
        if candidate.count <= candidate.limit.max_loadable() {
            return;
        }
        if slot.as_ref().map_or(true, |w| candidate.count > w.count) {
            *slot = Some(candidate);
        }
    };

    for &b in blocks {
        let active = actives[raw(b)];
        keep(
            &mut locals,
            FrameOverflow {
                limit: FrameLimit::Locals,
                count: active,
                actives: active,
                at: None,
                cost: StatementCost::default(),
                wide_calls: Vec::new(),
            },
        );
        for &id in &func.blocks[b].insts {
            if shape.emits_nothing[raw(id)] {
                continue;
            }
            let arity = call_arity(func, shape, id);
            let received = receive_width(func, shape, id);
            let cost = arity.map_or(0, |n| CALL_BASE_SLOTS + n).max(received);
            if cost == 0 {
                continue;
            }
            wide.push((cost, id));
            keep(
                &mut slots,
                FrameOverflow {
                    limit: FrameLimit::Slots,
                    count: active + cost,
                    actives: active,
                    at: Some(id),
                    cost: StatementCost {
                        slots: cost,
                        call_arity: arity.unwrap_or(0),
                        wire_values: received,
                    },
                    wide_calls: Vec::new(),
                },
            );
        }
        // `do return v0, .., vN end` discharges each value into its own
        // register (`expr_list`).
        if !shape.return_replaced[raw(b)]
            && let Term::Return(values) = &func.blocks[b].term
            && values.len() > 1
        {
            keep(
                &mut slots,
                FrameOverflow {
                    limit: FrameLimit::Slots,
                    count: active + values.len(),
                    actives: active,
                    at: values.first().copied(),
                    cost: StatementCost {
                        slots: values.len(),
                        call_arity: 0,
                        wire_values: values.len(),
                    },
                    wide_calls: Vec::new(),
                },
            );
        }
    }
    let mut overflow = locals.or(slots)?;
    wide.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    wide.truncate(8);
    overflow.wide_calls = wide.into_iter().map(|(_, id)| id).collect();
    Some(overflow)
}
