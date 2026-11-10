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
//! Two further parse caps scale with the LENGTH of a body rather than its
//! width, and are measured on the emitted text ([`measure_body`]): in a flat
//! body every block costs a label and every edge a `goto`; in a structured
//! one the printer reports its control lines and the jumps they compile to
//! ([`ControlLines`]):
//!
//! | cap | constant | message |
//! |---|---|---|
//! | names per function | `LJ_MAX_VSTACK` 65476 | `chunk has more than 65476 local variables` |
//! | jump distance | `BCBIAS_J` 32768 | `control structure too long` |
//!
//! The model is validated against the bundled LuaJIT rather than argued:
//! `crates/skull/tests/wide_value_slot_budget.rs` walks families of real
//! emitted shapes across each boundary and asserts the compiler's verdict
//! equals `loadfile`'s on every member.

use comfy_la_arena::Idx;
use rustc_hash::FxHashMap as HashMap;
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

/// `LJ_MAX_VSTACK` (`lj_def.h`): the parser's stack of locals, labels and
/// `goto`s. Entries stay until the function ends, so scopes do not reclaim them.
pub(crate) const LJ_MAX_VSTACK: usize = 65476;

/// The longest forward jump `jmp_patchins` accepts: `BCBIAS_J - 1`.
pub(crate) const MAX_FORWARD_JUMP: usize = 32767;

/// The longest backward jump: `BCBIAS_J`.
const MAX_BACKWARD_JUMP: usize = 32768;

/// A body shorter than this cannot reach either length cap: each stack entry
/// and each counted instruction is a whole line of at least six bytes.
const MIN_MEASURED_BODY_BYTES: usize = 64 * 1024;

/// A length cap the emitted body breaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BodyOverflow {
    /// Labels, `goto`s and locals on the parser's stack.
    Names(usize),
    /// Instructions a single jump spans, at least.
    Jump(usize),
}

fn label_number(text: &str) -> Option<u32> {
    let digits = text.strip_prefix("bb")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A line that transfers control: its targets, and the instructions it takes
/// at least. A condition can fold away, so only the jumps themselves count;
/// `if .. else ..` also jumps over its else arm.
fn goto_line(line: &str) -> Option<([Option<u32>; 2], usize)> {
    if let Some(rest) = line.strip_prefix("goto ") {
        return Some(([Some(label_number(rest)?), None], 1));
    }
    if let Some(rest) = line.strip_prefix("else goto ") {
        let target = rest.strip_suffix(" end")?;
        return Some(([Some(label_number(target)?), None], 1));
    }
    if !line.starts_with("if ") && !line.starts_with("elseif ") {
        return None;
    }
    if let Some(body) = line.strip_suffix(" end") {
        let (head, otherwise) = body.rsplit_once(" else goto ")?;
        let (_, then) = head.rsplit_once(" then goto ")?;
        return Some((
            [Some(label_number(then)?), Some(label_number(otherwise)?)],
            3,
        ));
    }
    let (_, then) = line.rsplit_once(" then goto ")?;
    Some(([Some(label_number(then)?), None], 1))
}

/// Whether `line` assigns a register or cell a value that takes an instruction:
/// a self-move emits nothing, and a `nil` kill may merge with its neighbours.
fn assigns_a_value(line: &str) -> bool {
    let Some((target, value)) = line.split_once(" = ") else {
        return false;
    };
    let digits = target.strip_prefix(['r', 'c']).unwrap_or("");
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && value != target
        && value != "nil"
}

/// The control lines of a structured body, as its printer wrote them: where
/// each starts, the jump instructions it compiles to, and which jumps go
/// where. Only the printer knows which `end` closes which construct, so the
/// model reads this instead of re-parsing the text.
#[derive(Default)]
pub(crate) struct ControlLines {
    /// Byte offset of each control line in the printer's buffer, and the
    /// jump instructions LuaJIT emits for it at least.
    pub(crate) lines: Vec<(usize, u8)>,
    /// `(from line, instruction in it, to line, instruction in it)`: a jump
    /// from that instruction to the position that many instructions into the
    /// target line.
    pub(crate) jumps: Vec<(u32, u8, u32, u8)>,
    /// Labels, `goto`s and `break`s: entries on LuaJIT's name stack.
    pub(crate) names: usize,
}

impl ControlLines {
    /// Record a control line starting at `offset`; returns its index.
    pub(crate) fn line(&mut self, offset: usize, instructions: u8) -> u32 {
        self.lines.push((offset, instructions));
        (self.lines.len() - 1) as u32
    }
}

/// The first length cap `body` breaks, given the names already `declared` by
/// the parameter list, the top-level `local` line and the counted loops.
/// `control` describes a structured body that starts at byte `base` of the
/// printer's buffer; a flat body passes an empty one.
///
/// A lower bound, like the frame model above: it counts only the emitter's own
/// label, jump and register-assignment lines, and everything else is free.
pub(crate) fn measure_body(
    body: &str,
    declared: usize,
    control: &ControlLines,
    base: usize,
) -> Option<BodyOverflow> {
    if body.len() < MIN_MEASURED_BODY_BYTES {
        return None;
    }
    let mut names = declared + control.names;
    let mut position = 0;
    let mut labels: HashMap<u32, usize> = HashMap::default();
    let mut jumps: Vec<(u32, usize, usize)> = Vec::new();
    // Where each control line starts, in counted instructions.
    let mut control_at: Vec<usize> = Vec::with_capacity(control.lines.len());
    let mut next_control = control.lines.iter().peekable();
    let mut offset = base;
    for line in body.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        if let Some(&&(at, instructions)) = next_control.peek()
            && at == start
        {
            next_control.next();
            control_at.push(position);
            position += instructions as usize;
            continue;
        }
        let line = line.trim();
        if let Some(label) = line
            .strip_prefix("::")
            .and_then(|l| l.strip_suffix("::"))
            .and_then(label_number)
        {
            names += 1;
            labels.insert(label, position);
        } else if let Some((targets, instructions)) = goto_line(line) {
            for target in targets.into_iter().flatten() {
                names += 1;
                jumps.push((target, position, position + instructions - 1));
            }
            position += instructions;
        } else if assigns_a_value(line) {
            position += 1;
        }
    }
    if names > LJ_MAX_VSTACK {
        return Some(BodyOverflow::Names(names));
    }
    let mut spans: Vec<(usize, usize, usize)> = jumps
        .into_iter()
        .filter_map(|(target, first, last)| Some((*labels.get(&target)?, first, last)))
        .collect();
    if control_at.len() == control.lines.len() {
        for &(from, from_at, to, to_at) in &control.jumps {
            let first = control_at[from as usize] + from_at as usize;
            let label = control_at[to as usize] + to_at as usize;
            spans.push((label, first, first));
        }
    }
    let mut longest = None;
    for (label, first, last) in spans {
        let (span, over) = if label > last {
            let span = label - last - 1;
            (span, span > MAX_FORWARD_JUMP)
        } else {
            let span = first.saturating_sub(label) + 1;
            (span, span > MAX_BACKWARD_JUMP)
        };
        if over && longest.is_none_or(|l| span > l) {
            longest = Some(span);
        }
    }
    longest.map(BodyOverflow::Jump)
}

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
    /// Values rendered inside their single use instead of a local.
    pub(crate) inlinable: Option<&'a [bool]>,
}

impl EmittedShape<'static> {
    /// No overrides: every call keeps its arguments and results.
    const PLAIN: Self = EmittedShape {
        emits_nothing: &[],
        results_unassigned: &[],
        fixed_arity_override: &[],
        return_replaced: &[],
        inlinable: None,
    };
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

/// Whether the folded expression `value` renders a call anywhere in its tree.
pub(crate) fn folded_call_within(func: &MirFunc, inlinable: &[bool], value: Idx<Inst>) -> bool {
    if !inlinable[raw(value)] {
        return false;
    }
    let inst = &func.insts[value];
    if call_arity(func, &EmittedShape::PLAIN, value).is_some() {
        return true;
    }
    let mut found = false;
    inst.uses_each(&mut |v| found = found || folded_call_within(func, inlinable, v));
    found
}

/// Slots, from the one it lands in upward, that evaluating `value` occupies.
/// A value in a local or a constant needs only its landing slot. A folded
/// call stacks its callee, frame link and arguments there, and an argument's
/// own folded call stacks again from that argument's slot. Other folded
/// expressions put each non-trivial operand but the last in a temporary.
/// Spellings this does not see (a runtime helper behind an index) only make
/// it lower, the direction the model may err in.
fn folded_height(func: &MirFunc, shape: &EmittedShape<'_>, value: Idx<Inst>) -> usize {
    let Some(inlinable) = shape.inlinable else {
        return 1;
    };
    if !inlinable[raw(value)] || matches!(func.insts[value], Inst::Const(_) | Inst::Load(_)) {
        return 1;
    }
    operand_height(func, shape, value).max(1)
}

/// Slots the statement or folded expression `id` stacks above the first free
/// register, counting its folded operands.
fn operand_height(func: &MirFunc, shape: &EmittedShape<'_>, id: Idx<Inst>) -> usize {
    let inst = &func.insts[id];
    if let Some(arity) = call_arity(func, shape, id) {
        let mut height = CALL_BASE_SLOTS + arity;
        if let Inst::CallValue { callee, .. } | Inst::CallValueMulti { callee, .. } = inst {
            height = height.max(folded_height(func, shape, *callee));
        }
        let mut stack = |i: usize, arg: Idx<Inst>| {
            if i < arity {
                height = height.max(CALL_BASE_SLOTS + i + folded_height(func, shape, arg));
            }
        };
        match inst {
            Inst::Call { args, .. }
            | Inst::CallMulti { args, .. }
            | Inst::CallValue { args, .. }
            | Inst::CallValueMulti { args, .. } => {
                for (i, &arg) in args.iter().enumerate() {
                    stack(i, arg);
                }
            }
            Inst::MakeValueStruct { fields, .. } | Inst::MaterializeValueStruct { fields, .. } => {
                for (i, &(_, arg)) in fields.iter().enumerate() {
                    stack(i, arg);
                }
            }
            _ => {}
        }
        return height;
    }
    let mut height = 0;
    let mut temporaries = 0;
    inst.uses_each(&mut |v| {
        let own = folded_height(func, shape, v);
        height = height.max(temporaries + own);
        if shape.inlinable.is_some_and(|m| m[raw(v)])
            && !matches!(func.insts[v], Inst::Const(_) | Inst::Load(_))
        {
            temporaries += 1;
        }
    });
    height
}

/// Slots a terminator's operands stack above the first free register.
fn terminator_height(func: &MirFunc, shape: &EmittedShape<'_>, term: &Term) -> usize {
    let mut height = 0;
    match term {
        // `do return v0, .., vN end` discharges each value into its own
        // register (`expr_list`).
        Term::Return(values) => {
            for (i, &v) in values.iter().enumerate() {
                height = height.max(i + folded_height(func, shape, v));
            }
        }
        _ => term.uses_each(&mut |v| height = height.max(folded_height(func, shape, v))),
    }
    height
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
    inlinable: &[bool],
) -> bool {
    let actives_upper = top_locals + loop_count * FOR_CONTROL_LOCALS + loop_scoped_locals;
    if actives_upper > LJ_MAX_LOCVAR {
        return false;
    }
    let shape = EmittedShape {
        inlinable: Some(inlinable),
        ..EmittedShape::PLAIN
    };
    let mut widest = 0usize;
    for (id, inst) in func.insts.iter() {
        let mut slots = statement_slots_upper_bound(inst);
        if !inlinable[raw(id)] {
            slots = slots.max(operand_height(func, &shape, id));
        }
        widest = widest.max(slots);
    }
    for (_, block) in func.blocks.iter() {
        widest = widest.max(terminator_height(func, &shape, &block.term));
    }
    actives_upper + widest <= FrameLimit::Slots.max_loadable()
}

/// The fixed arguments the emitter writes for `id`, when it renders as a Lua
/// CALL. `None` for every other form -- an unlisted form only makes the bound
/// looser, never wrong.
fn call_arity(func: &MirFunc, shape: &EmittedShape<'_>, id: Idx<Inst>) -> Option<usize> {
    if let Some(&Some(fixed)) = shape.fixed_arity_override.get(raw(id)) {
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
            let mut cost = arity.map_or(0, |n| CALL_BASE_SLOTS + n).max(received);
            if shape.inlinable.is_some_and(|m| !m[raw(id)]) {
                cost = cost.max(operand_height(func, shape, id));
            }
            if cost <= 1 {
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
        let term = &func.blocks[b].term;
        let height = terminator_height(func, shape, term);
        if !shape.return_replaced[raw(b)] && height > 1 {
            let mut first = None;
            term.uses_each(&mut |v| first = first.or(Some(v)));
            let returned = match term {
                Term::Return(values) => values.len(),
                _ => 0,
            };
            keep(
                &mut slots,
                FrameOverflow {
                    limit: FrameLimit::Slots,
                    count: active + height,
                    actives: active,
                    at: first,
                    cost: StatementCost {
                        slots: height,
                        call_arity: 0,
                        wire_values: returned,
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

#[cfg(test)]
mod body_measure_tests {
    use super::*;

    /// Whether LuaJIT accepts the function, asked of LuaJIT itself.
    fn loads(body: &str, locals: &str) -> bool {
        let chunk = format!("f = function()\n{locals}{body}end\n");
        let dir = std::env::temp_dir().join(format!(
            "rock_body_measure_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("chunk.lua");
        std::fs::write(&path, chunk).expect("write chunk");
        let executable = std::env::var_os("ROCK_LUAJIT").unwrap_or_else(|| "luajit".into());
        let output = std::process::Command::new(executable)
            .arg("-e")
            .arg(format!(
                "local f, e = loadfile({:?}); if not f then io.stderr:write(e) os.exit(1) end",
                path.to_string_lossy()
            ))
            .output()
            .expect("spawn luajit");
        let _ = std::fs::remove_dir_all(&dir);
        output.status.success()
    }

    fn agrees(body: &str, locals: &str) -> bool {
        let rejected = measure_body(body, 0, &ControlLines::default(), 0).is_some();
        assert_eq!(
            rejected,
            !loads(body, locals),
            "verdict differs from LuaJIT"
        );
        rejected
    }

    fn label_goto_pairs(pairs: usize) -> String {
        let mut body = String::new();
        for i in 0..pairs {
            body.push_str(&format!("  goto bb{i}\n  ::bb{i}::\n"));
        }
        body
    }

    #[test]
    fn the_name_cap_matches_luajit_at_its_boundary() {
        let last_fit = LJ_MAX_VSTACK / 2;
        assert!(!agrees(&label_goto_pairs(last_fit), ""));
        assert!(agrees(&label_goto_pairs(last_fit + 1), ""));
    }

    fn jump_over(statements: usize) -> String {
        let mut body = String::from("  goto bb1\n");
        body.push_str(&"  r0 = 1\n".repeat(statements));
        body.push_str("  ::bb1::\n");
        body
    }

    #[test]
    fn the_forward_jump_cap_matches_luajit_at_its_boundary() {
        let locals = "  local r0\n";
        assert!(!agrees(&jump_over(MAX_FORWARD_JUMP), locals));
        assert!(agrees(&jump_over(MAX_FORWARD_JUMP + 1), locals));
    }

    #[test]
    fn a_self_move_and_a_kill_cost_no_measured_instruction() {
        let body = format!(
            "  goto bb1\n{}  ::bb1::\n",
            "  r0 = r0\n  r0 = nil\n".repeat(40_000)
        );
        assert_eq!(measure_body(&body, 0, &ControlLines::default(), 0), None);
    }

    #[test]
    fn jump_text_inside_a_string_is_not_a_jump() {
        let body = "  print(\"goto bb1\")\n".repeat(70_000);
        assert_eq!(measure_body(&body, 0, &ControlLines::default(), 0), None);
    }
}
