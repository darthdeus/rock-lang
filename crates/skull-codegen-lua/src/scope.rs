//! Rendering one scope of a body: declarations, blocks and kills.

use super::*;

/// Render `function(<params>) <locals> <blocks> end` for the ordinary MIR
/// function rooted at `entry`, appending to `out` after its name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_scope(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    // The emitted Lua name, for the rejection key of a frame-limit overflow.
    lua_name: &str,
    func: &MirFunc,
    entry: Idx<Block>,
    params: &[Idx<Cell>],
    rpo: &[Idx<Block>],
    inlinable: Vec<bool>,
    is_top: bool,
    frame: Option<&FrameCtx>,
    cell_colors: &[u32],
    pinned: &[bool],
    state_slots: &HashMap<ustr::Ustr, u32>,
    static_slots: &HashMap<ustr::Ustr, String>,
    hoist: &HashMap<Idx<Inst>, u32>,
    hoist_names: &[String],
    allocation: Option<AllocationEmission<'_>>,
    tail_forwards: &[Option<TailForward>],
    call_arg_forwards: &[Option<CallArgForward>],
    suppressed: &[bool],
    line_marks: Option<&RefCell<Vec<LineMark>>>,
    bound: Option<&CalleeBindingPlan>,
    out: &mut String,
) {
    // RPO drives both block layout (preheader-before-header, for the relooper)
    // and the reachable set the regalloc colors over.
    let loops = recognize_counted_loops(func, rpo, frame);
    let t_ra = bone_core::prelude::Instant::now();
    // Counted-loop regions partition the coloring so loop-body temps get
    // body-scoped `local`s (see `ValueAlloc::color_scope`). Partitioning trades
    // some color sharing for scoping; if a monster function's palette total
    // nears LuaJIT's 200-active-locals parse cap, drop the partitions.
    let mut loop_scopes: Vec<regalloc::LoopScope> = loops
        .iter()
        .map(|(&h, cl)| regalloc::LoopScope {
            header: h,
            region: cl.region.iter().copied().collect(),
        })
        .collect();
    loop_scopes.sort_by_key(|ls| func.emit_block_id(ls.header));
    let mut va = regalloc::color_values(func, rpo, &inlinable, &loop_scopes);
    if va.color_scope.len() > 150 && !loop_scopes.is_empty() {
        va = regalloc::color_values(func, rpo, &inlinable, &[]);
    }
    let regalloc::ValueAlloc {
        colors,
        color_scope,
    } = va;
    // Only cells this region actually references get a local (a hoisted lambda
    // body is a full-arena clone; declaring the dead enclosing/sibling cells would
    // blow LuaJIT's 200-local cap on a large co-fn step). Computed once per top
    // scope; inline (non-`is_top`) renders declare nothing, so they skip it.
    let referenced = if is_top {
        referenced_cells_in_region(func, entry, tail_forwards, suppressed)
    } else {
        HashSet::default()
    };
    let param_cells: HashSet<Idx<Cell>> = params.iter().copied().collect();
    let is_frame = |c: &Idx<Cell>| frame.is_some_and(|f| f.fields.contains(c));
    // THE one answer to "does this cell render as a bare `c<..>` local here" --
    // read by the nil-kill exclusion just below and by the declaration loop that
    // emits those locals. A kill of a cell with no local assigns a GLOBAL of that
    // name instead: a store the pass never intended, which under the `_G` state
    // bridge costs a `__newindex` metamethod dispatch on every execution.
    let cell_has_local = |cid: Idx<Cell>| -> bool {
        param_cells.contains(&cid) || (is_top && !is_frame(&cid) && referenced.contains(&cid))
    };
    // Where each declared local dies, for the `<local> = nil` kills that keep
    // dead slots out of LuaJIT exit snapshots (see `regalloc::nil_kills`). A
    // coroutine `__step` keeps raw cell names and frame fields; it gets none.
    let kills = if frame.is_some() {
        regalloc::NilKills::none(func)
    } else {
        let mut emits_nothing = inlinable.clone();
        for id in hoist.keys() {
            emits_nothing[raw(*id) as usize] = true;
        }
        // A counted loop's header (compare) and latch (increment) are subsumed
        // by the `for` line; their instructions never run as statements.
        for (h, cl) in loops.iter() {
            for &id in func.blocks[*h]
                .insts
                .iter()
                .chain(func.blocks[cl.latch].insts.iter())
            {
                emits_nothing[raw(id) as usize] = true;
            }
        }
        // A loop-scoped register may be killed anywhere inside its `for` block:
        // the body region and the latch label, not the header (rendered as the
        // `for` line itself, outside the block).
        let kill_scopes: Vec<regalloc::LoopScope> = loops
            .iter()
            .map(|(&h, cl)| regalloc::LoopScope {
                header: h,
                region: cl
                    .region
                    .iter()
                    .copied()
                    .chain(std::iter::once(cl.latch))
                    .collect(),
            })
            .collect();
        // A cell with no local of its own is excluded exactly like a pinned one:
        // both mean "this cell is not a killable `c<color>` in this body".
        let mut excluded = pinned.to_vec();
        for (cid, _) in func.cells.iter() {
            if !cell_has_local(cid) {
                excluded[raw(cid) as usize] = true;
            }
        }
        regalloc::nil_kills(
            func,
            rpo,
            &emits_nothing,
            &colors,
            &color_scope,
            &kill_scopes,
            cell_colors,
            &excluded,
            params,
        )
    };
    REGALLOC_TIME.with(|c| c.set(c.get() + t_ra.elapsed()));

    // Param list `(a, b, c)` — written straight into the buffer (no Vec<String>).
    // A hoisted capturing lambda's env-record param is a materialized cell in
    // `params` (the pass), so it renders through the normal loop.
    out.push('(');
    let mut wrote_param = false;
    for &c in params.iter() {
        if wrote_param {
            out.push_str(", ");
        }
        wrote_param = true;
        write_cell_local(out, func, cell_colors, c);
    }
    out.push_str(")\n");

    // A body that registers a `defer` cleanup re-enters itself under the
    // unwind trampoline, which runs the cleanups still registered when an
    // error escapes and re-raises it. The flag is consumed by that re-entry,
    // so a recursive or nested call is protected again.
    if is_top && func.registers_defer_cleanup(rpo) {
        assert!(
            frame.is_none(),
            "coroutine step `{lua_name}` registers an unwind defer"
        );
        out.push_str("  if not __rock_defer_entering then return ");
        out.push_str(skull_mir::mir::DEFER_CALL);
        out.push('(');
        out.push_str(lua_name);
        for &c in params.iter() {
            out.push_str(", ");
            write_cell_local(out, func, cell_colors, c);
        }
        out.push_str(") end\n  __rock_defer_entering = false\n");
    }

    for &c in params {
        if cell_is_uint(ws, func.cells[c].ty) {
            out.push_str("  ");
            write_cell_local(out, func, cell_colors, c);
            out.push_str(" = ((");
            write_cell_local(out, func, cell_colors, c);
            out.push_str(" % 4294967296) + 4294967296) % 4294967296\n");
        }
    }

    // Declare registers in sorted color order (the dense table holds `u32::MAX`
    // for uncolored slots — inlined / non-value insts — which we skip), then the
    // cell locals. All written directly into a single `  local ...` line; we
    // stamp the keyword up front and `truncate` it back if nothing is declared.
    // Loop-owned colors are declared inside their `for` block (expand_for), not
    // here.
    let mut distinct: Vec<u32> = colors
        .iter()
        .copied()
        .filter(|&c| c != u32::MAX && color_scope[c as usize].is_none())
        .collect();
    distinct.sort_unstable();
    distinct.dedup();
    let decl_start = out.len();
    out.push_str("  local ");
    let mut any = false;
    // Counted as the names are written, so the frame-limit model reads THIS
    // declaration rather than a second derivation of it.
    let mut top_locals = 0usize;
    for &c in &distinct {
        if any {
            out.push_str(", ");
        }
        any = true;
        top_locals += 1;
        out.push('r');
        push_uint(out, c);
    }
    if is_top {
        // Declare each cell LOCAL exactly once. Colored cells collapse onto their
        // shared `c<color>` (deduped, and excluding any color already named by a
        // param); any remaining uncolored cells declare per raw id.
        let param_colors: HashSet<u32> = params
            .iter()
            .map(|c| cell_colors[raw(*c) as usize])
            .filter(|&col| col != u32::MAX)
            .collect();
        let mut seen_color: HashSet<u32> = HashSet::default();
        for (cid, _) in func.cells.iter() {
            if param_cells.contains(&cid) || !cell_has_local(cid) {
                continue;
            }
            let (decl, col) = match cell_colors[raw(cid) as usize] {
                u32::MAX => (true, None), // uncolored -> c<raw>
                col if !param_colors.contains(&col) => (seen_color.insert(col), Some(col)),
                _ => (false, None), // color already a param's
            };
            if !decl {
                continue;
            }
            if any {
                out.push_str(", ");
            }
            any = true;
            top_locals += 1;
            out.push('c');
            match col {
                Some(col) => push_uint(out, col),
                None => push_uint(out, func.emit_cell_id(cid)),
            }
        }
    }
    if any {
        out.push('\n');
    } else {
        out.truncate(decl_start); // no locals -> drop the stamped `  local `
    }

    // The declarations above are final, so this is the earliest point at which
    // the emitted frame is fully known. A body over one of LuaJIT's parse-time
    // caps is rejected here with a Rock span instead of failing at `loadfile`
    // with a Lua chunk position (`frame_limits`).
    if ctx.frame_limits() {
        check_frame_limits(
            ws,
            ctx,
            lua_name,
            func,
            rpo,
            params.len() + top_locals,
            &loops,
            &color_scope,
            hoist,
            tail_forwards,
            call_arg_forwards,
        );
    }

    // (Box `{ v = .. }` init lines are gone -- `convert_closures` materializes a
    // shared mutable capture as an explicit `MakeEnv { fn_ref: None }` value in the
    // MIR, so codegen never sees a "boxed cell".)

    let mut em = Emitter {
        enum_box_groups: RefCell::new(HashMap::default()),
        selection_preds: std::cell::OnceCell::new(),
        ws,
        ctx,
        interner,
        func,
        colors,
        color_scope,
        cell_colors,
        frame,
        loops: &loops,
        rpo,
        inlinable,
        emitted: vec![false; func.blocks.len()],
        hoist,
        hoist_names,
        state_slots,
        static_slots,
        allocation,
        tail_forwards,
        call_arg_forwards,
        suppressed,
        kills,
        line_marks,
        bound,
    };
    let body_start = out.len();
    for &bb in rpo {
        em.emit_block(out, bb); // self-skips an already-emitted block (loop body)
    }
    if ctx.frame_limits() {
        let declared = params.len() + top_locals + loops.len() * frame_limits::FOR_CONTROL_LOCALS;
        check_body_limits(ctx, lua_name, func, &out[body_start..], declared);
    }
    out.push_str("end");
}
