//! Effect statements.

use super::*;

impl Emitter<'_> {
    /// Whether an operand's own instruction can only produce a non-nil value,
    /// so a store of it needs no nil-write guard.
    pub(crate) fn never_nil(&self, id: Idx<Inst>) -> bool {
        match &self.func.insts[id] {
            Inst::Const(lit) => !matches!(lit, Lit::Nil),
            Inst::Load(cell) => cell_never_nil(self.ws, self.func.cells[*cell].ty),
            // A callee declared to return a non-optional scalar returns one; an
            // extern that breaks its declaration only costs the rehash guard.
            Inst::Call { func, .. } => {
                cell_never_nil(self.ws, crate::func_of(self.ws, *func).return_ty)
            }
            Inst::IntAdd(..)
            | Inst::IntSub(..)
            | Inst::IntMul(..)
            | Inst::IntDiv(..)
            | Inst::IntMod(..)
            | Inst::UintAdd(..)
            | Inst::UintSub(..)
            | Inst::UintMul(..)
            | Inst::UintDiv(..)
            | Inst::UintMod(..)
            | Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::FloatMod(..)
            | Inst::StringConcat(..)
            | Inst::ToString(..)
            | Inst::Format { .. }
            | Inst::FormatDyn { .. }
            | Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::UintLt(..)
            | Inst::UintLeq(..)
            | Inst::UintGt(..)
            | Inst::UintGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::Neg(..)
            | Inst::Not(..)
            | Inst::BitNot(..)
            | Inst::UintBitNot(..)
            | Inst::BitAnd(..)
            | Inst::BitOr(..)
            | Inst::BitXor(..)
            | Inst::Shl(..)
            | Inst::Shr(..)
            | Inst::UintBitAnd(..)
            | Inst::UintBitOr(..)
            | Inst::UintBitXor(..)
            | Inst::UintShl(..)
            | Inst::UintShr(..)
            | Inst::Len { .. }
            | Inst::MakeTuple(..)
            | Inst::MakeVec(..)
            | Inst::MakeStruct { .. }
            | Inst::Lambda { .. } => true,
            _ => false,
        }
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
                    let guard = !self.never_nil(*value);
                    self.write_frame_assignment(out, &[(target, guard)], |out| {
                        self.write_opnd(out, *value)
                    });
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
                if regalloc::alias_at(&self.alias, *value) == Some(*cell) {
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
                        let guard = !self.never_nil(*value);
                        self.write_frame_assignment(out, &[(target, guard)], |out| {
                            self.write_opnd(out, *value)
                        });
                        return;
                    }
                }
                // LuaJIT can rehash when assigning nil to an absent key.
                // Metatable writes remain observable; plain operands evaluate once.
                let guard = self.opnd_is_plain(*base)
                    && self.opnd_is_plain(*value)
                    && !self.never_nil(*value);
                if guard {
                    out.push_str(" if ");
                    self.write_opnd(out, *value);
                    out.push_str(" ~= nil or getmetatable(");
                    self.write_opnd(out, *base);
                    out.push_str(") ~= nil or ");
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
                    ContainerKind::Vec => {
                        out.push_str(" __vec_checked_set(");
                        self.write_opnd(out, *base);
                        out.push_str(", ");
                        self.write_opnd(out, *index);
                        out.push_str(", ");
                        self.write_opnd(out, *value);
                        out.push_str(")\n");
                    }
                    // Raw 1-based write into one leaf column; the owning Vec's
                    // `len` was checked once by `VecCheckedIndex`.
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
                    ContainerKind::Vec => {
                        // increment len BEFORE assign (1-indexed data table).
                        let _ = writeln!(out, " {a}.len = {a}.len + 1");
                        let _ = write!(out, " {a}._data[{a}.len] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                    // Column push bumps the shared `len` once, then writes K
                    // columns -- more than one `value`, so lowering splits it.
                    ContainerKind::NativeBuffer
                    | ContainerKind::NativeBufferValue(_)
                    | ContainerKind::NativeBufferLeaf
                    | ContainerKind::VecValueColumns(_)
                    | ContainerKind::VecValueStrided(_)
                    | ContainerKind::VecColumn
                    | ContainerKind::VecStrideSlot
                    | ContainerKind::Map => {
                        unreachable!("ArrayPush only lowers for Vec array literals, got {kind:?}")
                    }
                }
            }
            Inst::MapNext {
                key_cell,
                val_cell,
                table,
            } => {
                if self.cell_is_frame_field(*key_cell) || self.cell_is_frame_field(*val_cell) {
                    let targets: Vec<_> = [*key_cell, *val_cell]
                        .iter()
                        .map(|c| {
                            (
                                cellname(self.func, self.frame, self.cell_colors, *c),
                                self.cell_is_frame_field(*c),
                            )
                        })
                        .collect();
                    self.write_frame_assignment(out, &targets, |out| {
                        out.push_str("next(");
                        self.write_opnd(out, *table);
                        write!(out, ", {})", targets[0].0).unwrap();
                    });
                    return;
                }
                // `key_cell` is referenced twice; render it once.
                let k = cellname(self.func, self.frame, self.cell_colors, *key_cell);
                let _ = write!(out, " {k}, ");
                write_cellname(out, self.func, self.frame, self.cell_colors, *val_cell);
                let _ = write!(out, " = next(");
                self.write_opnd(out, *table);
                let _ = writeln!(out, ", {k})");
            }
            // Scalar-ABI multi-return: `d0, d1, ... = callee(args)`. The callee is
            // a value-returning fn whose return is scalarized to N leaf values.
            Inst::CallMulti { dests, func, args } => {
                if dests.iter().any(|c| self.cell_is_frame_field(*c)) {
                    let targets: Vec<_> = dests
                        .iter()
                        .map(|c| {
                            (
                                cellname(self.func, self.frame, self.cell_colors, *c),
                                self.cell_is_frame_field(*c),
                            )
                        })
                        .collect();
                    self.write_frame_assignment(out, &targets, |out| {
                        write!(out, "{}(", self.callee_name(id, *func)).unwrap();
                        self.write_csv(out, self.call_args(*func, args));
                        out.push(')');
                    });
                    return;
                }
                out.push(' ');
                for (i, c) in dests.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_cellname(out, self.func, self.frame, self.cell_colors, *c);
                }
                let callee = self.callee_name(id, *func);
                if !dests.is_empty() {
                    out.push_str(" = ");
                }
                let _ = write!(out, "{callee}(");
                self.write_csv(out, self.call_args(*func, args));
                out.push_str(")\n");
            }
            // Indirect multi-return: `d0, d1, ... = callee(args)` -- a tuple
            // returned through a fn-value. Multi-assign so Lua doesn't truncate.
            Inst::CallValueMulti {
                dests,
                callee,
                args,
            } => {
                if dests.iter().any(|c| self.cell_is_frame_field(*c)) {
                    let targets: Vec<_> = dests
                        .iter()
                        .map(|c| {
                            (
                                cellname(self.func, self.frame, self.cell_colors, *c),
                                self.cell_is_frame_field(*c),
                            )
                        })
                        .collect();
                    self.write_frame_assignment(out, &targets, |out| {
                        self.write_opnd(out, *callee);
                        out.push('(');
                        self.write_csv(out, args);
                        out.push(')');
                    });
                    return;
                }
                out.push(' ');
                for (i, c) in dests.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_cellname(out, self.func, self.frame, self.cell_colors, *c);
                }
                if !dests.is_empty() {
                    out.push_str(" = ");
                }
                self.write_opnd(out, *callee);
                out.push('(');
                self.write_csv(out, args);
                out.push_str(")\n");
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
