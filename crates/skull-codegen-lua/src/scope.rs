use super::*;

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_scope(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    lua_name: &str,
    func: &MirFunc,
    entry: Idx<Block>,
    params: &[Idx<Cell>],
    rpo: &[Idx<Block>],
    locals: &regalloc::Locals<'_>,
    inlinable: &[bool],
    alias: &[Option<Idx<Cell>>],
    loops: HashMap<Idx<Block>, ForLoop>,
    frame: Option<&FrameCtx>,
    cell_colors: &[u32],
    pinned: &[bool],
    state_slots: &HashMap<ustr::Ustr, u32>,
    static_slots: &HashMap<ustr::Ustr, String>,
    hoist: &HashMap<Idx<Inst>, u32>,
    hoist_names: &[String],
    allocation: Option<AllocationEmission<'_>>,
    tail_forwards: &[Option<&[Idx<Inst>]>],
    call_arg_forwards: &CallArgForwards,
    suppressed: &[bool],
    line_marks: Option<&RefCell<Vec<LineMark>>>,
    bound: Option<&CalleeBindingPlan>,
    reads: &regalloc::Reads,
    out: &mut String,
) {
    let t_ra = bone_core::prelude::Instant::now();
    let (colors, value_count) = locals.color_values();
    // A hoisted lambda body is a full-arena clone; declaring its dead cells
    // would blow LuaJIT's 200-local cap.
    let referenced = referenced_cells_in_region(func, rpo, tail_forwards, suppressed);
    let mut param_cells = vec![false; func.cells.len()];
    for &c in params {
        param_cells[raw(c) as usize] = true;
    }
    let is_frame = |c: &Idx<Cell>| frame.is_some_and(|f| f.fields.contains(c));
    // Killing an undeclared cell would assign to a global.
    let cell_has_local = |cid: Idx<Cell>| -> bool {
        param_cells[raw(cid) as usize] || (!is_frame(&cid) && referenced[raw(cid) as usize])
    };
    // `<local> = nil` kills keep dead slots out of LuaJIT exit snapshots. A
    // coroutine `__step` keeps raw cell names and frame fields; it gets none.
    let kills = if frame.is_some() {
        regalloc::NilKills::default()
    } else {
        let emits_nothing = (!hoist.is_empty() || !loops.is_empty()).then(|| {
            let mut quiet = inlinable.to_vec();
            for id in hoist.keys() {
                quiet[raw(*id) as usize] = true;
            }
            for id in for_control_insts(func, &loops) {
                // MapNext still supplies the generic-for head's table
                // read and cursor definitions to kill liveness.
                quiet[raw(id) as usize] = !matches!(func.insts[id], Inst::MapNext { .. });
            }
            quiet
        });
        let mut excluded = pinned.to_vec();
        for (cid, _) in func.cells.iter() {
            if !cell_has_local(cid) {
                excluded[raw(cid) as usize] = true;
            }
        }
        let mut kills = regalloc::nil_kills(
            locals,
            emits_nothing.as_deref(),
            &colors,
            value_count,
            cell_colors,
            &excluded,
            params,
            alias,
            reads,
        );
        // A path that can only end in a raise unwinds the frame; no slot it
        // leaves stale is ever observed.
        for b in raising_blocks(ws, func) {
            kills.drop_block(func, b);
        }
        let emits_nothing = emits_nothing.as_deref().unwrap_or(inlinable);
        kills.mark_return_tails(func, |id| {
            emits_nothing[raw(id) as usize] || suppressed[raw(id) as usize]
        });
        kills
    };
    REGALLOC_TIME.with(|c| c.set(c.get() + t_ra.elapsed()));

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

    // The unwind trampoline's re-entry consumes the flag, so a recursive or
    // nested call is protected again.
    if func.registers_defer_cleanup(rpo) {
        assert!(
            frame.is_none(),
            "coroutine step `{lua_name}` registers an unwind defer"
        );
        out.push_str(" if not __rock_defer_entering then return ");
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
            out.push(' ');
            write_cell_local(out, func, cell_colors, c);
            out.push_str(" = ((");
            write_cell_local(out, func, cell_colors, c);
            out.push_str(" % 4294967296) + 4294967296) % 4294967296\n");
        }
    }

    let decl_start = out.len();
    out.push_str(" local ");
    let mut any = false;
    let mut top_locals = 0usize;
    for color in 0..value_count {
        if any {
            out.push_str(", ");
        }
        any = true;
        top_locals += 1;
        out.push('r');
        push_uint(out, color);
    }
    let mut declared_colors: HashSet<u32> = params
        .iter()
        .map(|c| cell_colors[raw(*c) as usize])
        .filter(|&col| col != u32::MAX)
        .collect();
    for (cid, _) in func.cells.iter() {
        if param_cells[raw(cid) as usize] || !cell_has_local(cid) {
            continue;
        }
        let (decl, col) = match cell_colors[raw(cid) as usize] {
            u32::MAX => (true, None), // uncolored -> c<raw>
            col => (declared_colors.insert(col), Some(col)),
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
    if any {
        out.push('\n');
    } else {
        out.truncate(decl_start); // no locals -> drop the stamped `  local `
    }

    // Only now is the emitted frame final, so LuaJIT's parse-time caps are
    // checked here.
    if ctx.frame_limits() {
        check_frame_limits(
            ws,
            ctx,
            lua_name,
            func,
            rpo,
            params.len() + top_locals,
            &loops,
            hoist,
            tail_forwards,
            call_arg_forwards,
            inlinable,
        );
    }

    let _guard = scope_guard::install(func, &loops, cell_colors);
    let mut em = Emitter {
        enum_box_groups: RefCell::new(HashMap::default()),
        selection_preds: std::cell::OnceCell::new(),
        ws,
        ctx,
        interner,
        func,
        colors,
        cell_colors,
        frame,
        loops: &loops,
        rpo,
        inlinable,
        alias,
        emitted: vec![false; func.blocks.len()],
        label_refs: Vec::new(),
        control: ControlLines::default(),
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
    let structured = if let &[only] = rpo
        && matches!(func.blocks[only].term, Term::Return(_) | Term::Unreachable)
    {
        if !em.emit_block_statements(out, only) {
            em.emit_term(out, &func.blocks[only].term, only);
        }
        true
    } else {
        structured::Shape::analyze(
            func,
            entry,
            rpo,
            &loops,
            |b| em.is_empty_forwarder(b),
            |b| crate::raises(ws, func, b),
        )
        .map(|shape| structured::emit_body(&mut em, &shape, entry, out))
        .is_some()
    };
    if !structured {
        em.label_refs = block::label_ref_counts(func, rpo, &loops);
        for &bb in rpo {
            em.emit_block(out, bb); // self-skips an already-emitted block (loop body)
        }
    }
    if ctx.frame_limits() {
        let declared =
            params.len() + top_locals + loops.values().map(ForLoop::control_locals).sum::<usize>();
        check_body_limits(
            ctx,
            lua_name,
            func,
            &out[body_start..],
            declared,
            &em.control,
            body_start,
        );
    }
    out.push_str("end");
}

impl Emitter<'_> {
    fn is_empty_forwarder(&self, b: Idx<Block>) -> bool {
        let r = raw(b) as usize;
        self.tail_forwards[r].is_none()
            && self.kills.at_entry(b).is_empty()
            && self.func.blocks[b]
                .insts
                .iter()
                .all(|&id| !self.suppressed[raw(id) as usize] && !self.emits_statement(id))
    }
}

/// The blocks every path from which ends in a call that never returns.
fn raising_blocks(ws: &Workspace, func: &MirFunc) -> Vec<Idx<Block>> {
    let mut raising = vec![false; func.blocks.len()];
    for (b, blk) in func.blocks.iter() {
        raising[raw(b) as usize] = crate::raises(ws, func, b) && !blk.insts.is_empty();
    }
    let mut changed = raising.iter().any(|&r| r);
    while changed {
        changed = false;
        for (b, blk) in func.blocks.iter() {
            if raising[raw(b) as usize] || !matches!(blk.term, Term::Goto(_) | Term::Branch { .. })
            {
                continue;
            }
            let mut all = true;
            skull_mir::cfg::successors(&blk.term, |s| all &= raising[raw(s) as usize]);
            if all {
                raising[raw(b) as usize] = true;
                changed = true;
            }
        }
    }
    func.blocks
        .iter()
        .filter(|(b, _)| raising[raw(*b) as usize])
        .map(|(b, _)| b)
        .collect()
}
