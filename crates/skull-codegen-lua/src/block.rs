use super::*;

impl Emitter<'_> {
    /// Record that the line starting at the end of `out` renders `id`.
    pub(crate) fn mark_line(&self, out: &str, id: Idx<Inst>) {
        if let (Some(marks), Some(span)) = (self.line_marks, self.func.inst_spans.get(id)) {
            marks.borrow_mut().push((out.len(), *span));
        }
    }

    /// [`Self::mark_line`] for an operand a terminator renders in place. A
    /// register operand was computed by an earlier statement and is not this
    /// line's source.
    pub(crate) fn mark_inline_operand(&self, out: &str, id: Idx<Inst>) {
        if self.inlinable[raw(id) as usize] {
            self.mark_line(out, id);
        }
    }

    /// `id` renders in place and evaluates a call or a possible error, so a
    /// construct that would drop the expression must still run it.
    pub(crate) fn folds_effect(&self, id: Idx<Inst>) -> bool {
        if !self.inlinable[raw(id) as usize] {
            return false;
        }
        let inst = &self.func.insts[id];
        let mut effect = skull_mir::sinking::may_throw(inst);
        inst.uses_each(&mut |u| effect = effect || self.folds_effect(u));
        effect
    }

    /// `id` renders in place as an expression of registers, cells and
    /// constants that reads no other memory, calls nothing and cannot fail.
    pub(crate) fn folds_pure(&self, id: Idx<Inst>) -> bool {
        if !self.inlinable[raw(id) as usize] {
            return true;
        }
        let inst = &self.func.insts[id];
        if matches!(inst, Inst::Load(_)) {
            return true;
        }
        let mut pure =
            skull_mir::sinking::always_inlinable(inst) && !skull_mir::sinking::may_throw(inst);
        inst.uses_each(&mut |u| pure = pure && self.folds_pure(u));
        pure
    }

    pub(crate) fn write_kills(&self, out: &mut String, locals: &[regalloc::KillLocal]) {
        for local in locals {
            out.push(' ');
            match *local {
                regalloc::KillLocal::Value(c) => {
                    scope_guard::register(self.func, c);
                    out.push('r');
                    push_uint(out, c);
                }
                regalloc::KillLocal::Cell(c) => {
                    scope_guard::cell_kill(self.func, c);
                    out.push('c');
                    push_uint(out, c);
                }
            }
            out.push_str(" = nil\n");
        }
    }

    fn write_pending_kills(&self, out: &mut String, bb: Idx<Block>, after: Option<Idx<Inst>>) {
        let locals = match after {
            Some(id) => self.kills.after_inst(id),
            None => self.kills.at_entry(bb),
        };
        self.write_kills(out, locals);
    }

    /// Whether `emit_inst` writes a statement for `id` (kills attach after it).
    pub(crate) fn emits_statement(&self, id: Idx<Inst>) -> bool {
        !self.suppressed[raw(id) as usize]
            && !self.inlinable[raw(id) as usize]
            && !self.hoist.contains_key(&id)
    }

    pub(crate) fn write_global(&self, out: &mut String, name: ustr::Ustr) {
        let name = skull_mir::mangle::symbol_of_key(name);
        if let Some(&slot) = self.state_slots.get(&name) {
            out.push_str(self.ctx.state_table());
            out.push('[');
            push_uint(out, slot);
            out.push(']');
        } else if let Some(slot) = self.static_slots.get(&name) {
            out.push_str(self.ctx.state_table());
            out.push('[');
            write_lua_string(out, slot);
            out.push(']');
        } else {
            out.push_str(self.bound_name(name).as_str());
        }
    }

    /// Records `id` as an allocation site of `kind` when allocation sites are
    /// being collected, and returns it when the emission is instrumented.
    pub(crate) fn record_allocation_site(
        &self,
        id: Idx<Inst>,
        kind: Option<EmittedAllocationKind>,
        lua_byte_offset: usize,
    ) -> Option<EmittedAllocationSite> {
        let allocation = self.allocation?;
        let kind = kind?;
        let span = self.func.inst_spans.get(id).copied();
        let site = EmittedAllocationSite {
            id: allocation_site_id(allocation.body, id, kind, span),
            kind,
            span,
            lua_body_line: 0,
            mir_inst: raw(id),
            lua_byte_offset,
        };
        allocation.sites.borrow_mut().push(site.clone());
        allocation.instrument.then_some(site)
    }

    pub(crate) fn emit_block(&mut self, out: &mut String, bb: Idx<Block>) {
        if self.emitted[raw(bb) as usize] {
            return;
        }
        self.emitted[raw(bb) as usize] = true;
        self.drop_fallthrough_goto(out, bb);
        self.write_block_label(out, bb);
        self.emit_block_contents(out, bb);
    }

    /// Drops a fallthrough goto, but not after a label: LuaJIT nests one syntax
    /// level per label in a run, so a long chain of empty blocks would overflow.
    fn drop_fallthrough_goto(&mut self, out: &mut String, bb: Idx<Block>) {
        if let Some((start, end, target)) = self.control.last_goto.take()
            && target == bb
            && end == out.len()
            && !out[..start].ends_with("::\n")
        {
            out.truncate(start);
            self.control.pop_goto(start);
            self.label_refs[raw(bb) as usize] -= 1;
        }
    }

    fn write_block_label(&mut self, out: &mut String, bb: Idx<Block>) {
        if self.label_refs[raw(bb) as usize] != 0 {
            self.write_label(out, Label::Block, bb);
        }
    }

    pub(crate) fn reject_pass_residue(&self, residue: &str, pass: &str) {
        self.ctx.reject(
            self.func.name.to_string(),
            CompilerError::new_no_location(format!(
                "internal compiler error: {residue} survived the {pass} and reached Lua \
                 emission in `{}`",
                self.func.name
            )),
        );
    }

    fn emit_block_contents(&mut self, out: &mut String, bb: Idx<Block>) {
        if self.emit_block_statements(out, bb) {
            return;
        }
        let term = &self.func.blocks[bb].term;
        if let Term::Goto(t) = term {
            let t = *t;
            if !self.emitted[raw(t) as usize] {
                if self.loops.contains_key(&t) {
                    self.expand_for(out, t);
                    return;
                }
            }
        }
        self.emit_term(out, &self.func.blocks[bb].term, bb);
    }

    /// The statements of `bb` without its terminator. True when a tail
    /// forward already ended the block with its `return`.
    pub(crate) fn emit_block_statements(&mut self, out: &mut String, bb: Idx<Block>) -> bool {
        let insts: &[Idx<Inst>] = &self.func.blocks[bb].insts;
        let tail = self.tail_forwards[raw(bb) as usize];
        let emitted_len = insts.len() - tail.map_or(0, <[_]>::len);
        // Kills are written before the next statement, so the ones after the
        // last statement of a returning block are dropped: `return` (and a
        // tail call, which replaces the frame) drops every slot itself.
        let returns = tail.is_some()
            || match self.func.blocks[bb].term {
                Term::Return(_) | Term::Unreachable => true,
                Term::Goto(s) => self.kills.reaches_return(s),
                _ => false,
            };
        // In a returning block, only a statement that may call can expose a
        // stale slot to an exit; kills past the last such statement are dropped.
        let last_impure = if returns {
            insts[..emitted_len]
                .iter()
                .rposition(|&id| {
                    self.emits_statement(id)
                        && !regalloc::statement_is_pure(self.func, self.inlinable, id)
                })
                .map(|i| insts[i])
        } else {
            None
        };
        let mut needs_kills = !returns || last_impure.is_some();
        let mut pending = None;
        for &id in &insts[..emitted_len] {
            if !self.emits_statement(id) {
                continue;
            }
            if needs_kills {
                self.write_pending_kills(out, bb, pending);
            }
            self.emit_statement(out, id);
            pending = Some(id);
            if Some(id) == last_impure {
                needs_kills = false;
            }
        }
        if needs_kills {
            self.write_pending_kills(out, bb, pending);
        }
        if let Some(tail) = tail {
            self.emit_tail_forward(out, tail[0]);
            return true;
        }
        false
    }

    fn emit_tail_forward(&self, out: &mut String, call: Idx<Inst>) {
        if matches!(
            self.func.insts[call],
            Inst::Call { .. } | Inst::CallMulti { .. }
        ) {
            self.mark_line(out, call);
        }
        out.push_str(" do return ");
        self.write_call(out, call);
        out.push_str(" end\n");
    }

    /// The init reads the slot cell (the preheader stored `lo` there), so it
    /// never depends on `lo`'s register being live at the loop head.
    pub(crate) fn write_for_head(&self, out: &mut String, header: Idx<Block>) {
        let cl = &self.loops[&header];
        scope_guard::open(self.func, header);
        out.push_str(" for ");
        match &cl.control {
            ForControl::Numeric {
                slot,
                hi,
                inclusive,
            } => {
                write_cellname(out, self.func, self.frame, self.cell_colors, *slot);
                out.push_str(" = ");
                write_cellname(out, self.func, self.frame, self.cell_colors, *slot);
                out.push_str(", ");
                match hi {
                    HiLimit::Value(v) => self.write_opnd(out, *v),
                    HiLimit::Cell(c) => {
                        write_cellname(out, self.func, self.frame, self.cell_colors, *c)
                    }
                }
                out.push_str(if *inclusive { " do\n" } else { " - 1 do\n" });
            }
            ForControl::Map { key, value, table } => {
                let h = self.func.emit_block_id(header);
                write!(out, "mk{h}, mv{h} in next, ").unwrap();
                self.write_opnd(out, *table);
                out.push_str(" do\n ");
                write_cellname(out, self.func, self.frame, self.cell_colors, *key);
                out.push_str(", ");
                write_cellname(out, self.func, self.frame, self.cell_colors, *value);
                writeln!(out, " = mk{h}, mv{h}").unwrap();
            }
        }
        // Declared inside the `for` block so LuaJIT's snapshot liveness can prune
        // them at loop exits; otherwise side traces inherit them and `asm_head_side`
        // aborts past ~16 inherited FP values.
        let mut regs = self
            .color_scope
            .iter()
            .enumerate()
            .filter(|(_, scope)| **scope == Some(header))
            .map(|(color, _)| color as u32);
        if let Some(first) = regs.next() {
            out.push_str(" local r");
            push_uint(out, first);
            for color in regs {
                out.push_str(", r");
                push_uint(out, color);
            }
            out.push('\n');
        }
    }

    /// The latch keeps only its label, so a body `goto bb<latch>` re-iterates.
    pub(crate) fn expand_for(&mut self, out: &mut String, header: Idx<Block>) {
        let cl = &self.loops[&header];
        let (body_entry, latch, exit) = (cl.body_entry, cl.latch, cl.exit);
        let region: HashSet<Idx<Block>> = cl.region.iter().copied().collect();
        self.emitted[raw(header) as usize] = true;
        // Dead on entry to the header is dead for the whole loop unless
        // redefined, so the header's kills go once, above the `for`.
        self.write_kills(out, self.kills.at_entry(header));
        self.write_for_head(out, header);
        self.emit_block(out, body_entry);
        for &bb in self.rpo {
            if region.contains(&bb) {
                self.emit_block(out, bb);
            }
        }
        self.emitted[raw(latch) as usize] = true;
        self.drop_fallthrough_goto(out, latch);
        self.write_block_label(out, latch);
        if latch != header {
            self.write_kills(out, self.kills.at_entry(latch));
        }
        out.push_str(" end\n");
        scope_guard::close(self.func, header);
        self.write_goto(out, Label::Block, exit);
    }
}

/// Edges naming each block. Counts may over-state the gotos emitted but never
/// under-state them, so a label is never dropped while a goto names it.
pub(crate) fn label_ref_counts(
    func: &MirFunc,
    rpo: &[Idx<Block>],
    loops: &HashMap<Idx<Block>, ForLoop>,
) -> Vec<u32> {
    let mut refs = vec![0u32; func.blocks.len()];
    for &bb in rpo {
        match &func.blocks[bb].term {
            Term::Goto(t) => refs[raw(*t) as usize] += 1,
            Term::Branch {
                then_bb, else_bb, ..
            } => {
                refs[raw(*then_bb) as usize] += 1;
                refs[raw(*else_bb) as usize] += 1;
            }
            _ => {}
        }
    }
    // A counted loop's header branch is the `for` line: its edge into the body
    // is the loop entry, which names no label.
    for (&header, cl) in loops {
        if let Term::Branch {
            then_bb, else_bb, ..
        } = func.blocks[header].term
            && (then_bb == cl.body_entry || else_bb == cl.body_entry)
        {
            refs[raw(cl.body_entry) as usize] -= 1;
        }
    }
    refs
}
