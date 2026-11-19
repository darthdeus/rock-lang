use super::*;

impl Emitter<'_> {
    pub(crate) fn emit_statement(&mut self, out: &mut String, id: Idx<Inst>) {
        self.mark_line(out, id);
        if let Inst::Call { func, args } = &self.func.insts[id]
            && self.splice_inline_lua(out, func_of(self.ws, *func), args, id)
        {
            return;
        }
        let inst = &self.func.insts[id];
        if inst.produces_value() {
            out.push(' ');
            // Only a discarded call result reaches here without a register.
            if self.has_value_local(id) {
                self.write_value_name(out, id);
                out.push_str(" = ");
            }
            self.write_value(out, id, inst);
            out.push('\n');
        } else {
            self.write_effect(out, id, inst);
        }
    }

    pub(crate) fn write_call(&self, out: &mut String, id: Idx<Inst>) {
        match &self.func.insts[id] {
            Inst::Call { func, args } | Inst::CallMulti { func, args, .. } => {
                out.push_str(&self.callee_name(id, *func));
                out.push('(');
                if let Some(forward) = self.call_arg_forwards.get(id) {
                    self.write_csv(out, &args[..forward.prefix_len]);
                    if forward.prefix_len > 0 {
                        out.push_str(", ");
                    }
                    self.write_call(out, forward.producer);
                } else {
                    self.write_csv(out, self.call_args(*func, args));
                }
            }
            Inst::CallValue { callee, args } | Inst::CallValueMulti { callee, args, .. } => {
                self.write_opnd(out, *callee);
                out.push('(');
                self.write_csv(out, args);
            }
            _ => unreachable!("not a call"),
        }
        out.push(')');
    }

    pub(crate) fn callee_name(&self, id: Idx<Inst>, func: FuncRef) -> ustr::Ustr {
        let name = match self.func.spec_bindings.get(&id) {
            Some(inst) => self.ctx.instance_symbol(self.ws, self.interner, inst),
            None => self.ctx.mangled_name(self.ws, func),
        };
        self.bound_name(name)
    }

    pub(crate) fn bound_name(&self, name: ustr::Ustr) -> ustr::Ustr {
        self.bound
            .and_then(|plan| plan.parameter_for(name.as_str()))
            .unwrap_or(name)
    }

    /// Splicing keeps cdata the body produces in the caller's scope so LuaJIT
    /// can sink it; a call boundary forces it to materialize.
    fn splice_inline_lua(
        &mut self,
        out: &mut String,
        callee: &Function,
        args: &[Idx<Inst>],
        result: Idx<Inst>,
    ) -> bool {
        let Some((bone_core::hir::FnBackend::Lua, body)) = callee.inline_body else {
            return false;
        };
        let ann = |n: &str| callee.annotations.iter().any(|a| a.name.name.as_str() == n);
        if !ann("inline") || ann("scalar_abi") {
            return false;
        }
        if callee.param_decls.len() != args.len() {
            return false;
        }
        let Some(shape) = classify_inline_lua_body(body.as_str()) else {
            return false;
        };
        // A spliced call is never tree-folded, so `result` has a register
        // unless nothing reads it.
        let discarded = !self.has_value_local(result);
        let mut destination = String::new();
        if discarded {
            destination.push_str("local _");
        } else {
            self.write_value_name(&mut destination, result);
        }

        // A parameter the body only reads takes a plain operand in place of a
        // local copy of it: the body writes no register of the caller, and
        // its result register is assigned by the last statement only.
        let analyzed = InlineLuaBody::new(body.as_str());
        let splice_start = out.len();
        let first_site = self.allocation.map_or(0, |a| a.sites.borrow().len());
        let mut locals = String::new();
        let mut substitutions = Vec::new();
        for (p, a) in callee.param_decls.iter().zip(args.iter()) {
            let mut operand = String::new();
            self.write_opnd(&mut operand, *a);
            let standalone = operand
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
                || one_group(&operand);
            // A pure expression may move to its read if it is evaluated once.
            // A materialized read is already a snapshot in a local operand.
            let movable = || {
                self.opnd_is_plain(*a)
                    || self.folds_pure(*a)
                        && analyzed
                            .param_reads(p.name.as_str())
                            .is_some_and(|n| n <= 1)
            };
            if standalone
                && !(operand.starts_with('(') && analyzed.opens_statement(p.name.as_str()))
                && !analyzed.contains_name(&operand)
                && movable()
                && analyzed.reads_param_only(p.name.as_str())
            {
                substitutions.push((p.name.as_str(), operand));
            } else {
                let _ = writeln!(locals, "  local {} = {operand}", p.name);
            }
        }
        let subs: Vec<_> = substitutions
            .iter()
            .map(|(p, o)| (*p, o.as_str()))
            .collect();
        let body = analyzed.substitute_params(&subs);
        // A discarded call result runs as a bare call; any other discarded
        // result still needs a local to evaluate it into.
        let result = result_expr(&body, shape);
        let bare = discarded && result.is_some_and(is_call_statement);
        let block =
            !locals.is_empty() || discarded && !bare && result.is_some() || declares_names(&body);
        if block {
            out.push_str(" do\n");
        }
        out.push_str(&locals);
        render_spliced_inline_lua(out, &body, shape, (!bare).then_some(destination.as_str()));
        if block {
            out.push_str(" end\n");
        }
        self.control.raw_lua(&out[splice_start..], splice_start);
        // Operands were rendered into scratch strings, so their allocation
        // sites recorded scratch offsets. An instrumented site is found by its
        // ID; an uninstrumented one gets the splice's first line.
        if let Some(allocation) = self.allocation {
            for site in &mut allocation.sites.borrow_mut()[first_site..] {
                let id = format!("\"{:016x}\"", site.id);
                site.lua_byte_offset = out[splice_start..]
                    .find(&id)
                    .map_or(splice_start, |at| splice_start + at);
            }
        }
        true
    }

    /// True if `id` renders as a bare name or literal, so an inline shape may
    /// spell it twice without re-evaluating or re-allocating.
    pub(crate) fn opnd_is_plain(&self, id: Idx<Inst>) -> bool {
        if !self.inlinable[raw(id) as usize] || self.hoist.contains_key(&id) {
            return true;
        }
        matches!(self.func.insts[id], Inst::Load(_) | Inst::Const(_))
    }

    pub(crate) fn plain_int_to_float(&self, id: Idx<Inst>) -> bool {
        matches!(self.func.insts[id], Inst::IntToFloat(x) if self.opnd_is_plain(x))
    }

    pub(crate) fn write_opnd(&self, out: &mut String, id: Idx<Inst>) {
        let folded = self.inlinable[raw(id) as usize];
        if folded {
            match self.func.insts[id] {
                Inst::Load(cell) => {
                    return write_cellname(out, self.func, self.frame, self.cell_colors, cell);
                }
                // int->float is the identity on LuaJIT: every number is a double.
                Inst::IntToFloat(x) => return self.write_opnd(out, x),
                _ => {}
            }
        }
        if let Some(&g) = self.hoist.get(&id) {
            out.push_str(&self.hoist_names[g as usize]);
        } else if folded {
            // A bare negative literal under unary `-` would form `--`, a Lua comment.
            out.push('(');
            self.write_value(out, id, &self.func.insts[id]);
            out.push(')');
        } else {
            self.write_value_name(out, id);
        }
    }

    pub(crate) fn has_value_local(&self, id: Idx<Inst>) -> bool {
        self.colors[raw(id) as usize] != u32::MAX || regalloc::alias_at(self.alias, id).is_some()
    }

    fn write_value_name(&self, out: &mut String, id: Idx<Inst>) {
        match regalloc::alias_at(self.alias, id) {
            Some(cell) => write_cellname(out, self.func, self.frame, self.cell_colors, cell),
            None => write_vname(out, self.func, &self.colors, id),
        }
    }

    pub(crate) fn opnd_str(&self, id: Idx<Inst>) -> String {
        let mut s = String::new();
        self.write_opnd(&mut s, id);
        s
    }
}
