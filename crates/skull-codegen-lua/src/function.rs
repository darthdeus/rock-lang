//! Function entry points: plain, named, framed and allocation-tracking emits.

use super::*;

/// Emit one MIR function as a top-level Lua `function <name>(...) ... end`.
/// `self_ref` identifies the function being emitted so its header name mangles
/// identically to a call site that targets it.
pub fn emit_function(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    self_ref: FuncRef,
    func: &MirFunc,
    bound: Option<&CalleeBindingPlan>,
) -> String {
    let name = mangle_fn(ws, func_of(ws, self_ref));
    emit_named(ws, ctx, interner, &name, func, bound)
}

/// Emit a MIR function under the EXACT Lua name `name` (no mangling) — for
/// synthetic functions with an already-final name (a global-initializer fn).
/// Calls inside the body still mangle their `FuncRef`s via `ctx`.
pub fn emit_named(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    bound: Option<&CalleeBindingPlan>,
) -> String {
    emit_lua_definition(ws, ctx, interner, name, func, None, &[], bound, None).lua
}

/// Map coroutine cells onto a state table. Dense slots stay stable when
/// editing a detached lambda changes the cell arena.
pub struct FrameCtx {
    pub s_cell: Idx<Cell>,
    pub fields: HashSet<Idx<Cell>>,
    /// Frame cell -> dense slot (`c<slot>` field name). Total over `fields`.
    pub slots: HashMap<Idx<Cell>, u32>,
}

#[derive(Clone, Copy)]
pub(crate) struct AllocationEmission<'a> {
    pub(crate) body: &'a str,
    pub(crate) sites: &'a RefCell<Vec<EmittedAllocationSite>>,
    pub(crate) instrument: bool,
}

/// [`emit_named`] with an optional coroutine frame context (the `__step` fn).
pub fn emit_named_framed(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    bound: Option<&CalleeBindingPlan>,
) -> String {
    emit_lua_definition(ws, ctx, interner, name, func, frame, &[], bound, None).lua
}

/// The one Lua emission of a MIR function under the exact name `name`.
///
/// `frame` is the coroutine state-table context of a `__step` fn.
/// `inherited_statics` are static cells owned by an enclosing function:
/// closure conversion moves a lambda body into a sibling function while its
/// static cell stays with the enclosing callable. `allocation_sites`, when
/// set, collects the body's allocation sites, and `Some(true)` also
/// instruments them in the emitted Lua.
#[allow(clippy::too_many_arguments)]
pub fn emit_lua_definition(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    inherited_statics: &[StaticCell],
    bound: Option<&CalleeBindingPlan>,
    allocation_sites: Option<bool>,
) -> LuaEmission {
    let Some(instrument) = allocation_sites else {
        let (lua, _) = emit_body(
            ws,
            ctx,
            interner,
            name,
            func,
            frame,
            inherited_statics,
            bound,
            None,
        );
        return LuaEmission {
            lua,
            allocation_sites: Vec::new(),
        };
    };
    let sites = RefCell::new(Vec::new());
    // The header offset comes back from the emission rather than from a search
    // for `function <name>`: a bound body's only
    // copy sits inside a factory and that spelling does not appear at all.
    let (lua, header_offset) = emit_body(
        ws,
        ctx,
        interner,
        name,
        func,
        frame,
        inherited_statics,
        bound,
        Some(AllocationEmission {
            body: name,
            sites: &sites,
            instrument,
        }),
    );
    let mut allocation_sites = sites.into_inner();
    let function_line_offset = lua.as_bytes()[..header_offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count() as u32;
    allocation_sites.sort_by_key(|site| site.lua_byte_offset);
    let mut byte_cursor = 0;
    let mut line = 1;
    for site in &mut allocation_sites {
        for byte in &lua.as_bytes()[byte_cursor..site.lua_byte_offset] {
            if *byte == b'\n' {
                line += 1;
            }
        }
        site.lua_body_line = line - function_line_offset;
        byte_cursor = site.lua_byte_offset;
    }
    allocation_sites.sort_by_key(|site| site.id);
    allocation_sites.dedup_by_key(|site| site.id);
    LuaEmission {
        lua,
        allocation_sites,
    }
}

/// Runs `emit` over `func` with every read of a scalar constant replaced by
/// its literal, and puts the reads back afterwards. The emit entry points
/// rewrite a copy of a body that still reads a constant; a caller that owns
/// its body lends it here and spares the copy.
pub fn with_constants_baked<R>(
    ctx: &CodegenCtx,
    func: &mut MirFunc,
    emit: impl FnOnce(&MirFunc) -> R,
) -> R {
    struct Baked<'a> {
        func: &'a mut MirFunc,
        reads: Vec<(Idx<Inst>, ustr::Ustr)>,
    }
    // Restored on unwind too: the body outlives a failed emit in a host that
    // keeps compiling.
    impl Drop for Baked<'_> {
        fn drop(&mut self) {
            for &(id, name) in &self.reads {
                self.func.insts[id] = Inst::Global(name);
            }
        }
    }
    let mut reads = Vec::new();
    for (id, inst) in func.insts.iter_mut() {
        if let Inst::Global(name) = inst
            && let Some(lit) = ctx.scalar_constants.get(name)
        {
            reads.push((id, *name));
            *inst = Inst::Const(lit.clone());
        }
    }
    let baked = Baked { func, reads };
    emit(baked.func)
}

/// Returns the emitted definition and the byte offset of its header (the
/// `function` keyword that opens the body), which the allocation-site reporter
/// turns into a body-relative line number.
fn emit_body(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    inherited_statics: &[StaticCell],
    bound: Option<&CalleeBindingPlan>,
    allocation: Option<AllocationEmission<'_>>,
) -> (String, usize) {
    let mut literals = None;
    for (id, inst) in func.insts.iter() {
        if let Inst::Global(name) = inst {
            if let Some(lit) = ctx.scalar_constants.get(name) {
                let rewritten = literals.get_or_insert_with(|| func.clone());
                rewritten.insts[id] = Inst::Const(lit.clone());
            }
        }
    }
    let source = literals.as_ref().unwrap_or(func);
    let scheduled = if frame.is_none() && !ctx.line_directives() {
        late_float::defer_wide_float_math(source)
    } else {
        None
    };
    let func = scheduled.as_ref().map(|(func, _)| func).unwrap_or(source);
    let entry = func.entry();
    let params = func.params.as_slice();
    let rpo = reverse_postorder(func, entry);
    if let Some(rows) = &ctx.trace_frames {
        let mut contributors = Vec::new();
        if let Some(abi) = &func.abi_plan {
            let decl = func_of(ws, abi.decl());
            let subst = abi.subject.subst(ws, interner);
            let type_name = |ty| {
                skull_mir::monoty::convert_decl(ws, ty, &subst)
                    .map(|ty| ty.render())
                    .unwrap_or_else(|_| ws.render(None, ty))
            };
            if let Some(widths) = abi.param_leaf_widths() {
                for (i, width) in widths
                    .into_iter()
                    .enumerate()
                    .filter(|(_, width)| *width > 1)
                {
                    let rep = abi.representation().params()[i];
                    let nobox = abi.rep_plan(rep).nobox();
                    let param = &decl.param_decls[i];
                    contributors.push(format!(
                        "input `{}` of `{}` has type `{}` and is {width} scalar lanes{}",
                        param.name,
                        func.name,
                        type_name(param.ty),
                        if nobox {
                            " (nobox: cannot be boxed)"
                        } else {
                            ""
                        }
                    ));
                }
            }
            if abi.result_lanes() > 1 {
                let nobox = abi
                    .representation()
                    .result()
                    .is_some_and(|rep| abi.rep_plan(rep).nobox());
                let ty = decl
                    .return_ty
                    .map(type_name)
                    .unwrap_or_else(|| "unit".into());
                contributors.push(format!(
                    "return of `{}` has type `{ty}` and is {} scalar lanes{}",
                    func.name,
                    abi.result_lanes(),
                    if nobox {
                        " (nobox: cannot be boxed)"
                    } else {
                        ""
                    }
                ));
            }
        }
        for &block in &rpo {
            for &id in &func.blocks[block].insts {
                for line in wide_call_contributors(ws, func, id) {
                    if !contributors.contains(&line) {
                        contributors.push(line);
                    }
                }
            }
        }
        rows.lock()
            .expect("trace frame sink")
            .insert(name.to_string(), contributors);
    }
    let tail_forwards = tail_forwards(ws, func, frame.is_none());
    let (call_arg_forwards, suppressed) = if frame.is_some() {
        // Coroutine lifting moves later uses into frame fields, outside the
        // ordinary cell reference count that proves a producer unobserved.
        (Default::default(), vec![false; func.insts.len()])
    } else {
        call_arg_forwards(ws, func)
    };
    let t_cells = bone_core::prelude::Instant::now();
    let flow = regalloc::Flow::new(func, &rpo);
    let pinned = regalloc::pinned_cells(func, &flow);
    REGALLOC_TIME.with(|c| c.set(c.get() + t_cells.elapsed()));
    let (mut inlinable, repeatable, uses) = compute_inlinable(func, &rpo, params, &pinned);
    for tail in tail_forwards.iter().flatten() {
        for &id in &tail[1..] {
            inlinable[raw(id) as usize] = true;
        }
    }
    for (id, inst) in func.insts.iter() {
        let raw_id = raw(id) as usize;
        if suppressed[raw_id] && matches!(inst, Inst::Load(_)) {
            inlinable[raw_id] = true;
        }
    }
    // Coroutine frame fields are shared with the step's detached hooks, so a
    // step keeps its reads at their program position.
    let folded_reads = if frame.is_none() {
        fold_single_use_reads(
            ws,
            func,
            &rpo,
            &mut inlinable,
            &pinned,
            &suppressed,
            &call_arg_forwards,
            ctx.line_directives() && !name.is_empty(),
            &uses,
        )
    } else {
        vec![false; func.insts.len()]
    };
    if let Some((_, pinned_calls)) = &scheduled {
        for &call in pinned_calls {
            inlinable[raw(call) as usize] = false;
        }
    }
    alias_int_to_float(func, &rpo, &mut inlinable, &repeatable);
    skull_mir::sinking::preserve_effect_order_with(
        func,
        &rpo,
        &mut inlinable,
        same_base_projections(func),
    );
    keep_reads_ordered(func, &rpo, &mut inlinable, &folded_reads, &uses);
    bound_fold_levels(func, &rpo, &mut inlinable, &folded_reads);
    let loops = recognize_for_loops(func, &rpo, frame);
    let reads = regalloc::Reads::new(func, &rpo, &inlinable);
    if frame.is_none() {
        for id in regalloc::stable_loads(func, &rpo, &flow, &inlinable, &pinned, &loops, &reads) {
            inlinable[raw(id) as usize] = true;
        }
    }
    record_representation_violations(ctx, func);
    let (hoist, hoist_names, hoist_defs) = collect_const_hoists(ws, ctx, func, name);
    // A value moves into the cell it is copied to when it renders through the
    // plain `<local> = <value>` statement, outside the lines a counted `for`
    // subsumes.
    let alias = if frame.is_none() {
        let mut movable = vec![true; func.insts.len()];
        for tail in tail_forwards.iter().flatten() {
            for &id in *tail {
                movable[raw(id) as usize] = false;
            }
        }
        for id in for_control_insts(func, &loops) {
            movable[raw(id) as usize] = false;
        }
        regalloc::store_targets(func, &rpo, &flow, &inlinable, &pinned, &reads, |v, cell| {
            movable[raw(v) as usize]
                && !hoist.contains_key(&v)
                && call_arg_forwards.get(v).is_none()
                && !suppressed[raw(v) as usize]
                && !loops.values().any(
                    |cl| matches!(cl.control, ForControl::Numeric { slot, .. } if slot == cell),
                )
        })
    } else {
        Vec::new()
    };
    // Frame fields map to their dense slots before this table is consulted.
    // Frame accesses name `s` implicitly in the printer rather than through a
    // MIR cell use; reserve its parameter's color so a later local can never
    // overwrite the state table itself.
    let t_locals = bone_core::prelude::Instant::now();
    let frame_cell = frame.map(|frame| frame.s_cell);
    let locals = regalloc::Locals::new(
        func,
        &rpo,
        &flow,
        &inlinable,
        &alias,
        &pinned,
        frame_cell.as_slice(),
        &reads,
        uses.count,
    );
    let cell_colors = locals.color_cells();
    REGALLOC_TIME.with(|c| c.set(c.get() + t_locals.elapsed()));
    let static_slots = static_slots_for(ws, interner, func, inherited_statics, name, ctx);
    // An unnamed definition is an expression its caller embeds mid-line, where
    // a directive (which must start a line) cannot go.
    let line_marks = (ctx.line_directives() && !name.is_empty()).then(|| RefCell::new(Vec::new()));
    let estimated_body_bytes = func.insts.len() * 24;
    let mut out = String::with_capacity(hoist_defs.len() + name.len() + 64 + estimated_body_bytes);
    // Keep hoisted constants in the owning chunk so live reload carries them.
    out.push_str(&hoist_defs);
    // The factory captures final callee globals under private parameter names.
    // The same plan names its parameters and every bound use in the body.
    let header_offset = match bound {
        Some(plan) => {
            debug_assert_eq!(plan.name, name);
            let _ = write!(out, "{} = function(", plan.factory);
            for (i, capture) in plan.parameters.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(capture.as_str());
            }
            out.push_str(") return ");
            let offset = out.len();
            out.push_str("function");
            offset
        }
        None => {
            let offset = out.len();
            let _ = write!(out, "function {}", name);
            offset
        }
    };
    render_scope(
        ws,
        ctx,
        interner,
        name,
        func,
        entry,
        params,
        &rpo,
        &locals,
        &inlinable,
        &alias,
        loops,
        frame,
        &cell_colors,
        &pinned,
        &ctx.state_slots,
        &static_slots,
        &hoist,
        &hoist_names,
        allocation,
        &tail_forwards,
        &call_arg_forwards,
        &suppressed,
        line_marks.as_ref(),
        bound,
        &reads,
        &mut out,
    );
    if bound.is_some() {
        out.push_str(" end");
    }
    out.push('\n');
    let Some(marks) = line_marks else {
        return (out, header_offset);
    };
    let Some((lua, inserted)) = write_line_directives(&out, header_offset, &marks.into_inner())
    else {
        return (out, header_offset);
    };
    // A directive goes in front of a line, so an offset at that line's start
    // moves past it.
    let shift = |offset: usize| {
        offset
            + inserted
                .iter()
                .take_while(|(at, _)| *at <= offset)
                .map(|(_, len)| len)
                .sum::<usize>()
    };
    if let Some(allocation) = allocation {
        for site in allocation.sites.borrow_mut().iter_mut() {
            site.lua_byte_offset = shift(site.lua_byte_offset);
        }
    }
    (lua, shift(header_offset))
}
