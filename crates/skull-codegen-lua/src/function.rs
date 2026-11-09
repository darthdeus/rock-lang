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

pub fn emit_function_with_allocation_sites(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    self_ref: FuncRef,
    func: &MirFunc,
    bound: Option<&CalleeBindingPlan>,
    instrument: bool,
) -> LuaEmission {
    let name = mangle_fn(ws, func_of(ws, self_ref));
    emit_named_with_allocation_sites(ws, ctx, interner, &name, func, bound, instrument)
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
    emit_named_with_statics(ws, ctx, interner, name, func, &[], bound)
}

pub(crate) fn emit_named_with_allocation_sites(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    bound: Option<&CalleeBindingPlan>,
    instrument: bool,
) -> LuaEmission {
    emit_named_framed_with_allocation_sites(
        ws,
        ctx,
        interner,
        name,
        func,
        None,
        &[],
        bound,
        instrument,
    )
}

/// Emit a function whose static cells may be owned by an enclosing lowered
/// function. Closure conversion moves the lambda body into a sibling function,
/// while its static cell remains owned by the enclosing callable.
pub fn emit_named_with_statics(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    inherited_statics: &[StaticCell],
    bound: Option<&CalleeBindingPlan>,
) -> String {
    emit_named_framed_with_statics(
        ws,
        ctx,
        interner,
        name,
        func,
        None,
        inherited_statics,
        bound,
    )
}

/// The coroutine state-table context: a `co fn`'s `__step` reads/writes its
/// frame cells as fields of the state table `s` (the first param cell). A frame
/// cell renders `<s>.c<slot>` instead of a bare local and is NOT declared. The
/// slot is a DENSE per-frame ordinal (`slots`), NOT the raw cell-arena id:
/// lambda-cell allocation bumps raw ids but never reorders the frame cells, so
/// a slot -- and the layout stamp keyed on it -- is invariant under
/// lambda-internal cell-count edits (the P2 finding).
/// Lambdas are converted before emission;
/// detached hook/defer regions are ordinary `MirFunc`s by the time they reach
/// this printer.
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
///
/// Closures no longer convert here: `skull_transform::convert_closures` (the
/// shared MIR pass) has already rewritten every reachable `Inst::Lambda` into a
/// `MakeEnv` record / bare `Global` + a hoisted sibling `MirFunc` before emission.
/// So this fn only renders the given
/// region.
pub fn emit_named_framed(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    bound: Option<&CalleeBindingPlan>,
) -> String {
    emit_named_framed_with_statics(ws, ctx, interner, name, func, frame, &[], bound)
}

fn emit_named_framed_with_statics(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    inherited_statics: &[StaticCell],
    bound: Option<&CalleeBindingPlan>,
) -> String {
    emit_named_framed_with_statics_impl(
        ws,
        ctx,
        interner,
        name,
        func,
        frame,
        inherited_statics,
        bound,
        None,
    )
    .0
}

pub fn emit_named_framed_with_allocation_sites(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    inherited_statics: &[StaticCell],
    bound: Option<&CalleeBindingPlan>,
    instrument: bool,
) -> LuaEmission {
    let sites = RefCell::new(Vec::new());
    // The header offset comes back from the emission rather than from a search
    // for `function <name>`: a bound body's only
    // copy sits inside a factory and that spelling does not appear at all.
    let (lua, header_offset) = emit_named_framed_with_statics_impl(
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

/// Returns the emitted definition and the byte offset of its header (the
/// `function` keyword that opens the body), which the allocation-site reporter
/// turns into a body-relative line number.
fn emit_named_framed_with_statics_impl(
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
    let func = literals.as_ref().unwrap_or(func);
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
        (vec![None; func.insts.len()], vec![false; func.insts.len()])
    } else {
        call_arg_forwards(ws, func)
    };
    let t_cells = bone_core::prelude::Instant::now();
    let cell_analysis = regalloc::CellAnalysis::new(func);
    REGALLOC_TIME.with(|c| c.set(c.get() + t_cells.elapsed()));
    let (mut inlinable, repeatable) = compute_inlinable(func, &rpo, params, &cell_analysis.pinned);
    for tail in tail_forwards.iter().flatten() {
        for &id in &tail.suppressed[1..] {
            inlinable[raw(id) as usize] = true;
        }
    }
    for (id, inst) in func.insts.iter() {
        let raw_id = raw(id) as usize;
        if suppressed[raw_id] && matches!(inst, Inst::Load(_)) {
            inlinable[raw_id] = true;
        }
    }
    alias_int_to_float(func, &rpo, &mut inlinable, &repeatable);
    skull_mir::sinking::preserve_effect_order(func, &rpo, &mut inlinable);
    bound_fold_levels(func, &rpo, &mut inlinable);
    // Frame fields map to their dense slots before this table is consulted.
    // Frame accesses name `s` implicitly in the printer rather than through a
    // MIR cell use; reserve its parameter's color so a later local can never
    // overwrite the state table itself.
    let cell_colors = if let Some(frame) = frame {
        color_cells_timed(&cell_analysis, &inlinable, &[frame.s_cell])
    } else {
        color_cells_timed(&cell_analysis, &inlinable, &[])
    };
    // Module-scope const hoisting: an all-constant `value` ctor is built ONCE as a
    // function-owned top-level global and referenced everywhere, instead of rebuilt
    // at each use. On a trace the per-use ctor sinks anyway, but in the interpreter
    // (uncompiled text-rendering loops) it allocates every time -- the nested
    // `outline_color: Vec2 { 0.0, 0.0 }` per glyph vertex was the entire skull-vs
    // -rock interpreter-path text gap. rock does the same (`__rock_const_*`). The
    // def is emitted in THIS function's chunk (not shared scaffolding), so an
    // incremental body-edit overlay carries it; value structs are immutable, so
    // re-running the def on reload is a harmless no-op (no preservation guard).
    record_representation_violations(ctx, func);
    let (hoist, hoist_names, hoist_defs) = collect_const_hoists(ws, ctx, func, name);
    let static_slots = static_slots_for(ws, interner, func, inherited_statics, name, ctx);
    // An unnamed definition is an expression its caller embeds mid-line, where
    // a directive (which must start a line) cannot go.
    let line_marks = (ctx.line_directives() && !name.is_empty()).then(|| RefCell::new(Vec::new()));
    let estimated_body_bytes = func.insts.len() * 24;
    let mut out = String::with_capacity(hoist_defs.len() + name.len() + 64 + estimated_body_bytes);
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
        inlinable,
        true,
        frame,
        &cell_colors,
        &cell_analysis.pinned,
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
