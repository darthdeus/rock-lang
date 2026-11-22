use super::*;

impl Emitter<'_> {
    /// Whether an operand can only produce a non-nil value, so a store of it
    /// needs no nil-write guard. For plain Rock tables a wrong answer costs a
    /// rehash; host metatables can observe the skipped store (docs/compiler.md).
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

    fn effect_allocation(&self, inst: &Inst) -> Option<EmittedAllocationKind> {
        self.allocation?;
        let grows = match inst {
            Inst::ArrayPush { .. }
            | Inst::StoreIndex {
                kind:
                    ContainerKind::Map
                    | ContainerKind::Vec
                    | ContainerKind::VecInBounds
                    | ContainerKind::VecColumn,
                ..
            } => true,
            Inst::StoreIndex {
                base,
                index,
                kind: ContainerKind::Record,
                ..
            } => match self.func.insts[*index] {
                Inst::Const(Lit::Str(field)) => {
                    !cell_never_nil(self.ws, self.field_type(*base, field))
                }
                _ => true,
            },
            Inst::StoreField { base, field, value } => {
                // A guarded literal nil store only overwrites an existing slot.
                let guarded_clear = matches!(self.func.insts[*value], Inst::Const(Lit::Nil))
                    && self.opnd_is_plain(*base);
                !guarded_clear && !cell_never_nil(self.ws, self.field_type(*base, *field))
            }
            _ => false,
        };
        grows.then_some(EmittedAllocationKind::ContainerGrowth)
    }

    fn field_type(&self, base: Idx<Inst>, field: ustr::Ustr) -> Option<bone_core::prelude::TyId> {
        use bone_core::hir::{TypeCtor, TypeExpr};
        use bone_core::prelude::TyId;

        let ty = match self.func.insts[base] {
            Inst::Load(cell) => self.func.cells[cell]
                .ty
                .filter(|ty| matches!(ty, TyId::Static(..)))
                .or_else(|| {
                    if !self.func.params.contains(&cell) {
                        return None;
                    }
                    let name = self.func.cells[cell].name?;
                    let owner = self.func.abi_plan.as_ref()?.subject.func;
                    let f = func_of(self.ws, owner);
                    f.param_decls.iter().find(|p| p.name == name).map(|p| p.ty)
                }),
            Inst::Global(name) => self.ws.globals_by_name.get(&name).copied(),
            _ => self
                .func
                .field_projection(base)
                .and_then(|(base, field)| self.field_type(base, field)),
        };
        let mut ty = ty?;
        let mut parts = field.as_str().split('.').peekable();
        loop {
            let (resolved, node) = self.ws.resolve_ty(None, ty)?;
            match node {
                TypeExpr::App {
                    ctor: TypeCtor::Ref,
                    params,
                } => ty = resolved.with_idx(params.into_iter().next()?),
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    ..
                } => {
                    let owner = self.ws.nominals.struct_id(name)?;
                    let field = parts.next()?;
                    ty = self.ws.module(owner.module).structs[owner.def]
                        .fields
                        .iter()
                        .find(|f| f.name.as_str() == field)?
                        .storage_ty;
                    if parts.peek().is_none() {
                        return Some(ty);
                    }
                }
                _ => return None,
            }
        }
    }

    pub(crate) fn write_effect(&self, out: &mut String, id: Idx<Inst>, inst: &Inst) {
        if let Some(site) = self.record_allocation_site(id, self.effect_allocation(inst), out.len())
        {
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
                // Two cells that share a local make a copy between them a self-assignment.
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
                self.write_assignment_rhs(out, *value);
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
                // StoreField targets compiler-owned tables without host metamethods.
                self.write_field_assignment(out, *base, field.as_str(), *value, false);
            }
            Inst::StoreIndex {
                base,
                index,
                value,
                kind,
            } => {
                if matches!(kind, ContainerKind::VecStrideSlot) {
                    self.pc_source(out.len(), id, "container-growth");
                }
                let checked_set = match kind {
                    ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_) => {
                        Some("__nb_checked_set")
                    }
                    ContainerKind::Vec | ContainerKind::VecNonNull => Some("__vec_checked_set"),
                    // One `value` operand cannot carry K leaves; lowering splits
                    // an element write into per-column stores.
                    ContainerKind::VecValueColumns(elem) => {
                        unreachable!("whole-element store into column-stored [{elem}] reached emit")
                    }
                    ContainerKind::VecValueStrided(elem) => {
                        unreachable!("whole-element store into strided [{elem}] reached emit")
                    }
                    _ => None,
                };
                if let Some(setter) = checked_set {
                    out.push(' ');
                    self.write_builtin_call(out, setter, &[*base, *index, *value]);
                    out.push('\n');
                } else if matches!(kind, ContainerKind::Record)
                    && let Inst::Const(Lit::Str(field)) = &self.func.insts[*index]
                {
                    self.write_field_assignment(out, *base, field.as_str(), *value, true);
                } else {
                    out.push(' ');
                    self.write_index_place(out, *base, *index, *kind);
                    self.write_assignment_rhs(out, *value);
                }
            }
            Inst::StoreGlobal { name, value } => {
                out.push(' ');
                self.write_global(out, *name);
                self.write_assignment_rhs(out, *value);
            }
            Inst::ArrayPush { arr, value, kind } => {
                // `arr` is spelled up to four times; render it once.
                let a = self.opnd_str(*arr);
                match kind {
                    ContainerKind::Vec | ContainerKind::VecNonNull => {
                        // Increment len before the write: the data table is 1-indexed.
                        let _ = writeln!(out, " {a}[0] = {a}[0] + 1");
                        let _ = write!(out, " {a}[{a}[0]]");
                        self.write_assignment_rhs(out, *value);
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
            Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. } => {
                self.write_cells_assignment(out, dests, |out| self.write_call(out, id));
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

    fn write_field_assignment(
        &self,
        out: &mut String,
        base: Idx<Inst>,
        field: &str,
        value: Idx<Inst>,
        check_metatable: bool,
    ) {
        // Compiler-owned plain fields cannot invoke host callbacks.
        // Avoid LuaJIT rehashing an absent key for a no-op nil store.
        let guard = self.opnd_is_plain(base) && self.opnd_is_plain(value) && !self.never_nil(value);
        let write_target = |out: &mut String| {
            self.write_opnd(out, base);
            write_field_key(out, field);
        };
        if guard {
            out.push_str(" if ");
            self.write_non_nil_disjunct(out, value);
            if check_metatable {
                // Test the metatable before reading the field: a
                // getter must not run merely because we store nil.
                self.write_builtin_call(out, "getmetatable", &[base]);
                out.push_str(" ~= nil or ");
            }
            write_target(out);
            out.push_str(" ~= nil then\n");
        }
        out.push(' ');
        write_target(out);
        self.write_assignment_rhs(out, value);
        if guard {
            out.push_str(" end\n");
        }
    }

    pub(crate) fn write_assignment_rhs(&self, out: &mut String, value: Idx<Inst>) {
        out.push_str(" = ");
        self.write_opnd(out, value);
        out.push('\n');
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
                ws.nominals.struct_id(*name).is_some() || ws.nominals.enum_id(*name).is_some()
            }
        },
        TypeExpr::Function { .. } | TypeExpr::Tuple { .. } => true,
        TypeExpr::Generic(..) | TypeExpr::Infer(..) | TypeExpr::Nullable(_) => false,
    }
}
