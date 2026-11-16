//! Instructions, call-argument forwards, inline(lua) splices and operands.

use super::*;

impl Emitter<'_> {
    pub(crate) fn emit_inst(&mut self, out: &mut String, id: Idx<Inst>) {
        // A tree-folded value emits NO statement; its expression is rendered at
        // its single use site by `opnd`.
        if self.inlinable[raw(id) as usize] {
            return;
        }
        // A hoisted const ctor emits NO statement -- its value is the module-scope
        // global, referenced via `write_opnd`. (Skips the dead `rN = ctor` a
        // multi-use hoisted const would otherwise materialize.)
        if self.hoist.contains_key(&id) {
            return;
        }
        self.mark_line(out, id);
        // An `@inline inline(lua)` call is spliced inline (body in the caller's
        // scope) instead of emitting a `callee(args)` call -- see the method.
        if matches!(&self.func.insts[id], Inst::Call { .. })
            && self.try_emit_inline_lua_splice(out, id)
        {
            return;
        }
        let inst = &self.func.insts[id];
        if inst.produces_value() {
            out.push(' ');
            // Only a discarded call result reaches here without a register.
            if self.colors[raw(id) as usize] != u32::MAX
                || regalloc::alias_at(self.alias, id).is_some()
            {
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

    /// Splice an `@inline inline(lua)` `Call` inline, if `id` is one. Returns
    /// true iff it spliced; else the caller emits the normal `callee(args)`.
    /// The lexical name this call targets. A call the lowerer gave a concrete
    /// instance (its value args splatted to leaves) targets that instance's
    /// definition. The same instance object drives definition and call spelling,
    /// so the two cannot drift. Bound calls use the factory parameter instead.
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

    fn try_emit_inline_lua_splice(&mut self, out: &mut String, id: Idx<Inst>) -> bool {
        let Inst::Call { func, args } = &self.func.insts[id] else {
            return false;
        };
        let callee = func_of(self.ws, *func);
        self.splice_inline_lua(out, callee, args, id)
    }

    /// Emit `callee`'s `inline(lua)` body inline at this invocation -- a `do … end`
    /// block: each param bound to a `local` = its arg, a tail `return X` rewritten
    /// to `<result> = X`. Returns true iff spliced (false leaves the call to the
    /// caller). Splicing keeps any cdata the body produces in the CALLER's scope
    /// so LuaJIT can sink it; a call boundary forces it to materialize.
    ///
    /// Conservative: only `@inline`, plain-param inline(lua) (no `@scalar_abi`
    /// leaf expansion) whose body is a single bare expression, a statement-shaped
    /// unit body, or a single TAIL `return`. Early / multiple returns fall back to
    /// a call -- rewriting those line-by-line would drop the control flow.
    fn splice_inline_lua(
        &mut self,
        out: &mut String,
        callee: &Function,
        args: &[Idx<Inst>],
        result: Idx<Inst>,
    ) -> bool {
        // Only a lua-targeted inline body can splice into lua output (a js body
        // would be nonsense here).
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
        let mut lv = String::new();
        if self.colors[raw(result) as usize] == u32::MAX
            && regalloc::alias_at(self.alias, result).is_none()
        {
            lv.push_str("local _");
        } else {
            self.write_value_name(&mut lv, result);
        }

        // A parameter the body only reads takes a plain operand in place of a
        // local copy of it: the body writes no register of the caller, and
        // its result register is assigned by the last statement only.
        let analyzed = InlineLuaBody::new(body.as_str());
        let splice_start = out.len();
        let first_site = self.allocation.map_or(0, |a| a.sites.borrow().len());
        let mut locals = String::new();
        let mut stand_ins = Vec::new();
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
                stand_ins.push((p.name.as_str(), operand));
            } else {
                let _ = writeln!(locals, "  local {} = {operand}", p.name);
            }
        }
        let subs: Vec<(&str, &str)> = stand_ins.iter().map(|(p, o)| (*p, o.as_str())).collect();
        let body = analyzed.substitute_params(&subs);
        // A discarded call result runs as a bare call; any other discarded
        // result still needs a local to evaluate it into.
        let result = result_expr(&body, shape);
        let discarded = lv.starts_with("local");
        let bare = discarded && result.is_some_and(is_call_statement);
        let block =
            !locals.is_empty() || discarded && !bare && result.is_some() || declares_names(&body);
        if block {
            out.push_str(" do\n");
        }
        out.push_str(&locals);
        render_spliced_inline_lua(out, &body, shape, (!bare).then_some(lv.as_str()));
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

    /// Render a reference to value `id` as an operand directly into `out`: its
    /// register `r<color>`, or — if it was tree-folded — its expression in
    /// parentheses, recursively. Every value-operand reference goes through here.
    /// True if `id` renders in operand position as a bare name or a literal --
    /// a register, a cell, a hoisted module global or a constant -- so an inline
    /// shape may spell it twice without re-evaluating an expression or
    /// repeating an allocation (`MakeVec` and `StringConcat` fold too).
    pub(crate) fn opnd_is_plain(&self, id: Idx<Inst>) -> bool {
        if self.hoist.contains_key(&id) || !self.inlinable[raw(id) as usize] {
            return true;
        }
        matches!(self.func.insts[id], Inst::Load(_) | Inst::Const(_))
    }

    /// A folded `IntToFloat` of a plain operand renders as that operand alone,
    /// so it reads no more and evaluates no more than a plain operand does.
    pub(crate) fn plain_int_to_float(&self, id: Idx<Inst>) -> bool {
        matches!(self.func.insts[id], Inst::IntToFloat(x) if self.opnd_is_plain(x))
    }

    pub(crate) fn write_opnd(&self, out: &mut String, id: Idx<Inst>) {
        if self.inlinable[raw(id) as usize] {
            if let Inst::Load(cell) = self.func.insts[id] {
                write_cellname(out, self.func, self.frame, self.cell_colors, cell);
                return;
            }
        }
        // int->float subsumption is identity on luajit (all doubles): a FOLDED
        // (inlinable) coercion renders exactly as the inner operand in this
        // position -- no register, no extra precedence wrap -- so emitted Lua
        // stays byte-identical. A reused one is folded only when its operand is
        // a bare local (`alias_int_to_float`); otherwise it keeps its own
        // register (fall through) and the `rN = <inner>` def carries the identity.
        if self.inlinable[raw(id) as usize] {
            if let Inst::IntToFloat(x) = self.func.insts[id] {
                return self.write_opnd(out, x);
            }
        }
        // A hoisted all-constant value ctor renders as its module-scope global,
        // whether it would otherwise inline or materialize a register.
        if let Some(&g) = self.hoist.get(&id) {
            out.push_str(&self.hoist_names[g as usize]);
            return;
        }
        if self.inlinable[raw(id) as usize] {
            // Always parenthesize the folded expression so it composes safely in
            // the parent context regardless of precedence/adjacency. (Notably a
            // bare negative literal under unary `-` would form `--` — a Lua
            // comment — without the wrap.)
            out.push('(');
            self.write_value(out, id, &self.func.insts[id]);
            out.push(')');
        } else {
            self.write_value_name(out, id);
        }
    }

    /// The local holding non-folded value `id`: its register, or the cell it
    /// is computed in.
    fn write_value_name(&self, out: &mut String, id: Idx<Inst>) {
        match regalloc::alias_at(self.alias, id) {
            Some(cell) => write_cellname(out, self.func, self.frame, self.cell_colors, cell),
            None => write_vname(out, self.func, &self.colors, id),
        }
    }

    /// Collect the flat operand list of a `StringConcat` tree, descending into
    /// child `StringConcat`s that are inlined here (single-use, folded into this
    /// expression). A non-inlinable child stays a leaf -- it is a separate `..`
    /// already materialized in its own cell, so flattening past it would be wrong.
    pub(crate) fn collect_concat(&self, parts: &mut Vec<Idx<Inst>>, id: Idx<Inst>) {
        concat_run(self.func, self.inlinable, parts, id);
    }

    /// `write_opnd` into a fresh `String` — for the few cold sites that need an
    /// owned operand string (loop-limit arithmetic in `expand_for`).
    pub(crate) fn opnd_str(&self, id: Idx<Inst>) -> String {
        let mut s = String::new();
        self.write_opnd(&mut s, id);
        s
    }
}
