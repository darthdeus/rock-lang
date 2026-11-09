//! Block statements, returns and trampoline terminators.

use super::*;

impl Emitter<'_> {
    pub(super) fn emit_block_effects(&mut self, b: Idx<Block>) -> Result<(), CompilerError> {
        for iid in self.mf.blocks[b].insts.clone() {
            match &self.mf.insts[iid] {
                Inst::Store { cell, value } => {
                    let rhs = self.render_val(*value)?;
                    let line = format!("{} = {};", cell_name(self.mf, *cell), rhs);
                    self.push(&line);
                }
                Inst::StoreIndex {
                    base,
                    index,
                    value,
                    kind,
                } => {
                    let (b, i, v) = (
                        self.render_val(*base)?,
                        self.render_val(*index)?,
                        self.render_val(*value)?,
                    );
                    let line = match kind {
                        ContainerKind::NativeBuffer => format!("__nb_checked_set({b}, {i}, {v});"),
                        // Column storage has no JS emitter arm yet -- reject so
                        // the case lands in the SKIP histogram (the emitter
                        // worklist) instead of emitting a wrong single-slot write.
                        // One `value` operand cannot carry K leaves; lowering
                        // splits an element write into per-column stores.
                        ContainerKind::VecValueColumns(elem) => {
                            return Err(err(format!(
                                "JS subset: whole-element store into column-stored `[{elem}]` \
                                 reached emit -- lowering must split it per column"
                            )));
                        }
                        ContainerKind::VecValueStrided(elem) => {
                            return Err(err(format!(
                                "JS subset: whole-element store into strided `[{elem}]` reached emit"
                            )));
                        }
                        ContainerKind::VecColumn | ContainerKind::VecStrideSlot => {
                            format!("{b}[{i}] = {v};")
                        }
                        // Raw: the element index was bounds-checked once ahead of
                        // every leaf write. A JS boolean into a Uint8Array stores
                        // 1/0 already, so the write side needs no coercion.
                        ContainerKind::NativeBufferLeaf => format!("{b}[{i}] = {v};"),
                        // Lowering decomposes every whole-element write into
                        // per-leaf stores (`nb_leaf_element_store`), so reaching
                        // emit means the element has no C leaf layout to address.
                        ContainerKind::NativeBufferValue(elem) => {
                            return Err(err(format!(
                                "JS subset: whole-element store into `NativeBuffer<{elem}>` \
                                 reached emit -- `{elem}` has no C leaf layout"
                            )));
                        }
                        ContainerKind::Vec => format!("__vec_checked_set({b}, {i}, {v});"),
                        ContainerKind::Map => format!("__map_set({b}, {i}, {v});"),
                    };
                    self.push(&line);
                }
                // `k, v = next(t, k)` -- one step of Map iteration. JS has no
                // `next`, so a cursor-cached shim (`__map_next`, JS_MAP_ITER_RUNTIME)
                // returns the next key in the native Map's INSERTION order (null at
                // exhaustion; Map order is backend-defined by doctrine). The value
                // side reads `t.get(k)` -- no per-step pair allocation. The `table`
                // operand renders to a Load/Global/Field (idempotent) or a
                // materialized `v<N>` temp, so re-rendering it is safe.
                Inst::MapNext {
                    key_cell,
                    val_cell,
                    table,
                } => {
                    let t = self.render_val(*table)?;
                    let k = cell_name(self.mf, *key_cell);
                    let v = cell_name(self.mf, *val_cell);
                    self.push(&format!("{k} = __map_next({t}, {k});"));
                    self.push(&format!("{v} = ({k} === null ? null : {t}.get({k}));"));
                }
                // `base.field = value` (struct field / cursor mutation). A write
                // THROUGH a NativeBuffer element never reaches here: lowering
                // decomposes it into leaf stores against the element's typed views.
                Inst::StoreField { base, field, value } => {
                    let line = format!(
                        "{} = {};",
                        js_member(&self.render_val(*base)?, field)?,
                        self.render_val(*value)?
                    );
                    self.push(&line);
                }
                // A call in statement position (value unused): `nb_set(b, i, v)`
                // or a closure invoked for effect, `(f)(x);`.
                Inst::Call { .. } | Inst::CallValue { .. }
                    if self.use_count[raw(iid) as usize] == 0 =>
                {
                    let line = format!("{};", self.render_inst_expr(iid)?);
                    self.push(&line);
                }
                // A multi-value call (scalarized value-struct / tuple return):
                // `[d0, d1, ...] = callee(args);` destructures the array the
                // multi-return callee packs. Dest cells are hoist-declared at the
                // fn top, so this is an assignment, not a `let`. The leaves are
                // read back via `Load` of each dest.
                Inst::CallMulti { dests, func, args } => {
                    let call = self.render_call(iid, *func, args)?;
                    // An intrinsic is ONE JS expression: when its result type has
                    // more than one lane (a tuple-typed `A` from `tuple_first`),
                    // the lanes are read off that value rather than the callee's
                    // return bank, which no intrinsic ever writes.
                    let line = match self.intrinsic_lane_paths(iid, *func, dests.len())? {
                        Some(paths) => {
                            let temp = format!("__rock_mv{}", raw(iid));
                            let mut line = format!("let {temp} = {call};");
                            for (dest, path) in dests.iter().zip(paths) {
                                line.push_str(&format!(
                                    " {} = {temp}{path};",
                                    cell_name(self.mf, *dest)
                                ));
                            }
                            line
                        }
                        None => self.multi_assign(dests, &call),
                    };
                    self.push(&line);
                }
                Inst::CallValueMulti {
                    dests,
                    callee,
                    args,
                } => {
                    let mut rendered = vec![self.render_val(*callee)?];
                    for a in args {
                        rendered.push(self.render_val(*a)?);
                    }
                    let call = format!("__rock_call({})", rendered.join(", "));
                    let line = self.multi_assign(dests, &call);
                    self.push(&line);
                }
                // Module-level `let`/`global` state write: `__G.<name> = value;`
                // (scoped so no body local can collide). The name must be in the
                // program's state set (`emit_with_closure` registers it before
                // emit); an unregistered write is a backend bug, not a skip.
                Inst::StoreGlobal { name, value } => {
                    let Some(slot) = self.globals.state.get(&symbol_of_key(*name)) else {
                        return Err(err(format!(
                            "JS subset: write to unregistered global `{name}`"
                        )));
                    };
                    self.globals.uses_storage.set(true);
                    let v = self.render_val(*value)?;
                    self.push(&format!("__rock_globals[{}] = {v};", js_string_lit(slot)?));
                }
                // A checked Vec/NativeBuffer index whose value is UNUSED: its
                // bounds check throws on OOB, so it must be emitted as a statement
                // rather than dropped (luajit keeps the checked read -- dropping
                // it would silently lose the OOB error, diverging under
                // `try_call(|| xs[oob])`). Map index is a raw miss-returns-nil VM
                // op with no side effect, so it stays droppable below.
                Inst::Index { kind, .. }
                    if (matches!(kind, ContainerKind::Vec) || kind.is_native_buffer())
                        && self.use_count[raw(iid) as usize] == 0 =>
                {
                    let line = format!("{};", self.render_inst_expr(iid)?);
                    self.push(&line);
                }
                // The shared element check of a column or strided read
                // likewise stays when every leaf read was dropped.
                Inst::VecCheckedIndex { .. } if self.use_count[raw(iid) as usize] == 0 => {
                    let line = format!("{};", self.render_inst_expr(iid)?);
                    self.push(&line);
                }
                // A used, non-inlinable value inst: materialize it at its
                // program position as `let v<raw> = <expr>` so cell reads capture
                // their value here and effectful calls keep their order. Its use
                // sites read the temp via `render_val`.
                other
                    if other.produces_value()
                        && !self.inlinable[raw(iid) as usize]
                        && self.use_count[raw(iid) as usize] > 0 =>
                {
                    let expr = self.render_inst_expr(iid)?;
                    // A cross-case temp is hoist-declared at the fn top (see
                    // `hoisted_temps`); assign, don't re-`let`, so it stays live
                    // across the flat switch cases. Block-local temps `let` here.
                    if self.hoisted_temps.contains(&raw(iid)) {
                        self.push(&format!("v{} = {};", self.mf.emit_inst_id(iid), expr));
                    } else {
                        self.push(&format!("let v{} = {};", self.mf.emit_inst_id(iid), expr));
                    }
                }
                Inst::ArrayPush {
                    arr,
                    value,
                    kind: ContainerKind::Vec,
                } => {
                    let a = self.render_val(*arr)?;
                    let v = self.render_val(*value)?;
                    self.push(&format!("{a}._data[{a}.len] = {v}; {a}.len = {a}.len + 1;"));
                }
                // An inlinable value inst renders at its use; an unused pure inst
                // (dead `Load`/`Index`/`Field`/`Len`) is dropped. Remaining
                // EFFECT insts (StoreField, StoreGlobal, column ArrayPush, MapNext,
                // CallMulti, coroutine ops, ...) are outside the subset.
                other if other.produces_value() => {}
                other => return Err(err(format!("JS subset: unsupported effect {other:?}"))),
            }
        }
        Ok(())
    }

    pub(super) fn emit_return(&mut self, vs: &[Idx<Inst>]) -> Result<(), CompilerError> {
        match vs {
            [] => self.push("return;"),
            [v] => {
                let s = self.render_val(*v)?;
                self.push(&format!("return {s};"));
            }
            // The scalar ABI returns a scalarized value struct / tuple as N leaf
            // values. JS has no multi-return, so leaf 0 rides the ordinary return
            // value and leaves 1..N ride module-scope scratch slots -- emulated
            // secondary return registers. An array would allocate, which a
            // `nobox` value forbids outright and every other value pays for
            // nothing (measured).
            //
            // Publishing before the `return` reorders the leaves' evaluation,
            // which is sound ONLY because a leaf is a temp or an
            // `always_inlinable` pure expression -- that set excludes every Call,
            // so no leaf can run user code or clobber the bank.
            _ => {
                let parts: Result<Vec<String>, CompilerError> =
                    vs.iter().map(|&v| self.render_val(v)).collect();
                let parts = parts?;
                for (i, p) in parts.iter().enumerate().skip(1) {
                    self.push(&format!("{} = {p};", ret_slot(i)));
                }
                self.push(&format!("return {};", parts[0]));
            }
        }
        Ok(())
    }

    /// A trampoline case's terminator: `__bb` reassignment + `continue` for
    /// in-region edges, `return` for exits. `Switch` is a literal if-chain (the
    /// discriminant is a temp or a pure inlinable, so re-rendering is safe).
    pub(super) fn emit_trampoline_term(&mut self, b: Idx<Block>) -> Result<(), CompilerError> {
        match self.mf.blocks[b].term.clone() {
            Term::Goto(t) => self.push(&format!("__bb = {}; continue;", self.mf.emit_block_id(t))),
            Term::Branch {
                cond,
                then_bb,
                else_bb,
            } => {
                let c = self.render_val(cond)?;
                self.push(&format!(
                    "if ({c}) {{ __bb = {}; }} else {{ __bb = {}; }} continue;",
                    self.mf.emit_block_id(then_bb),
                    self.mf.emit_block_id(else_bb)
                ));
            }
            Term::Switch {
                discr,
                cases,
                default,
            } => {
                let d = self.render_val(discr)?;
                let mut s = String::new();
                for (lit, bb) in &cases {
                    let cmp = match lit {
                        Lit::Nil => format!("{d} == null"),
                        Lit::Omitted => format!("{d} === {OMITTED_ARG}"),
                        Lit::Int(i) => format!("{d} === {i}"),
                        Lit::Uint(i) => format!("{d} === {i}"),
                        Lit::Bool(v) => format!("{d} === {v}"),
                        Lit::Float(f) => format!("{d} === {}", js_number(*f)?),
                        Lit::Str(v) => format!("{d} === {}", js_string_lit(v.as_str())?),
                    };
                    s.push_str(&format!(
                        "if ({cmp}) {{ __bb = {}; }} else ",
                        self.mf.emit_block_id(*bb)
                    ));
                }
                s.push_str(&format!(
                    "{{ __bb = {}; }} continue;",
                    self.mf.emit_block_id(default)
                ));
                self.push(&s);
            }
            Term::Return(vs) => match vs.as_slice() {
                [] => self.push("return;"),
                [v] => {
                    let s = self.render_val(*v)?;
                    self.push(&format!("return {s};"));
                }
                // Scalarized value-struct / tuple return: leaf 0 in the return
                // value, leaves 1..N in the scratch bank, mirroring the
                // structured `emit_return` -- the two printers MUST agree, or a
                // body that falls back to the trampoline silently changes
                // calling convention. Only the ordinary-body fallback opts in; a
                // coroutine step's return channels through the step protocol, so
                // it keeps the reject.
                _ if self.pack_multi_return => {
                    let parts: Result<Vec<String>, CompilerError> =
                        vs.iter().map(|&v| self.render_val(v)).collect();
                    let parts = parts?;
                    for (i, p) in parts.iter().enumerate().skip(1) {
                        self.push(&format!("{} = {p};", ret_slot(i)));
                    }
                    self.push(&format!("return {};", parts[0]));
                }
                _ => return Err(err("JS subset: multi-value coroutine return")),
            },
            Term::Unreachable => self.emit_unreachable(),
            Term::Yield { .. } => {
                return Err(err(
                    "trampoline: Term::Yield must not survive transform_coroutine (pass bug)",
                ));
            }
        }
        Ok(())
    }

    /// The JS expression for a value operand: the bound `v<raw>` temp if the
    /// inst was materialized at its position, else its expression folded inline.
    pub(super) fn checked_uint_shift(
        &self,
        value: Idx<Inst>,
        count: Idx<Inst>,
        op: &str,
    ) -> Result<String, CompilerError> {
        let value = self.render_val(value)?;
        let count = self.render_val(count)?;
        let shifted = format!("((value {op} count) >>> 0)");
        Ok(format!(
            "((value, count) => count <= 31 ? {shifted} : (() => {{ throw new Error('uint shift count out of range'); }})())({value}, {count})"
        ))
    }

    /// An operand that renders as a temp or a literal, so repeating its text
    /// neither re-evaluates nor grows the expression.
    pub(super) fn is_plain_operand(&self, iid: Idx<Inst>) -> bool {
        !self.inlinable[raw(iid) as usize] || matches!(self.mf.insts[iid], Inst::Const(_))
    }

    pub(super) fn render_val(&self, iid: Idx<Inst>) -> Result<String, CompilerError> {
        if self.inlinable[raw(iid) as usize] {
            self.render_inst_expr(iid)
        } else {
            Ok(format!("v{}", self.mf.emit_inst_id(iid)))
        }
    }
}
