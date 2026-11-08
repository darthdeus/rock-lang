//! Blocks, labels, kills, allocation sites and counted-loop expansion.

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

    pub(crate) fn write_kills(&self, out: &mut String, locals: &[regalloc::KillLocal]) {
        for local in locals {
            out.push_str("  ");
            match *local {
                regalloc::KillLocal::Value(c) => {
                    out.push('r');
                    push_uint(out, c);
                }
                regalloc::KillLocal::Cell(c) => {
                    out.push('c');
                    push_uint(out, c);
                }
            }
            out.push_str(" = nil\n");
        }
    }

    /// The kills due before the next statement of `bb`: the block's entry kills
    /// when no statement has been written yet, else the ones after `after`.
    fn write_pending_kills(
        &self,
        out: &mut String,
        bb: Idx<Block>,
        at_entry: bool,
        after: Option<Idx<Inst>>,
    ) {
        let locals: &[regalloc::KillLocal] = if at_entry {
            &self.kills.at_entry[raw(bb) as usize]
        } else {
            match after {
                Some(id) => &self.kills.after_inst[raw(id) as usize],
                None => &[],
            }
        };
        self.write_kills(out, locals);
    }

    /// Whether `emit_inst` writes a statement for `id` (kills attach after it).
    fn emits_statement(&self, id: Idx<Inst>) -> bool {
        !self.suppressed[raw(id) as usize]
            && !self.inlinable[raw(id) as usize]
            && !self.hoist.contains_key(&id)
    }
    pub(crate) fn has_state_slot(&self, name: ustr::Ustr) -> bool {
        let symbol = skull_mir::mangle::symbol_of_key(name);
        self.state_slots.contains_key(&symbol) || self.static_slots.contains_key(&symbol)
    }

    pub(crate) fn write_state_slot(&self, out: &mut String, name: ustr::Ustr) {
        let name = skull_mir::mangle::symbol_of_key(name);
        if let Some(&slot) = self.state_slots.get(&name) {
            push_uint(out, slot);
        } else if let Some(slot) = self.static_slots.get(&name) {
            out.push_str(&lua_string(slot));
        } else {
            unreachable!("state slot checked before emission")
        }
    }

    pub(crate) fn record_value_allocation_site(
        &self,
        id: Idx<Inst>,
        inst: &Inst,
        lua_byte_offset: usize,
    ) -> Option<EmittedAllocationSite> {
        let allocation = self.allocation?;
        let kind = emitted_value_allocation_kind(inst)?;
        let site = self.record_allocation_site(allocation, id, kind, lua_byte_offset);
        allocation.instrument.then_some(site)
    }

    pub(crate) fn record_effect_allocation_site(
        &self,
        id: Idx<Inst>,
        inst: &Inst,
        lua_byte_offset: usize,
    ) -> Option<EmittedAllocationSite> {
        let allocation = self.allocation?;
        let kind = emitted_effect_allocation_kind(inst)?;
        let site = self.record_allocation_site(allocation, id, kind, lua_byte_offset);
        allocation.instrument.then_some(site)
    }

    fn record_allocation_site(
        &self,
        allocation: AllocationEmission<'_>,
        id: Idx<Inst>,
        kind: EmittedAllocationKind,
        lua_byte_offset: usize,
    ) -> EmittedAllocationSite {
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
        site
    }

    pub(crate) fn emit_block(&mut self, out: &mut String, bb: Idx<Block>) {
        if self.emitted[raw(bb) as usize] {
            return; // already emitted (e.g. as a loop body block)
        }
        self.emitted[raw(bb) as usize] = true;
        self.write_block_label(out, bb);
        self.emit_block_contents(out, bb);
    }

    fn write_block_label(&self, out: &mut String, bb: Idx<Block>) {
        out.push_str("  ::bb");
        push_uint(out, self.func.emit_block_id(bb));
        out.push_str("::\n");
    }

    /// The label an edge to `t` names: `t`'s own block label.
    pub(crate) fn write_label_ref(&self, out: &mut String, t: Idx<Block>) {
        out.push_str("bb");
        push_uint(out, self.func.emit_block_id(t));
    }

    pub(crate) fn write_goto(&self, out: &mut String, t: Idx<Block>) {
        out.push_str("  goto ");
        self.write_label_ref(out, t);
        out.push('\n');
    }

    /// The statements of `bb` and its terminator, without its label.
    fn emit_block_contents(&mut self, out: &mut String, bb: Idx<Block>) {
        let insts: &[Idx<Inst>] = &self.func.blocks[bb].insts;
        let tail = self.tail_forwards[raw(bb) as usize].as_ref();
        let emitted_len = tail.map_or(insts.len(), |tail| tail.first_suppressed);
        // Kills are written before the next statement, so the ones after the
        // last statement of a returning block are dropped: `return` (and a
        // tail call, which replaces the frame) drops every slot itself.
        let returns = tail.is_some()
            || matches!(
                self.func.blocks[bb].term,
                Term::Return(_) | Term::Unreachable
            );
        // In a returning block, only a statement that may call can expose a
        // stale slot to an exit; kills past the last such statement are dropped.
        let last_impure = if returns {
            insts[..emitted_len]
                .iter()
                .rposition(|&id| {
                    self.emits_statement(id)
                        && !regalloc::statement_is_pure(self.func, &self.inlinable, id)
                })
                .map(|i| insts[i])
        } else {
            None
        };
        let mut live_region = !returns || last_impure.is_some();
        // Indexed rather than borrowed: `emit_inst` needs `&mut self`.
        let mut pending: Option<Idx<Inst>> = None;
        let mut first = true;
        for &id in &insts[..emitted_len] {
            if self.suppressed[raw(id) as usize] {
                continue;
            }
            if self.emits_statement(id) {
                if live_region {
                    self.write_pending_kills(out, bb, first, pending);
                }
                first = false;
                pending = Some(id);
            }
            self.emit_inst(out, id);
            if Some(id) == last_impure {
                live_region = !returns;
            }
        }
        if live_region {
            self.write_pending_kills(out, bb, first, pending);
        }
        if let Some(tail) = tail {
            self.emit_tail_forward(out, tail);
            return;
        }
        // A goto into an un-emitted counted-loop header becomes a structured
        // `for` (the preheader falls through into it); everything else is a goto.
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

    fn emit_tail_forward(&self, out: &mut String, tail: &TailForward) {
        if let TailForwardCall::Direct { call, .. } = &tail.call {
            self.mark_line(out, *call);
        }
        out.push_str("  do return ");
        self.write_forward_call(out, &tail.call);
        out.push_str(" end\n");
    }

    pub(crate) fn write_forward_call(&self, out: &mut String, call: &TailForwardCall) {
        match call {
            TailForwardCall::Direct { call, func, args } => {
                if let Some(forward) = &self.call_arg_forwards[raw(*call) as usize] {
                    self.write_call_arg_forward(out, forward);
                    return;
                }
                out.push_str(&self.callee_name(*call, *func));
                out.push('(');
                self.write_csv(out, args);
            }
            TailForwardCall::Value { callee, args } => {
                self.write_opnd(out, *callee);
                out.push('(');
                self.write_csv(out, args);
            }
        }
        out.push(')');
    }

    /// Emit a recognized counted loop as `for <slot> = <slot>, hi[-1] do .. end`.
    /// The init reads the slot cell (the preheader's store set it to `lo`), so it
    /// never depends on `lo`'s register being live at the loop head. The header's
    /// compare/branch and the latch's increment are subsumed by the `for`; the
    /// latch keeps only its label so a body `goto bb<latch>` (continue / normal
    /// fall-through) lands at the loop tail and re-iterates.
    pub(crate) fn expand_for(&mut self, out: &mut String, header: Idx<Block>) {
        let cl = &self.loops[&header];
        let (slot, inclusive, body_entry, latch, exit) =
            (cl.slot, cl.inclusive, cl.body_entry, cl.latch, cl.exit);
        let region: HashSet<Idx<Block>> = cl.region.iter().copied().collect();
        let slot_name = cellname(self.func, self.frame, self.cell_colors, slot);
        let hi = match &cl.hi {
            HiLimit::Value(v) => self.opnd_str(*v),
            HiLimit::Cell(c) => cellname(self.func, self.frame, self.cell_colors, *c),
        };
        let limit = if inclusive { hi } else { format!("{hi} - 1") };

        self.emitted[raw(header) as usize] = true;
        // Dead on entry to the header is dead for the whole loop unless
        // redefined, so the header's kills go once, above the `for`.
        self.write_kills(out, &self.kills.at_entry[raw(header) as usize]);
        let _ = writeln!(out, "  for {slot_name} = {slot_name}, {limit} do");
        // Body-scoped temp registers: declared INSIDE the `for` block, above the
        // loop-control slots, so LuaJIT's snapshot liveness can prune them at
        // loop exits (else they pin their values into every side trace and
        // `asm_head_side` aborts past ~16 inherited FP values — the
        // "NYI: register coalescing too complex" blacklist class).
        let mut own: Vec<u32> = self
            .color_scope
            .iter()
            .enumerate()
            .filter(|(_, s)| **s == Some(header))
            .map(|(c, _)| c as u32)
            .collect();
        if !own.is_empty() {
            own.sort_unstable();
            out.push_str("  local ");
            for (i, &c) in own.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('r');
                push_uint(out, c);
            }
            out.push('\n');
        }
        self.emit_block(out, body_entry);
        for &bb in self.rpo {
            if region.contains(&bb) {
                self.emit_block(out, bb); // self-skips body_entry / already-emitted
            }
        }
        // Latch label only (the `for` does the increment); then close + the
        // loop-done edge (header's false branch).
        self.emitted[raw(latch) as usize] = true;
        out.push_str("  ::bb");
        push_uint(out, self.func.emit_block_id(latch));
        out.push_str("::\n");
        self.write_kills(out, &self.kills.at_entry[raw(latch) as usize]);
        out.push_str("  end\n");
        self.write_goto(out, exit);
    }
}
