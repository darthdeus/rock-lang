use super::*;
impl<'a> Builder<'a> {
    pub(crate) fn new_flat_enum_cells(
        &mut self,
        binding: Idx<BindingInfo>,
        name: Ustr,
        shape: &ValueShape,
        owner: Option<&MonoTy>,
    ) -> Result<Option<Vec<Idx<Cell>>>, CompilerError> {
        let Some(flat) =
            EnumPlan::for_name(self.ws, shape.name).and_then(|plan| plan.flat().cloned())
        else {
            return Ok(None);
        };
        if flat.name != shape.name || shape.leaf_count() != flat.leaf_count() {
            return Err(self.gap_here(format!(
                "flat enum {} has a cell shape that differs from its compiler-owned layout",
                shape.name
            )));
        }
        let owner = owner
            .map(|owner| match owner {
                MonoTy::Nullable(inner) => inner.as_ref(),
                owner => owner,
            })
            .filter(|owner| {
                matches!(
                    owner,
                    MonoTy::App {
                        ctor: MonoCtor::Nominal(name),
                        ..
                    } if *name == flat.name
                )
            })
            .ok_or_else(|| {
                self.gap_here(format!(
                    "flat enum {} has no concrete owner type for native slots",
                    flat.name
                ))
            })?;
        let owner = self.scx.interner().intern(owner.clone());
        let tag_ty = self
            .scx
            .native_flat_enums
            .then(|| self.scx.interner().intern(MonoTy::String));
        let tuple_width = flat.tuple_width();
        let mut native_slot_fields = vec![None; tuple_width];
        let mut offset = 0;
        for slot in &flat.slots {
            if self.scx.native_flat_enums || matches!(slot, FlatSlot::Leaves(_)) {
                for (relative, field) in native_slot_fields[offset..offset + slot.width()]
                    .iter_mut()
                    .enumerate()
                {
                    let index = offset + relative;
                    *field = Some(crate::names::indexed("", "", index));
                }
            }
            offset += slot.width();
        }
        let cells = (0..flat.leaf_count())
            .map(|index| {
                let cell_name = crate::names::indexed(name.as_str(), "__", index);
                let projection = crate::names::indexed("", "", index);
                let cell = self.new_binding_cell(binding, cell_name, None, projection);
                if index == 0 {
                    self.f.cells[cell].native_ty = tag_ty;
                    return cell;
                }
                let slot = index - 1;
                if let Some(field) = native_slot_fields.get(slot).and_then(|field| *field) {
                    self.f.cells[cell].native_flat_slot =
                        Some(crate::mir::NativeFlatSlot { owner, field });
                }
                cell
            })
            .collect();
        Ok(Some(cells))
    }

    pub(crate) fn lower_flat_value(
        &mut self,
        shape: &EnumFlatShape,
        value: Idx<Expr>,
    ) -> Result<FlatCells, CompilerError> {
        if let Expr::EnumVariant {
            enum_name,
            variant,
            args,
            ..
        } = self.body.expr_arena[value]
        {
            if enum_name == shape.name {
                let (positional, rec_args) = match args {
                    VariantArgs::Unit => (Vec::new(), Vec::new()),
                    VariantArgs::Tuple(range) => (
                        range.into_iter().map(FlatPayload::Expr).collect(),
                        Vec::new(),
                    ),
                    VariantArgs::Record(fields) => {
                        let rec = fields
                            .into_iter()
                            .map(|fidx| {
                                let lf = self.body.struct_lit_field_arena[fidx];
                                (lf.name, lf.value)
                            })
                            .collect();
                        (Vec::new(), rec)
                    }
                };
                return self.flat_ctor_cells(shape, variant, positional, rec_args);
            }
        }
        let r = self.lower_expr_repr(value)?.into_value();
        match r {
            ValueRepr::Group { name, parts, .. }
                if EnumPlan::for_name(self.ws, name).is_some_and(|plan| plan.flat().is_some()) =>
            {
                Ok(self.repr_group_to_cells(shape, parts)?)
            }
            other => {
                let boxed = self.pack_maybe_absent(other)?;
                self.spread_maybe_null_box_to_cells(boxed, shape)
            }
        }
    }

    pub(crate) fn flat_ctor_cells(
        &mut self,
        shape: &EnumFlatShape,
        variant: Ustr,
        positional: Vec<FlatPayload>,
        rec_args: Vec<(Ustr, Idx<Expr>)>,
    ) -> Result<FlatCells, CompilerError> {
        let tag = self.emit_cur(Inst::Const(Lit::Str(variant)));
        let mut slots: Vec<Option<Idx<Inst>>> = vec![None; shape.tuple_width()];
        for (pos, arg) in positional.into_iter().enumerate() {
            let off = shape.slot_offset(pos);
            // Use this variant's payload width and leave the remaining slots inactive.
            match self.flat_payload_leaf_shape(Some(shape.name), variant, pos) {
                Some(vs) => {
                    // The enum is immutable and outlives its operands, so its payload owns its
                    // value children from the start.
                    let leaves = match arg {
                        FlatPayload::Expr(a) => {
                            let r = self.lower_expr_repr(a)?;
                            let r = ValueBundle::new(
                                r.representation(),
                                self.owned_value_children(r.into_value())?,
                            );
                            self.repr_to_storage_leaves(&r, &vs, "flat enum payload")?
                        }
                        FlatPayload::Repr(r) => {
                            let r = self.owned_value_children(r)?;
                            self.repr_to_leaves_shape(&r, &vs)?
                        }
                    };
                    for (k, lv) in leaves.into_iter().enumerate() {
                        slots[off + k] = Some(lv);
                    }
                }
                None => {
                    let v = match arg {
                        FlatPayload::Expr(a) => self.lower_expr(a)?,
                        // A compiler-built payload has no source line to mark `materialize`, so the
                        // box it needs is the authorized one.
                        FlatPayload::Repr(r) => {
                            let v = self.pack(r)?;
                            self.authorize_nobox_box(v);
                            v
                        }
                    };
                    slots[off] = Some(v);
                }
            }
        }
        let mut rec_vals = Vec::with_capacity(rec_args.len());
        for (name, value) in rec_args {
            rec_vals.push((name, self.lower_expr(value)?));
        }
        let nil = || Inst::Const(Lit::Nil);
        let positional = slots
            .into_iter()
            .map(|s| match s {
                Some(v) => v,
                None => self.emit_cur(nil()),
            })
            .collect();
        let named = shape
            .record_fields
            .iter()
            .map(|rf| {
                let v = rec_vals
                    .iter()
                    .find(|(n, _)| n == rf)
                    .map(|(_, v)| *v)
                    .unwrap_or_else(|| self.emit_cur(nil()));
                (*rf, v)
            })
            .collect();
        Ok(FlatCells {
            tag,
            positional,
            named,
        })
    }

    pub(crate) fn spread_maybe_null_box_to_cells(
        &mut self,
        boxed: Idx<Inst>,
        shape: &EnumFlatShape,
    ) -> Result<FlatCells, CompilerError> {
        let leaves = self.cdata_leaves(boxed, &shape.value_shape())?;
        self.repr_group_to_cells(shape, leaves.into_iter().map(ValueRepr::Scalar).collect())
    }

    /// Its materialized box is the interned singleton `__rock_enum_<E>_<variant>`.
    pub(crate) fn variant_is_unit(&self, enum_name: Ustr, variant: Ustr) -> bool {
        self.ws
            .enums_by_name
            .get(&enum_name)
            .map(|eid| &self.ws.modules[eid.module.index()].enums[eid.def])
            .and_then(|ed| ed.variants.iter().find(|v| v.name == variant))
            .is_some_and(|v| matches!(v.payload, VariantPayload::Unit))
    }

    /// True for both `E` and `E?`: `expr_ty` peels `Nullable`.
    pub(crate) fn expr_is_flat_enum(&self, e: Idx<Expr>) -> bool {
        self.dispatch().expr_is_flat_enum(e)
    }

    pub(crate) fn flat_enum_value_shape_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<ValueShape>, CompilerError> {
        let Some(id) = self.expr_rep_id(e, RepUse::Value, false)? else {
            return Ok(None);
        };
        let Some(mut shape) = self.scx.rep_arena().value_shape(id).cloned() else {
            return Ok(None);
        };
        if !EnumPlan::for_name(self.ws, shape.name).is_some_and(|plan| plan.flat().is_some()) {
            return Ok(None);
        }
        shape.nullable = false;
        Ok(Some(shape))
    }

    pub(crate) fn resume_frame_read_channel(
        &mut self,
        cobj: Idx<Inst>,
        channel: &EnumFlatShape,
    ) -> ValueRepr {
        let value_shape = channel.value_shape();
        self.resume_frame_read_with_shape(cobj, channel.leaf_count(), &value_shape)
    }

    fn resume_frame_read_with_shape(
        &mut self,
        cobj: Idx<Inst>,
        n: usize,
        value_shape: &ValueShape,
    ) -> ValueRepr {
        let frame = cobj;
        let state = self.read_field(frame, crate::names::ustr("__state"));
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        let done = self.emit_cur(Inst::IntLt(state, zero));
        let cells = self.fresh_cells(n);
        let done_bb = self.f.new_block();
        let live_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_to(done, done_bb, live_bb);
        // Terminal: all-nil leaves (tag = nil => `== null`).
        self.cur = done_bb;
        for c in &cells {
            let z = self.emit_cur(Inst::Const(Lit::Nil));
            self.store_cell(*c, z);
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = live_bb;
        for (i, c) in cells.iter().enumerate() {
            let v = self.read_field(frame, yield_slot(i));
            self.store_cell(*c, v);
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        let leaves = self.load_cells(&cells);
        Self::rebuild_from_leaves(value_shape, &leaves)
    }

    /// Parts are leaf Scalars in `[tag, _0.., <rec>..]` order.
    pub(crate) fn repr_group_to_cells(
        &mut self,
        shape: &EnumFlatShape,
        parts: Vec<ValueRepr>,
    ) -> Result<FlatCells, CompilerError> {
        let packed: Vec<Idx<Inst>> = parts
            .into_iter()
            .map(|p| self.pack(p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(FlatCells::from_slots(shape, &packed))
    }

    /// Cells as a `Group` in `[tag, _0.., <rec>..]` leaf order.
    pub(crate) fn flat_cells_to_repr(&self, shape: &EnumFlatShape, cells: FlatCells) -> ValueRepr {
        ValueRepr::Group {
            name: shape.name,
            parts: cells.into_slots().map(ValueRepr::Scalar).collect(),
            present: None,
        }
    }

    /// Box keys to cells: `__variant` -> tag, `[i+1]` -> positional, `.field` -> named.
    pub(crate) fn spread_box_to_cells(
        &mut self,
        bv: Idx<Inst>,
        shape: &EnumFlatShape,
    ) -> FlatCells {
        let tag = self.read_field(bv, crate::names::ustr("__variant"));
        let positional = (0..shape.tuple_width())
            .map(|i| self.emit_cur(Inst::TupleIndex { base: bv, index: i }))
            .collect();
        let named = shape
            .record_fields
            .clone()
            .into_iter()
            .map(|rf| (rf, self.read_field(bv, rf)))
            .collect();
        FlatCells {
            tag,
            positional,
            named,
        }
    }

    pub(crate) fn load_flat_cells(
        &mut self,
        parent: Idx<Inst>,
        prefix: Ustr,
        shape: &EnumFlatShape,
    ) -> FlatCells {
        let tag = self.read_field(parent, flat_tag_slot(prefix));
        let positional = (0..shape.tuple_width())
            .map(|i| self.read_field(parent, flat_payload_slot(prefix, i)))
            .collect();
        let named = shape
            .record_fields
            .clone()
            .into_iter()
            .map(|rf| (rf, self.read_field(parent, flat_named_slot(prefix, rf))))
            .collect();
        FlatCells {
            tag,
            positional,
            named,
        }
    }

    pub(crate) fn materialize_flat_cells(
        &mut self,
        shape: &EnumFlatShape,
        cells: FlatCells,
    ) -> Idx<Inst> {
        let tag = cells.tag;
        if self.scx.native_flat_enums {
            return self.emit_cur(Inst::MakeEnumDyn {
                tag,
                ty: shape.name,
                positional: cells.positional,
                named: cells.named,
            });
        }
        if let Inst::Const(Lit::Str(variant)) = self.f.insts[tag] {
            return self.box_flat_variant(shape, &cells, variant);
        }

        let result = self.f.new_cell(None);
        let nil_bb = self.f.new_block();
        let box_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_on_nil(tag, nil_bb, box_bb);
        self.cur = nil_bb;
        let absent = self.emit_cur(Inst::Const(Lit::Nil));
        self.store_cell(result, absent);
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = box_bb;
        let eid = self.ws.enums_by_name[&shape.name];
        let units: Vec<_> = self.ws.modules[eid.module.index()].enums[eid.def]
            .variants
            .iter()
            .filter(|v| matches!(v.payload, VariantPayload::Unit))
            .map(|v| v.name)
            .collect();
        for variant in units {
            let known_tag = self.emit_cur(Inst::Const(Lit::Str(variant)));
            let is_unit = self.emit_cur(Inst::Eq(tag, known_tag));
            let unit_bb = self.f.new_block();
            let next_bb = self.f.new_block();
            self.branch_to(is_unit, unit_bb, next_bb);
            self.cur = unit_bb;
            let global = crate::mangle::enum_singleton_global(self.ws, shape.name, variant);
            let singleton = self.emit_cur(Inst::Global(global));
            self.store_cell(result, singleton);
            self.f.set_term(self.cur, Term::Goto(join));
            self.cur = next_bb;
        }
        let boxed = self.emit_cur(Inst::MakeEnumDyn {
            tag,
            ty: shape.name,
            positional: cells.positional,
            named: cells.named,
        });
        self.store_cell(result, boxed);
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        self.emit_cur(Inst::Load(result))
    }

    fn box_flat_variant(
        &mut self,
        shape: &EnumFlatShape,
        cells: &FlatCells,
        variant: Ustr,
    ) -> Idx<Inst> {
        if self.variant_is_unit(shape.name, variant) {
            let global = crate::mangle::enum_singleton_global(self.ws, shape.name, variant);
            return self.emit_cur(Inst::Global(global));
        }
        let (width, fields) = shape.box_layout(self.ws, variant);
        let named = cells
            .named
            .iter()
            .copied()
            .filter(|(name, _)| fields.contains(name))
            .collect();
        self.emit_cur(Inst::MakeEnumDyn {
            tag: cells.tag,
            ty: shape.name,
            positional: cells.positional[..width].to_vec(),
            named,
        })
    }

    /// A nil tag materializes to nil, never `{__variant = nil}`.
    pub(crate) fn materialize_flat(
        &mut self,
        parent: Idx<Inst>,
        prefix: Ustr,
        shape: &EnumFlatShape,
    ) -> Idx<Inst> {
        let cells = self.load_flat_cells(parent, prefix, shape);
        self.materialize_flat_cells(shape, cells)
    }

    pub(crate) fn store_flat_cells(&mut self, parent: Idx<Inst>, prefix: Ustr, cells: FlatCells) {
        self.write_field(parent, flat_tag_slot(prefix), cells.tag);
        for (i, p) in cells.positional.into_iter().enumerate() {
            self.write_field(parent, flat_payload_slot(prefix, i), p);
        }
        for (rf, v) in cells.named {
            self.write_field(parent, flat_named_slot(prefix, rf), v);
        }
    }

    pub(crate) fn flat_literal_slots(
        &self,
        field: Ustr,
        cells: FlatCells,
    ) -> Vec<(Ustr, Idx<Inst>)> {
        let mut out = Vec::with_capacity(1 + cells.positional.len() + cells.named.len());
        out.push((flat_tag_slot(field), cells.tag));
        for (i, p) in cells.positional.into_iter().enumerate() {
            out.push((flat_payload_slot(field, i), p));
        }
        for (rf, v) in cells.named {
            out.push((flat_named_slot(field, rf), v));
        }
        out
    }
}

impl EnumFlatShape {
    pub fn box_layout(&self, ws: &Workspace, variant: Ustr) -> (usize, Vec<Ustr>) {
        let eid = ws.enums_by_name[&self.name];
        let payload = &ws.modules[eid.module.index()].enums[eid.def]
            .variants
            .iter()
            .find(|v| v.name == variant)
            .expect("flat enum tag belongs to its declaration")
            .payload;
        match payload {
            VariantPayload::Tuple(types) => {
                let width = types
                    .len()
                    .checked_sub(1)
                    .map(|last| {
                        let child = match static_ty(ws, types[last]) {
                            Some(TypeExpr::App {
                                ctor: TypeCtor::Nominal(name),
                                ..
                            }) => self.position_shape(last, Some(name)),
                            _ => None,
                        };
                        self.slot_offset(last) + child.map_or(1, |vs| vs.leaf_count())
                    })
                    .unwrap_or(0);
                (width, Vec::new())
            }
            VariantPayload::Record(fields) => {
                let names = self
                    .record_fields
                    .iter()
                    .copied()
                    .filter(|name| fields.iter().any(|field| field.name == *name))
                    .collect();
                (0, names)
            }
            VariantPayload::Unit => (0, Vec::new()),
        }
    }
}
