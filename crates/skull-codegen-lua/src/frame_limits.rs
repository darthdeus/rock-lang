//! Check LuaJIT parse-time limits against the emitted body.
//!
//! The model is a lower bound: count only widths provable from emission, so
//! a chunk LuaJIT can load is never rejected.

use crate::control::{ControlLines, Target};
use crate::lua_line_starts_in_code;
use comfy_la_arena::Idx;
use rustc_hash::FxHashMap as HashMap;
use skull_mir::mir::{Block, Inst, MirFunc, Term};

pub(crate) use skull_mir::lua_interop::{
    LUAJIT_CALL_BASE_SLOTS as CALL_BASE_SLOTS, LUAJIT_FRAME_SLOT_LIMIT as LJ_MAX_SLOTS,
    LUAJIT_MAX_ACTIVE_LOCALS as LJ_MAX_LOCVAR,
};

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BodyOverflow {
    Names(usize),
    Jump(usize),
}

/// A self-move emits nothing, and a `nil` kill may merge with its neighbours.
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

/// `control` describes the body starting at byte `base` of the printer's
/// buffer. A lower bound: only the emitter's own label, jump and
/// register-assignment lines count.
pub(crate) fn measure_body(
    body: &str,
    declared: usize,
    control: &ControlLines,
    base: usize,
) -> Option<BodyOverflow> {
    if body.len() < MIN_MEASURED_BODY_BYTES {
        return None;
    }
    if declared + control.names > LJ_MAX_VSTACK {
        return Some(BodyOverflow::Names(declared + control.names));
    }
    let code_lines = lua_line_starts_in_code(body);
    let mut position = 0;
    let mut control_at = Vec::with_capacity(control.lines.len());
    let mut next_control = control.lines.iter().peekable();
    let mut offset = base;
    for (line_index, line) in body.split_inclusive('\n').enumerate() {
        let start = offset;
        offset += line.len();
        if let Some(&&(at, instructions)) = next_control.peek()
            && at == start
        {
            next_control.next();
            control_at.push(position);
            position += instructions as usize;
        } else if code_lines[line_index] && assigns_a_value(line.trim()) {
            position += 1;
        }
    }
    if control_at.len() != control.lines.len() {
        return None;
    }
    let labels: HashMap<_, _> = control
        .labels
        .iter()
        .map(|&(key, at)| (key, control_at[at as usize]))
        .collect();
    let spans = control.jumps.iter().filter_map(|jump| {
        let label = match jump.target {
            Target::Line(at, instruction) => control_at[at as usize] + instruction as usize,
            Target::Label(key) => *labels.get(&key)?,
        };
        let from = control_at[jump.from as usize];
        Some((label, from + jump.first as usize, from + jump.last as usize))
    });
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
    Slots,
    Locals,
}

impl FrameLimit {
    pub(crate) fn constant(self) -> &'static str {
        match self {
            FrameLimit::Slots => "LJ_MAX_SLOTS",
            FrameLimit::Locals => "LJ_MAX_LOCVAR",
        }
    }

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

#[derive(Clone, Copy, Default)]
pub(crate) struct StatementCost {
    pub(crate) slots: usize,
    pub(crate) call_arity: usize,
    pub(crate) wire_values: usize,
}

pub(crate) struct FrameOverflow {
    pub(crate) limit: FrameLimit,
    pub(crate) count: usize,
    pub(crate) actives: usize,
    /// `None` when the declarations alone are over the cap (always, for locals).
    pub(crate) at: Option<Idx<Inst>>,
    pub(crate) cost: StatementCost,
    pub(crate) wide_calls: Vec<Idx<Inst>>,
}

pub(crate) struct EmittedShape<'a> {
    pub(crate) emits_nothing: &'a [bool],
    pub(crate) results_unassigned: &'a [bool],
    pub(crate) fixed_arity_override: &'a [Option<usize>],
    pub(crate) return_replaced: &'a [bool],
    pub(crate) inlinable: Option<&'a [bool]>,
}

impl EmittedShape<'static> {
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

/// An upper bound on one statement's slots above the locals in scope,
/// ignoring every emitter decision that only narrows a statement.
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

/// Whether this function is so far under both caps that the exact model
/// [`frame_overflow`] cannot reject it, using only upper bounds.
pub(crate) fn fits_without_measuring(
    func: &MirFunc,
    blocks: &[Idx<Block>],
    top_locals: usize,
    loop_count: usize,
    inlinable: &[bool],
) -> bool {
    let actives_upper = top_locals + loop_count * FOR_CONTROL_LOCALS;
    if actives_upper > LJ_MAX_LOCVAR {
        return false;
    }
    let shape = EmittedShape {
        inlinable: Some(inlinable),
        ..EmittedShape::PLAIN
    };
    let mut widest = 0usize;
    for &b in blocks {
        let block = &func.blocks[b];
        for &id in &block.insts {
            let mut slots = statement_slots_upper_bound(&func.insts[id]);
            if !inlinable[raw(id)] {
                slots = slots.max(operand_height(func, &shape, id));
            }
            widest = widest.max(slots);
        }
        widest = widest.max(terminator_height(func, &shape, &block.term));
    }
    actives_upper + widest <= FrameLimit::Slots.max_loadable()
}

/// `None` for non-call forms: an unlisted form only loosens the bound.
fn call_arity(func: &MirFunc, shape: &EmittedShape<'_>, id: Idx<Inst>) -> Option<usize> {
    if let Some(&Some(fixed)) = shape.fixed_arity_override.get(raw(id)) {
        return Some(fixed);
    }
    Some(match &func.insts[id] {
        Inst::Call { args, .. }
        | Inst::CallMulti { args, .. }
        | Inst::CallValue { args, .. }
        | Inst::CallValueMulti { args, .. } => args.len(),
        Inst::MakeValueStruct { fields, .. } | Inst::MaterializeValueStruct { fields, .. } => {
            fields.len()
        }
        // `k, v = next(t, k)`.
        Inst::MapNext { .. } => 2,
        _ => return None,
    })
}

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

/// `actives[raw(block)]` is the number of Lua locals in scope in that block.
/// When both limits break, LOCALS is reported: LuaJIT parses the
/// function-top `local` line before any statement.
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
    use crate::control::Label;

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

    fn measure_raw(body: &str) -> Option<BodyOverflow> {
        let mut control = ControlLines::default();
        control.raw_lua(body, 0);
        measure_body(body, 0, &control, 0)
    }

    #[test]
    fn recorded_loop_backedges_match_luajit_at_the_boundary() {
        let last_fit = MAX_BACKWARD_JUMP - 2;
        for statements in [last_fit, last_fit + 1] {
            let mut body = String::new();
            let mut control = ControlLines::default();
            control.label(body.len(), Label::Header, 0);
            body.push_str("  ::w0::\n");
            body.push_str(&"  r0 = 1\n".repeat(statements));
            let backedge = control.line(body.len(), 2);
            body.push_str("  goto w0\n");
            control.goto(backedge, 1, 1, Label::Header, 0);
            let rejected = measure_body(&body, 0, &control, 0).is_some();
            assert_eq!(rejected, !loads(&body, "  local r0\n"));
            assert_eq!(rejected, statements > last_fit);
        }
    }

    #[test]
    fn a_self_move_and_a_kill_cost_no_measured_instruction() {
        let body = format!(
            "  goto bb1\n{}  ::bb1::\n",
            "  r0 = r0\n  r0=nil\n".repeat(40_000)
        );
        assert_eq!(measure_raw(&body), None);
    }
}
