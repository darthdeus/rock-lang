//! Block statements, returns and trampoline terminators.

use super::*;

/// A value inst that still runs as a statement when unused: calls, bounds-checking or host-getter
/// indexes, column/strided element checks and range-checked uint ops.
pub(super) fn evaluated_when_unused(inst: &Inst) -> bool {
    match inst {
        Inst::Index { kind, .. } => {
            matches!(
                kind,
                ContainerKind::Vec
                    | ContainerKind::VecInBounds
                    | ContainerKind::VecNonNull
                    | ContainerKind::VecNonNullInBounds
                    | ContainerKind::Map
                    | ContainerKind::Record
            ) || kind.is_native_buffer()
        }
        _ => matches!(
            inst,
            Inst::Call { .. }
                | Inst::CallValue { .. }
                | Inst::VecCheckedIndex { .. }
                | Inst::UintShl(..)
                | Inst::UintShr(..)
                | Inst::IntToUint(_)
                | Inst::FloatToUint(_)
        ),
    }
}

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
                        ContainerKind::NativeBufferInBounds => format!("{b}.ptr[{i}] = {v};"),
                        // Column storage has no JS arm: reject, since one `value` operand cannot
                        // carry K leaves (lowering splits element writes into per-column stores).
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
                        // The element index was bounds-checked once ahead of every leaf write; a JS
                        // boolean stores 1/0 in a Uint8Array already.
                        ContainerKind::NativeBufferLeaf => format!("{b}[{i}] = {v};"),
                        ContainerKind::NativeBufferValue(elem) => {
                            return Err(err(format!(
                                "JS subset: whole-element store into `NativeBuffer<{elem}>` \
                                 reached emit -- `{elem}` has no C leaf layout"
                            )));
                        }
                        ContainerKind::Vec | ContainerKind::VecNonNull => {
                            format!("__vec_checked_set({b}, {i}, {v});")
                        }
                        ContainerKind::VecInBounds | ContainerKind::VecNonNullInBounds => {
                            format!("{b}._data[{i}] = {v};")
                        }
                        ContainerKind::Map => format!("__map_set({b}, {i}, {v});"),
                        ContainerKind::Record => {
                            if let Inst::Const(Lit::Str(field)) = &self.mf.insts[*index] {
                                format!("{} = {v};", js_member(&b, field.as_str()))
                            } else {
                                format!("{b}[{i}] = {v};")
                            }
                        }
                    };
                    self.push(&line);
                }
                // One Map iteration step through the cursor-cached `__map_next` shim (insertion
                // order, null at exhaustion); `table` renders idempotently, so re-rendering it is
                // safe.
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
                Inst::StoreField { base, field, value } => {
                    let line = format!(
                        "{} = {};",
                        js_member(&self.render_val(*base)?, field),
                        self.render_val(*value)?
                    );
                    self.push(&line);
                }
                other if self.use_count[raw(iid) as usize] == 0 && evaluated_when_unused(other) => {
                    let line = format!("{};", self.render_inst_expr(iid)?);
                    self.push(&line);
                }
                Inst::CallMulti { dests, func, args } => {
                    let call = self.render_call(iid, *func, args)?;
                    // An intrinsic is one JS expression: lanes of a multi-lane result are read off
                    // that value, since no intrinsic writes the return bank.
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
                    let call = self.indirect_call(*callee, args)?;
                    let line = self.multi_assign(dests, &call);
                    self.push(&line);
                }
                // An unregistered state-global write is a backend bug, not a skip.
                Inst::StoreGlobal { name, value } => {
                    let Some(slot) = self.globals.state.get(&symbol_of_key(*name)) else {
                        return Err(err(format!(
                            "JS subset: write to unregistered global `{name}`"
                        )));
                    };
                    self.globals.uses_storage.set(true);
                    let v = self.render_val(*value)?;
                    self.push(&format!("__rock_globals[{}] = {v};", js_string_lit(slot)));
                }
                other
                    if other.produces_value()
                        && !self.inlinable[raw(iid) as usize]
                        && self.use_count[raw(iid) as usize] > 0 =>
                {
                    let expr = self.render_inst_expr(iid)?;
                    // A cross-case temp is hoist-declared: assign, don't re-`let`.
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
            // Publish secondary lanes first: each operand is a temp or folded expression that
            // cannot overwrite the bank.
            _ => self.emit_multi_return(vs)?,
        }
        Ok(())
    }

    fn emit_multi_return(&mut self, vs: &[Idx<Inst>]) -> Result<(), CompilerError> {
        let parts = vs
            .iter()
            .map(|&v| self.render_val(v))
            .collect::<Result<Vec<_>, _>>()?;
        for (i, p) in parts.iter().enumerate().skip(1) {
            self.push(&format!("{} = {p};", ret_slot(i)));
        }
        self.push(&format!("return {};", parts[0]));
        Ok(())
    }

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
            Term::Return(vs) => {
                if vs.len() > 1 && !self.pack_multi_return {
                    return Err(err("JS subset: multi-value coroutine return"));
                }
                self.emit_return(&vs)?;
            }
            Term::Unreachable => self.emit_unreachable(),
            Term::Yield { .. } => {
                return Err(err(
                    "trampoline: Term::Yield must not survive transform_coroutine (pass bug)",
                ));
            }
        }
        Ok(())
    }

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
