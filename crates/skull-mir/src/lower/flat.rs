use super::*;
impl<'a> Builder<'a> {
    pub(super) fn flat_binding_width(
        &self,
        shape: &ValueShape,
    ) -> Result<Option<usize>, CompilerError> {
        let Some(plan) = EnumPlan::for_name(self.ws, shape.name) else {
            return Ok(None);
        };
        let Some(flat) = plan.flat() else {
            return Ok(None);
        };
        let width = flat.leaf_count();
        if shape.name != flat.name || shape.leaf_count() != width {
            return Err(self.gap_here(format!(
                "flat enum {} has a cell shape that differs from its compiler-owned layout",
                shape.name
            )));
        }
        Ok(Some(width))
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
            match shape.payload_shape(self.ws, variant, pos) {
                Some(vs) => {
                    let leaves = self.flat_payload_leaves(
                        arg,
                        vs,
                        super::shape::track_enum_payload_presence(self.ws, vs),
                    )?;
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
            if let Some(payload) = shape.named_payload(variant, name) {
                let leaves =
                    self.flat_payload_leaves(FlatPayload::Expr(value), &payload.shape, true)?;
                rec_vals.extend(payload.keys.iter().copied().zip(leaves));
            } else {
                rec_vals.push((name, self.lower_expr(value)?));
            }
        }
        let nil = || Inst::Const(Lit::Nil);
        let mut cells = Vec::with_capacity(shape.leaf_count());
        cells.push(tag);
        cells.extend(
            slots
                .into_iter()
                .map(|slot| slot.unwrap_or_else(|| self.emit_cur(nil()))),
        );
        cells.extend(shape.record_fields().iter().map(|field| {
            rec_vals
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, value)| *value)
                .unwrap_or_else(|| self.emit_cur(nil()))
        }));
        Ok(FlatCells {
            slots: cells,
            fields: shape.record_fields.clone(),
        })
    }

    fn flat_payload_leaves(
        &mut self,
        arg: FlatPayload,
        shape: &ValueShape,
        normalize_presence: bool,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let mut leaves = match arg {
            FlatPayload::Expr(expr) => {
                let repr = self.lower_expr_repr(expr)?;
                let repr = ValueBundle::new(
                    repr.representation(),
                    self.owned_value_children(repr.into_value())?,
                );
                self.repr_to_storage_leaves(&repr, shape, "flat enum payload")?
            }
            FlatPayload::Repr(repr) => {
                let repr = self.owned_value_children(repr)?;
                self.repr_to_leaves_shape(&repr, shape)?
            }
        };
        // ABI and field loads may spell absence differently. Keep one presence
        // representation in enum slots so both leafwise and boxed equality agree.
        if normalize_presence {
            let flags = shape
                .nullable
                .then_some(true)
                .into_iter()
                .chain(super::shape::value_leaf_presence_flags(shape));
            for (leaf, presence) in leaves.iter_mut().zip(flags) {
                if presence {
                    let absent = self.emit_cur(Inst::Not(*leaf));
                    *leaf = self.emit_cur(Inst::Not(absent));
                }
            }
        }
        Ok(leaves)
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
            .nominals
            .enum_id(enum_name)
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
        let mut slots = parts
            .into_iter()
            .map(|part| self.pack(part))
            .collect::<Result<Vec<_>, _>>()?;
        assert!(slots.len() >= shape.leaf_count());
        slots.truncate(shape.leaf_count());
        Ok(FlatCells {
            slots,
            fields: shape.record_fields.clone(),
        })
    }

    /// Cells as a `Group` in `[tag, _0.., <rec>..]` leaf order.
    pub(crate) fn flat_cells_to_repr(&self, shape: &EnumFlatShape, cells: FlatCells) -> ValueRepr {
        ValueRepr::Group {
            name: shape.name,
            parts: cells.slots().map(ValueRepr::Scalar).collect(),
            present: None,
        }
    }

    /// Box keys to cells: `__variant` -> tag, `[i+1]` -> positional, `.field` -> named.
    pub(crate) fn spread_box_to_cells(
        &mut self,
        bv: Idx<Inst>,
        shape: &EnumFlatShape,
    ) -> FlatCells {
        let mut slots = Vec::with_capacity(shape.leaf_count());
        slots.push(self.read_field(bv, crate::names::ustr("__variant")));
        slots.extend(
            (0..shape.tuple_width())
                .map(|index| self.emit_cur(Inst::TupleIndex { base: bv, index })),
        );
        slots.extend(
            shape
                .record_fields()
                .iter()
                .map(|&field| self.read_field(bv, field)),
        );
        FlatCells {
            slots,
            fields: shape.record_fields.clone(),
        }
    }

    pub(crate) fn load_flat_cells(
        &mut self,
        parent: Idx<Inst>,
        prefix: Ustr,
        shape: &EnumFlatShape,
    ) -> FlatCells {
        let slots = flat_keys(prefix, shape.tuple_width(), shape.record_fields())
            .map(|key| self.read_field(parent, key))
            .collect();
        FlatCells {
            slots,
            fields: shape.record_fields.clone(),
        }
    }

    pub(crate) fn materialize_flat_cells(
        &mut self,
        shape: &EnumFlatShape,
        cells: FlatCells,
    ) -> Idx<Inst> {
        let tag = cells.tag();
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
        let eid = self.ws.nominals.enum_id(shape.name).unwrap();
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
            positional: cells.positional().to_vec(),
            named: cells.named().collect(),
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
            .named()
            .filter(|(name, _)| fields.contains(name))
            .collect();
        self.emit_cur(Inst::MakeEnumDyn {
            tag: cells.tag(),
            ty: shape.name,
            positional: cells.positional()[..width].to_vec(),
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
        for (key, value) in
            flat_keys(prefix, cells.positional().len(), &cells.fields).zip(cells.slots)
        {
            self.write_field(parent, key, value);
        }
    }

    pub(crate) fn flat_literal_slots(
        &self,
        prefix: Ustr,
        cells: FlatCells,
    ) -> Vec<(Ustr, Idx<Inst>)> {
        flat_keys(prefix, cells.positional().len(), &cells.fields)
            .zip(cells.slots)
            .collect()
    }
}

impl EnumFlatShape {
    pub fn box_layout(&self, ws: &Workspace, variant: Ustr) -> (usize, Vec<Ustr>) {
        let eid = ws.nominals.enum_id(self.name).unwrap();
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
                        let child = self.payload_shape(ws, variant, last);
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
                    .filter(|name| {
                        fields.iter().any(|field| {
                            self.named_payload(variant, field.name)
                                .map_or(field.name == *name, |payload| payload.keys.contains(name))
                        })
                    })
                    .collect();
                (0, names)
            }
            VariantPayload::Unit => (0, Vec::new()),
        }
    }
}
