//! Checking LuaJIT's frame limits against the final emission.

use super::*;

/// Reject a body whose emitted Lua function would break one of LuaJIT's
/// parse-time frame caps, with a Rock span in place of LuaJIT's load-time
/// error. See [`frame_limits`] for the model and the direction it errs in.
///
/// Everything it reads is the emitter's own final answer: `top_locals` is the
/// `local` line just written, `color_scope` and `loops` are what the body-scoped
/// declarations and `for` lines come from, and the forward tables are what
/// decides which calls keep their results and which return statements survive.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_frame_limits(
    ws: &Workspace,
    ctx: &CodegenCtx,
    lua_name: &str,
    func: &MirFunc,
    rpo: &[Idx<Block>],
    top_locals: usize,
    loops: &HashMap<Idx<Block>, CountedLoop>,
    color_scope: &[Option<Idx<Block>>],
    hoist: &HashMap<Idx<Inst>, u32>,
    tail_forwards: &[Option<TailForward>],
    call_arg_forwards: &[Option<CallArgForward>],
) {
    // Almost every body is far under both caps, and the exact walk below needs
    // several instruction-sized side tables. Take an allocation-free upper
    // bound first, so a large project pays one linear scan per body rather than
    // five allocations.
    let loop_scoped_locals = color_scope.iter().flatten().count();
    if frame_limits::fits_without_measuring(func, top_locals, loops.len(), loop_scoped_locals) {
        return;
    }

    // Locals in scope inside each block: the function-top declarations, plus,
    // for every enclosing counted loop, its four control locals and its own
    // body-scoped `local r<N>` line. Regions nest, so adding per loop over its
    // region accumulates the whole enclosing nest.
    let mut actives = vec![top_locals; func.blocks.len()];
    let mut loop_locals: HashMap<Idx<Block>, usize> = HashMap::default();
    for header in color_scope.iter().flatten() {
        *loop_locals.entry(*header).or_default() += 1;
    }
    for (header, cl) in loops.iter() {
        let add = frame_limits::FOR_CONTROL_LOCALS + loop_locals.get(header).copied().unwrap_or(0);
        for &b in cl.region.iter().chain(std::iter::once(&cl.latch)) {
            actives[raw(b) as usize] += add;
        }
    }

    let mut emits_nothing = vec![false; func.insts.len()];
    for id in hoist.keys() {
        emits_nothing[raw(*id) as usize] = true;
    }
    for (header, cl) in loops.iter() {
        for &id in func.blocks[*header]
            .insts
            .iter()
            .chain(func.blocks[cl.latch].insts.iter())
        {
            emits_nothing[raw(id) as usize] = true;
        }
    }
    let mut results_unassigned = vec![false; func.insts.len()];
    let mut return_replaced = vec![false; func.blocks.len()];
    for (b, forward) in tail_forwards.iter().enumerate() {
        let Some(tail) = forward else { continue };
        return_replaced[b] = true;
        if let Some(&call) = tail.suppressed.first() {
            results_unassigned[raw(call) as usize] = true;
        }
    }
    let mut fixed_arity_override = vec![None; func.insts.len()];
    for forward in call_arg_forwards.iter().flatten() {
        // `f(prefix.., g(..))`: only the prefix are fixed arguments of `f`, and
        // `g`'s own results never land in locals.
        fixed_arity_override[raw(forward.consumer) as usize] = Some(forward.prefix.len());
        let producer = match &forward.producer {
            TailForwardCall::Direct { call, .. } => Some(*call),
            TailForwardCall::Value { .. } => None,
        };
        if let Some(call) = producer {
            results_unassigned[raw(call) as usize] = true;
        }
    }
    let shape = frame_limits::EmittedShape {
        emits_nothing: &emits_nothing,
        results_unassigned: &results_unassigned,
        fixed_arity_override: &fixed_arity_override,
        return_replaced: &return_replaced,
    };
    let Some(overflow) = frame_limits::frame_overflow(func, rpo, &actives, &shape) else {
        return;
    };
    ctx.reject(lua_name.to_string(), frame_limit_error(ws, func, &overflow));
}

/// The spanned diagnostic for a [`frame_limits::FrameOverflow`]: which Rock
/// function, which LuaJIT limit, the emitter's count, and the wide values that
/// are live across the statement that reaches it.
fn frame_limit_error(
    ws: &Workspace,
    func: &MirFunc,
    overflow: &frame_limits::FrameOverflow,
) -> CompilerError {
    let limit = overflow.limit;
    let rock_name = func.name;
    let mut message = format!(
        "`{rock_name}` is too wide for LuaJIT: the emitted Lua function needs {} {}, \
         and LuaJIT refuses to load a function over {} ({} is {}, \"{}\")",
        overflow.count,
        limit.subject(),
        limit.max_loadable(),
        limit.constant(),
        match limit {
            frame_limits::FrameLimit::Slots => frame_limits::LJ_MAX_SLOTS,
            frame_limits::FrameLimit::Locals => frame_limits::LJ_MAX_LOCVAR,
        },
        limit.luajit_message(),
    );
    if overflow.at.is_some() && (overflow.cost.call_arity > 0 || overflow.cost.wire_values > 0) {
        message.push_str(&format!(
            "\n  {} locals are live at that statement, which needs {} more slots \
             ({} call arguments, {} values received or returned)",
            overflow.actives,
            overflow.cost.slots,
            overflow.cost.call_arity,
            overflow.cost.wire_values,
        ));
    }
    let mut contributors: Vec<String> = Vec::new();
    for &call in &overflow.wide_calls {
        for line in wide_call_contributors(ws, func, call) {
            if !contributors.contains(&line) {
                contributors.push(line);
            }
        }
    }
    contributors.truncate(3);
    let nobox = contributors.iter().any(|c| c.contains("nobox"));
    if !contributors.is_empty() {
        message.push_str("\n  widest values live across a call in it:");
    }
    for line in &contributors {
        message.push_str("\n    ");
        message.push_str(line);
    }
    if nobox {
        message.push_str(
            "\n  a `nobox value` cannot be boxed automatically -- keep fewer wide values \
             live across one call, or split this function",
        );
    } else {
        message.push_str("\n  keep fewer values live across one call, or split this function");
    }
    let span = overflow
        .at
        .into_iter()
        .chain(overflow.wide_calls.iter().copied())
        .find_map(|at| func.inst_spans.get(at).copied())
        .or_else(|| any_inst_span(func))
        .unwrap_or_else(Span::unknown);
    let mut err = CompilerError::new_with_label(
        span,
        format!("`{rock_name}` exceeds LuaJIT's {} limit", limit.constant()),
    );
    err.message = message;
    err
}

/// The widest values on the wire of the call at `id`, as message lines. Read
/// from the ABI plan the call lowering itself used, so the lane counts are the
/// ones the emitted argument list actually has.
pub(crate) fn wide_call_contributors(ws: &Workspace, func: &MirFunc, id: Idx<Inst>) -> Vec<String> {
    let Some(abi) = func.call_abi_plans.get(&id) else {
        return Vec::new();
    };
    let decl = abi.decl();
    let callee = func_of(ws, decl);
    let callee_name = callee.name;
    let mut lines: Vec<(usize, String)> = Vec::new();
    if let Some(widths) = abi.param_leaf_widths() {
        let reps = abi.representation().params();
        for (i, width) in widths.iter().copied().enumerate() {
            if width < 2 {
                continue;
            }
            let nobox = reps.get(i).is_some_and(|r| abi.rep_plan(*r).nobox());
            let param = callee
                .param_decls
                .get(i)
                .map(|p| p.name.to_string())
                .unwrap_or_else(|| format!("#{i}"));
            lines.push((
                width,
                format!(
                    "parameter `{param}` of `{callee_name}` is {width} scalar lanes{}",
                    if nobox {
                        " (nobox: cannot be boxed)"
                    } else {
                        ""
                    }
                ),
            ));
        }
    }
    let result_lanes = abi.result_lanes();
    if result_lanes > 1 {
        let nobox = abi
            .representation()
            .result()
            .is_some_and(|r| abi.rep_plan(r).nobox());
        lines.push((
            result_lanes,
            format!(
                "the return of `{callee_name}` is {result_lanes} scalar lanes{}",
                if nobox {
                    " (nobox: cannot be boxed)"
                } else {
                    ""
                }
            ),
        ));
    }
    lines.sort_by_key(|x| std::cmp::Reverse(x.0));
    lines.truncate(3);
    lines.into_iter().map(|(_, line)| line).collect()
}
