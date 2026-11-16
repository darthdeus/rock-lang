//! Effect statements.

use super::*;

impl Emitter<'_> {
    /// Whether an operand can only produce a non-nil value, so a store of it
    /// needs no nil-write guard. Lowering marks the cells it can prove
    /// ([`Cell::never_nil`]); this adds what the emitted shape decides and
    /// what a declared type promises. For plain Rock tables, a wrong
    /// answer costs the rehash the guard avoids, never a wrong value. Host
    /// metatables can observe the skipped store; see docs/compiler.md.
    pub(crate) fn never_nil(&self, id: Idx<Inst>) -> bool {
        skull_mir::nilness::inst_never_nil_with(self.func, id, &|leaf| match &self.func.insts[leaf]
        {
            Inst::Load(cell) => self.cell_never_nil(*cell),
            Inst::Call { func, .. } => {
                cell_never_nil(self.ws, crate::func_of(self.ws, *func).return_ty)
            }
            _ => false,
        })
    }

    fn cell_never_nil(&self, cell: Idx<Cell>) -> bool {
        let c = &self.func.cells[cell];
        c.never_nil || cell_never_nil(self.ws, c.ty)
    }

    pub(crate) fn write_effect(&self, out: &mut String, id: Idx<Inst>, inst: &Inst) {
        if let Some(site) = self.record_effect_allocation_site(id, inst, out.len()) {
            out.push_str(" __rock_alloc_hit_effect(\"");
            write!(out, "{:016x}", site.id).unwrap();
            out.push_str("\")\n");
        }
        match inst {
            Inst::Store { cell, value } => {
                if self.cell_is_frame_field(*cell) {
                    let target = cellname(self.func, self.frame, self.cell_colors, *cell);
                    self.write_single_frame_assignment(out, &target, *value);
                    return;
                }
                // Two cells that share a local make a copy between them a
                // self-assignment. (Frame fields were handled above.)
                if let Inst::Load(source) = self.func.insts[*value]
                    && self.inlinable[raw(*value) as usize]
                    && !self.cell_is_frame_field(source)
                    && (source == *cell || {
                        let color = self.cell_colors[raw(*cell) as usize];
                        color != u32::MAX && color == self.cell_colors[raw(source) as usize]
                    })
                {
                    return;
                }
                if regalloc::alias_at(self.alias, *value) == Some(*cell) {
                    return;
                }
                out.push(' ');
                scope_guard::assigning(self.func);
                write_cellname(out, self.func, self.frame, self.cell_colors, *cell);
                out.push_str(" = ");
                self.write_opnd(out, *value);
                out.push('\n');
            }
            Inst::StoreField { base, field, value } => {
                if let Some(frame) = self.frame {
                    if matches!(self.func.insts[*base], Inst::Load(cell) if cell == frame.s_cell) {
                        let mut target = cellname(self.func, None, self.cell_colors, frame.s_cell);
                        write_field_key(&mut target, field.as_str());
                        self.write_single_frame_assignment(out, &target, *value);
                        return;
                    }
                }
                // Compiler-owned plain fields cannot invoke host callbacks.
                // Avoid LuaJIT rehashing an absent key for a no-op nil store.
                let guard = self.opnd_is_plain(*base)
                    && self.opnd_is_plain(*value)
                    && !self.never_nil(*value);
                if guard {
                    out.push_str(" if ");
                    // A store of literal nil skips its statically-false
                    // `nil ~= nil` disjunct; the field test still decides.
                    if !matches!(self.func.insts[*value], Inst::Const(Lit::Nil)) {
                        self.write_opnd(out, *value);
                        out.push_str(" ~= nil or ");
                    }
                    self.write_opnd(out, *base);
                    write_field_key(out, field.as_str());
                    out.push_str(" ~= nil then\n");
                }
                out.push(' ');
                self.write_opnd(out, *base);
                write_field_key(out, field.as_str());
                out.push_str(" = ");
                self.write_opnd(out, *value);
                out.push('\n');
                if guard {
                    out.push_str(" end\n");
                }
            }
            Inst::StoreIndex {
                base,
                index,
                value,
                kind,
            } => {
                match kind {
                    // NativeBuffer write -> debug-mode bounds-checked helper.
                    ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_) => {
                        out.push_str(" __nb_checked_set(");
                        self.write_opnd(out, *base);
                        out.push_str(", ");
                        self.write_opnd(out, *index);
                        out.push_str(", ");
                        self.write_opnd(out, *value);
                        out.push_str(")\n");
                    }
                    ContainerKind::Vec | ContainerKind::VecNonNull => {
                        out.push_str(" __vec_checked_set(");
                        self.write_opnd(out, *base);
                        out.push_str(", ");
                        self.write_opnd(out, *index);
                        out.push_str(", ");
                        self.write_opnd(out, *value);
                        out.push_str(")\n");
                    }
                    // The index was proven within `[0, len)`: a raw C array write.
                    ContainerKind::NativeBufferInBounds => {
                        out.push(' ');
                        self.write_opnd(out, *base);
                        out.push_str(".ptr[");
                        self.write_opnd(out, *index);
                        out.push_str("] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                    // The index was proven within `[0, len)`: a raw slot write.
                    ContainerKind::VecInBounds | ContainerKind::VecNonNullInBounds => {
                        out.push(' ');
                        self.write_opnd(out, *base);
                        out.push_str("._data[");
                        self.write_vec_slot(out, *index);
                        out.push_str("] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                    // Raw keyed writes, leaf columns, and already checked slots.
                    ContainerKind::Map
                    | ContainerKind::VecColumn
                    | ContainerKind::VecStrideSlot
                    | ContainerKind::NativeBufferLeaf => {
                        out.push(' ');
                        self.write_opnd(out, *base);
                        out.push('[');
                        self.write_opnd(out, *index);
                        out.push_str("] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                    ContainerKind::Record => {
                        let field = match &self.func.insts[*index] {
                            Inst::Const(Lit::Str(field)) => Some(*field),
                            _ => None,
                        };
                        let guard = field.is_some()
                            && self.opnd_is_plain(*base)
                            && self.opnd_is_plain(*value)
                            && !self.never_nil(*value);
                        if guard {
                            out.push_str(" if ");
                            if !matches!(self.func.insts[*value], Inst::Const(Lit::Nil)) {
                                self.write_opnd(out, *value);
                                out.push_str(" ~= nil or ");
                            }
                            // Test the metatable before reading the field: a
                            // getter must not run merely because we store nil.
                            out.push_str("getmetatable(");
                            self.write_opnd(out, *base);
                            out.push_str(") ~= nil or ");
                            self.write_opnd(out, *base);
                            write_field_key(out, field.unwrap().as_str());
                            out.push_str(" ~= nil then\n");
                        }
                        out.push(' ');
                        self.write_opnd(out, *base);
                        if let Some(field) = field {
                            write_field_key(out, field.as_str());
                        } else {
                            out.push('[');
                            self.write_opnd(out, *index);
                            out.push(']');
                        }
                        out.push_str(" = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                        if guard {
                            out.push_str(" end\n");
                        }
                    }
                    // One `value` operand cannot carry K leaves; lowering splits
                    // an element write into per-column stores.
                    ContainerKind::VecValueColumns(elem) => {
                        unreachable!("whole-element store into column-stored [{elem}] reached emit")
                    }
                    ContainerKind::VecValueStrided(elem) => {
                        unreachable!("whole-element store into strided [{elem}] reached emit")
                    }
                }
            }
            Inst::StoreGlobal { name, value } => {
                if self.has_state_slot(*name) {
                    out.push(' ');
                    out.push_str(self.ctx.state_table());
                    out.push('[');
                    self.write_state_slot(out, *name);
                    out.push_str("] = ");
                    self.write_opnd(out, *value);
                    out.push('\n');
                } else {
                    let _ = write!(
                        out,
                        " {} = ",
                        self.bound_name(skull_mir::mangle::symbol_of_key(*name))
                    );
                    self.write_opnd(out, *value);
                    out.push('\n');
                }
            }
            Inst::ArrayPush { arr, value, kind } => {
                // `arr` is referenced up to 4x; render it once.
                let a = self.opnd_str(*arr);
                match kind {
                    ContainerKind::Vec | ContainerKind::VecNonNull => {
                        // increment len BEFORE assign (1-indexed data table).
                        let _ = writeln!(out, " {a}.len = {a}.len + 1");
                        let _ = write!(out, " {a}._data[{a}.len] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                    // Column push bumps the shared `len` once, then writes K
                    // columns -- more than one `value`, so lowering splits it.
                    ContainerKind::VecInBounds
                    | ContainerKind::VecNonNullInBounds
                    | ContainerKind::NativeBuffer
                    | ContainerKind::NativeBufferInBounds
                    | ContainerKind::NativeBufferValue(_)
                    | ContainerKind::NativeBufferLeaf
                    | ContainerKind::VecValueColumns(_)
                    | ContainerKind::VecValueStrided(_)
                    | ContainerKind::VecColumn
                    | ContainerKind::VecStrideSlot
                    | ContainerKind::Map
                    | ContainerKind::Record => {
                        unreachable!("ArrayPush only lowers for Vec array literals, got {kind:?}")
                    }
                }
            }
            Inst::MapNext {
                key_cell,
                val_cell,
                table,
            } => {
                self.write_cells_assignment(out, &[*key_cell, *val_cell], |out| {
                    out.push_str("next(");
                    self.write_opnd(out, *table);
                    out.push_str(", ");
                    write_cellname(out, self.func, self.frame, self.cell_colors, *key_cell);
                    out.push(')');
                });
            }
            // Scalar-ABI multi-return: `d0, d1, ... = callee(args)`.
            Inst::CallMulti { dests, func, args } => {
                self.write_cells_assignment(out, dests, |out| {
                    write!(out, "{}(", self.callee_name(id, *func)).unwrap();
                    self.write_csv(out, self.call_args(*func, args));
                    out.push(')');
                });
            }
            // Multi-assign keeps all results returned through a function value.
            Inst::CallValueMulti {
                dests,
                callee,
                args,
            } => {
                self.write_cells_assignment(out, dests, |out| {
                    self.write_opnd(out, *callee);
                    out.push('(');
                    self.write_csv(out, args);
                    out.push(')');
                });
            }
            // Rewritten to `s.__inner = ..` by the coroutine transform.
            Inst::CoroSetInner(_) => {
                self.reject_pass_residue("await bookkeeping", "coroutine transform")
            }
            // Relocated to the constructor by the coroutine transform.
            Inst::CoroHook { .. } => {
                self.reject_pass_residue("a coroutine hook", "coroutine transform")
            }
            Inst::CallBoundMulti { .. } => {
                self.reject_pass_residue("a placeholder bound call", "residual-bound-call gate")
            }
            _ => unreachable!("not an effect"),
        }
    }
}

/// A source binding declared with a non-nullable builtin type never holds nil
/// where it is read.
fn cell_never_nil(ws: &Workspace, ty: Option<bone_core::prelude::TyId>) -> bool {
    use bone_core::hir::{BuiltinTy, TypeCtor, TypeExpr};
    let Some(bone_core::prelude::TyId::Static(module, index)) = ty else {
        return false;
    };
    match &ws.module(module).ty_arena[index] {
        TypeExpr::Builtin(builtin) => matches!(
            builtin,
            BuiltinTy::Int
                | BuiltinTy::Uint
                | BuiltinTy::Float
                | BuiltinTy::Bool
                | BuiltinTy::String
        ),
        // Tables, closures, cdata and enum ints. A newtype is left out: it
        // carries whatever its underlying type does.
        TypeExpr::App { ctor, .. } => match ctor {
            TypeCtor::Array | TypeCtor::Map | TypeCtor::Ref => true,
            TypeCtor::Nominal(name) => {
                ws.struct_by_name.contains_key(name) || ws.enums_by_name.contains_key(name)
            }
        },
        TypeExpr::Function { .. } | TypeExpr::Tuple { .. } => true,
        TypeExpr::Generic(..) | TypeExpr::Infer(..) | TypeExpr::Nullable(_) => false,
    }
}
