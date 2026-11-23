use super::*;

#[allow(clippy::too_many_arguments)]
pub(crate) fn check_frame_limits(
    ws: &Workspace,
    ctx: &CodegenCtx,
    lua_name: &str,
    func: &MirFunc,
    rpo: &[Idx<Block>],
    top_locals: usize,
    loops: &HashMap<Idx<Block>, ForLoop>,
    hoist: &HashMap<Idx<Inst>, u32>,
    tail_forwards: &[Option<&[Idx<Inst>]>],
    call_arg_forwards: &CallArgForwards,
    inlinable: &[bool],
) {
    if frame_limits::fits_without_measuring(
        func,
        rpo,
        top_locals
            + loops
                .values()
                .filter(|l| matches!(l.control, ForControl::Map { .. }))
                .count(),
        loops.len(),
        inlinable,
    ) {
        return;
    }

    // Count control locals from each enclosing loop.
    let mut actives = vec![top_locals; func.blocks.len()];
    for cl in loops.values() {
        let add = cl.control_locals();
        for &b in cl.region.iter().chain(std::iter::once(&cl.latch)) {
            actives[raw(b) as usize] += add;
        }
    }

    let mut emits_nothing = vec![false; func.insts.len()];
    for id in hoist.keys().copied().chain(for_control_insts(func, loops)) {
        emits_nothing[raw(id) as usize] = true;
    }
    let mut results_unassigned = vec![false; func.insts.len()];
    let mut return_replaced = vec![false; func.blocks.len()];
    for (b, forward) in tail_forwards.iter().enumerate() {
        let Some(tail) = forward else { continue };
        return_replaced[b] = true;
        results_unassigned[raw(tail[0]) as usize] = true;
    }
    let mut fixed_arity_override = vec![None; func.insts.len()];
    for forward in call_arg_forwards.iter() {
        // `f(prefix.., g(..))`: only the prefix are fixed arguments of `f`, and
        // `g`'s own results never land in locals.
        fixed_arity_override[raw(forward.consumer) as usize] = Some(forward.prefix_len);
        if matches!(
            func.insts[forward.producer],
            Inst::Call { .. } | Inst::CallMulti { .. }
        ) {
            results_unassigned[raw(forward.producer) as usize] = true;
        }
    }
    let shape = frame_limits::EmittedShape {
        emits_nothing: &emits_nothing,
        results_unassigned: &results_unassigned,
        fixed_arity_override: &fixed_arity_override,
        return_replaced: &return_replaced,
        inlinable: Some(inlinable),
    };
    let Some(overflow) = frame_limits::frame_overflow(func, rpo, &actives, &shape) else {
        return;
    };
    ctx.reject(lua_name.to_string(), frame_limit_error(ws, func, &overflow));
}

/// Reject a body too long for LuaJIT to load, now that its text is final.
pub(crate) fn check_body_limits(
    ctx: &CodegenCtx,
    lua_name: &str,
    func: &MirFunc,
    body: &str,
    declared: usize,
    control: &ControlLines,
    base: usize,
) {
    let Some(overflow) = frame_limits::measure_body(body, declared, control, base) else {
        return;
    };
    let rock_name = func.name;
    let detail = match overflow {
        frame_limits::BodyOverflow::Names(count) => format!(
            "the emitted Lua function uses {count} labels, jumps and locals, and LuaJIT refuses \
             to load a function over {} (LJ_MAX_VSTACK)",
            frame_limits::LJ_MAX_VSTACK
        ),
        frame_limits::BodyOverflow::Jump(span) => format!(
            "a jump in the emitted Lua function spans at least {span} instructions, and LuaJIT \
             refuses to load a jump over {} (\"control structure too long\")",
            frame_limits::MAX_FORWARD_JUMP
        ),
    };
    let mut err = CompilerError::new_with_label(
        any_inst_span(func).unwrap_or_else(Span::unknown),
        format!("`{rock_name}` is too large for LuaJIT: {detail}. Split this function"),
    );
    err.kind = bone_core::prelude::CompilerErrorKind::TargetLimit;
    ctx.reject(lua_name.to_string(), err);
}

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
    let mut contributors: Vec<_> = func
        .abi_plan
        .iter()
        .chain(
            overflow
                .wide_calls
                .iter()
                .filter_map(|id| func.call_abi_plans.get(id)),
        )
        .flat_map(|abi| signature_contributors(ws, abi))
        .collect();
    contributors.sort_by_key(|(nobox, width, _)| std::cmp::Reverse((*nobox, *width)));
    let nobox = contributors.first().is_some_and(|(nobox, _, _)| *nobox);
    if !contributors.is_empty() {
        message.push_str("\n  widest values on the relevant Lua call signatures:");
    }
    let mut seen = HashSet::default();
    for (_, _, line) in contributors
        .iter()
        .filter(|(_, _, line)| seen.insert(line.as_str()))
        .take(3)
    {
        message.push_str("\n    ");
        message.push_str(line);
    }
    if nobox {
        message.push_str(
            "\n  remove `nobox` from a wide type, pass an explicit `materialize(value)` box \
             through a boxed parameter, or split this function",
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

fn signature_contributors(
    ws: &Workspace,
    abi: &skull_mir::lower::BackendFnAbiPlan,
) -> Vec<(bool, usize, String)> {
    use skull_mir::lower::RepLayout;

    let callee = func_of(ws, abi.decl());
    let mut lines = Vec::new();
    let mut append = |rep, width, subject: String| {
        if width < 2 {
            return;
        }
        let plan = abi.rep_plan(rep);
        let suffix = match plan.layout() {
            RepLayout::Leaves(shape) if plan.nobox() => format!(
                " (nobox `{}`: cannot be boxed)",
                bone_core::modules::display_name(shape.name().as_str())
            ),
            _ => String::new(),
        };
        lines.push((
            plan.nobox(),
            width,
            format!(
                "{subject} of `{}` is {width} scalar lanes{suffix}",
                callee.name
            ),
        ));
    };

    if let Some(widths) = abi.param_leaf_widths() {
        for (i, (&rep, width)) in abi.representation().params().iter().zip(widths).enumerate() {
            let param = callee
                .param_decls
                .get(i)
                .map(|p| p.name.to_string())
                .unwrap_or_else(|| format!("#{i}"));
            append(rep, width, format!("parameter `{param}`"));
        }
    }
    if let Some(rep) = abi.representation().result() {
        append(rep, abi.result_lanes(), "the return".to_string());
    }
    lines
}
