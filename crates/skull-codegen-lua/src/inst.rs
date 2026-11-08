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
        if let Some(forward) = &self.call_arg_forwards[raw(id) as usize] {
            self.emit_call_arg_forward(out, forward);
            return;
        }
        // An `@inline inline(lua)` call is spliced inline (body in the caller's
        // scope) instead of emitting a `callee(args)` call -- see the method.
        if matches!(&self.func.insts[id], Inst::Call { .. })
            && self.try_emit_inline_lua_splice(out, id)
        {
            return;
        }
        let inst = &self.func.insts[id];
        if inst.produces_value() {
            out.push_str("  ");
            write_vname(out, self.func, &self.colors, id);
            out.push_str(" = ");
            self.write_value(out, id, inst);
            out.push('\n');
        } else {
            self.write_effect(out, id, inst);
        }
    }

    fn emit_call_arg_forward(&self, out: &mut String, forward: &CallArgForward) {
        out.push_str("  ");
        write_vname(out, self.func, &self.colors, forward.consumer);
        out.push_str(" = ");
        self.write_call_arg_forward(out, forward);
        out.push('\n');
    }

    pub(crate) fn write_call_arg_forward(&self, out: &mut String, forward: &CallArgForward) {
        out.push_str(&self.callee_name(forward.consumer, forward.func));
        out.push('(');
        self.write_csv(out, &forward.prefix);
        if !forward.prefix.is_empty() {
            out.push_str(", ");
        }
        self.write_forward_call(out, &forward.producer);
        out.push(')');
    }

    /// Splice an `@inline inline(lua)` `Call` inline, if `id` is one. Returns
    /// true iff it spliced; else the caller emits the normal `callee(args)`.
    /// The emitted symbol this call targets. A call the lowerer gave a concrete
    /// instance (its value args splatted to leaves) targets that instance's
    /// definition. The same instance object drives definition and call spelling,
    /// so the two cannot drift.
    pub(crate) fn callee_name(&self, id: Idx<Inst>, func: FuncRef) -> String {
        call_target_symbol(self.ws, self.interner, self.func, id, func)
    }

    fn try_emit_inline_lua_splice(&self, out: &mut String, id: Idx<Inst>) -> bool {
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
        &self,
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
        // A `Call`/`Index` value is never tree-folded here (`always_inlinable`
        // excludes them), so `result` has a register.
        let mut lv = String::new();
        write_vname(&mut lv, self.func, &self.colors, result);

        out.push_str("  do\n");
        for (p, a) in callee.param_decls.iter().zip(args.iter()) {
            let _ = write!(out, "    local {} = ", p.name);
            self.write_opnd(out, *a);
            out.push('\n');
        }
        render_spliced_inline_lua(out, body.as_str(), shape, &lv);
        out.push_str("  end\n");
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
            write_vname(out, self.func, &self.colors, id);
        }
    }

    /// Collect the flat operand list of a `StringConcat` tree, descending into
    /// child `StringConcat`s that are inlined here (single-use, folded into this
    /// expression). A non-inlinable child stays a leaf -- it is a separate `..`
    /// already materialized in its own cell, so flattening past it would be wrong.
    pub(crate) fn collect_concat(&self, parts: &mut Vec<Idx<Inst>>, id: Idx<Inst>) {
        concat_run(self.func, &self.inlinable, parts, id);
    }

    /// `write_opnd` into a fresh `String` — for the few cold sites that need an
    /// owned operand string (loop-limit arithmetic in `expand_for`).
    pub(crate) fn opnd_str(&self, id: Idx<Inst>) -> String {
        let mut s = String::new();
        self.write_opnd(&mut s, id);
        s
    }
}
