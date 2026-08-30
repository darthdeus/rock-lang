//! Flattened enums: an enum whose payload lives in dotted parent slots rather
//! than a nested table, plus the cell group/repr conversions a coroutine frame
//! and a flat literal both go through.
use super::*;
impl Builder<'_> {
    pub(crate) fn new_flat_enum_cells(
        &mut self,
        name: Ustr,
        shape: &ValueShape,
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
        let owner = self.scx.interner().intern(MonoTy::App {
            ctor: MonoCtor::Nominal(flat.name),
            params: Vec::new(),
        });
        let tuple_width = flat.tuple_width();
        let mut scalarized_fields = vec![None; tuple_width];
        let mut offset = 0;
        for slot in &flat.slots {
            if matches!(slot, FlatSlot::Leaves(_)) {
                for index in offset..offset + slot.width() {
                    scalarized_fields[index] = Some(Ustr::from(index.to_string().as_str()));
                }
            }
            offset += slot.width();
        }
        let cells = (0..flat.leaf_count())
            .map(|index| {
                let cell_name = Ustr::from(format!("{name}__{index}").as_str());
                if index == 0 {
                    return self.f.new_cell(Some(cell_name));
                }
                let slot = index - 1;
                if let Some(field) = scalarized_fields.get(slot).and_then(|field| *field) {
                    self.f
                        .new_cell_native_flat_slot(Some(cell_name), owner, field)
                } else {
                    self.f.new_cell(Some(cell_name))
                }
            })
            .collect();
        Ok(Some(cells))
    }

    /// Lower an enum-typed value into its flattened cells: the discriminant
    /// (variant-name string) plus ALL union slots (every positional + every record
    /// field), nil where the live variant doesn't fill them so stale slots from a
    /// prior wider/other-shaped variant are cleared. A direct `E::V(..)` ctor fuses
    /// to a string const + the lowered args (no box); any other enum expression is
    /// lowered to a box once and spread through the nil-safe leaf projection.
    pub(crate) fn lower_flat_value(
        &mut self,
        shape: &EnumFlatShape,
        value: Idx<Expr>,
    ) -> Result<FlatCells, CompilerError> {
        if let Expr::EnumVariant { enum_name, variant, args, .. } = self.body.expr_arena[value].clone() {
            if enum_name == shape.name {
                let tag = self.emit_cur(Inst::Const(Lit::Str(variant)));
                // Positional args are placed at their POSITION's slot offset, and a
                // scalarizable value spreads into its leaves there -- the layout comes
                // from the enum declaration, so this and the match arm that reads
                // it back agree without either seeing the other.
                let mut slots: Vec<Option<Idx<Inst>>> = vec![None; shape.tuple_width()];
                let rec_args: Vec<(Ustr, Idx<Inst>)> = match args {
                    VariantArgs::Unit => Vec::new(),
                    VariantArgs::Tuple(range) => {
                        for (pos, a) in range.into_iter().enumerate() {
                            let off = shape.slot_offset(pos);
                            // Widened only for THIS variant's own shape: a
                            // sibling carrying an ordinary payload, or a scalarizable
                            // value of another shape, addresses its own leading
                            // slots and the rest of the position nil-pads.
                            match self.flat_payload_leaf_shape(Some(enum_name), variant, pos) {
                                Some(vs) => {
                                    let r = self.lower_expr_repr(a)?;
                                    let leaves = self.repr_to_storage_leaves(
                                        &r,
                                        &vs,
                                        "flat enum constructor payload",
                                    )?;
                                    for (k, lv) in leaves.into_iter().enumerate()
                                    {
                                        slots[off + k] = Some(lv);
                                    }
                                }
                                None => slots[off] = Some(self.lower_expr(a)?),
                            }
                        }
                        Vec::new()
                    }
                    VariantArgs::Record(fields) => {
                        let mut rec = Vec::new();
                        for fidx in fields {
                            let lf = self.body.struct_lit_field_arena[fidx];
                            let v = self.lower_expr(lf.value)?;
                            rec.push((lf.name, v));
                        }
                        rec
                    }
                };
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
                        let v = rec_args
                            .iter()
                            .find(|(n, _)| n == rf)
                            .map(|(_, v)| *v)
                            .unwrap_or_else(|| self.emit_cur(nil()));
                        (*rf, v)
                    })
                    .collect();
                return Ok(FlatCells { tag, positional, named });
            }
        }
        // General enum value: thread it as a repr. A flat-enum Group (a scalarized
        // param/return/local, or a fresh ctor) yields its cells directly -- no box;
        // a Scalar (a genuinely boxed source: global singleton, `any`, if-join) is
        // spread back out.
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

    pub(crate) fn spread_maybe_null_box_to_cells(
        &mut self,
        boxed: Idx<Inst>,
        shape: &EnumFlatShape,
    ) -> Result<FlatCells, CompilerError> {
        let leaves = self.cdata_leaves(boxed, &shape.value_shape())?;
        self.repr_group_to_cells(
            shape,
            leaves.into_iter().map(ValueRepr::Scalar).collect(),
        )
    }

    /// Whether `variant` of `enum_name` is a Unit (payload-free) variant -- its
    /// materialized box is the interned singleton `__rock_enum_<E>_<variant>`.
    pub(crate) fn variant_is_unit(&self, enum_name: Ustr, variant: Ustr) -> bool {
        self.ws
            .enums_by_name
            .get(&enum_name)
            .map(|eid| &self.ws.modules[eid.module.index()].enums[eid.def])
            .and_then(|ed| ed.variants.iter().find(|v| v.name == variant))
            .is_some_and(|v| matches!(v.payload, VariantPayload::Unit))
    }

    /// Whether `e`'s inferred type is a flat enum (`expr_ty` peels `Nullable`, so
    /// this is true for both `E` and `E?`).
    pub(crate) fn expr_is_flat_enum(&self, e: Idx<Expr>) -> bool {
        self.dispatch().expr_is_flat_enum(e)
    }

    /// The flat-enum `ValueShape` of `e`'s (non-nullable) inferred type, else `None`
    /// -- the gate for threading a `let`/reassign target as leaf cells.
    pub(crate) fn flat_enum_value_shape_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<ValueShape>, CompilerError> {
        let Some(id) = self.expr_rep_id(e, RepUse::Value, false)? else { return Ok(None) };
        let Some(mut shape) = self.scx.rep_arena().value_shape(id) else { return Ok(None) };
        if !EnumPlan::for_name(self.ws, shape.name).is_some_and(|plan| plan.flat().is_some()) {
            return Ok(None);
        }
        shape.nullable = false;
        Ok(Some(shape))
    }

    pub(crate) fn resume_frame_read_channel(
        &mut self,
        cobj: Idx<Inst>,
        channel: &YieldChannel,
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
        let state = self.emit_cur(Inst::Field { base: frame, field: Ustr::from("__state") });
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        let done = self.emit_cur(Inst::IntLt(state, zero));
        let cells: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
        let done_bb = self.f.new_block();
        let live_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: done, then_bb: done_bb, else_bb: live_bb });
        // Terminal: all-nil leaves (tag = nil => `== null`).
        self.cur = done_bb;
        for c in &cells {
            let z = self.emit_cur(Inst::Const(Lit::Nil));
            self.emit_cur(Inst::Store { cell: *c, value: z });
        }
        self.f.set_term(self.cur, Term::Goto(join));
        // Live: spread the reused frame slots into the leaf cells.
        self.cur = live_bb;
        for (i, c) in cells.iter().enumerate() {
            let v = self.emit_cur(Inst::Field { base: frame, field: yield_slot(i) });
            self.emit_cur(Inst::Store { cell: *c, value: v });
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        let leaves: Vec<Idx<Inst>> = cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        let mut idx = 0;
        self.rebuild_from_leaves(value_shape, &leaves, &mut idx)
    }

    /// Split a flat-enum `ValueRepr::Group` into its cells (the inverse of
    /// `flat_cells_to_repr`) -- no box. Parts are leaf Scalars in `value_shape`
    /// order: `[tag, _0.., <rec>..]`.
    pub(crate) fn repr_group_to_cells(
        &mut self,
        shape: &EnumFlatShape,
        parts: Vec<ValueRepr>,
    ) -> Result<FlatCells, CompilerError> {
        let packed: Vec<Idx<Inst>> = parts
            .into_iter()
            .map(|p| self.pack(p))
            .collect::<Result<Vec<_>, _>>()?;
        let tag = packed[0];
        let positional = packed[1..1 + shape.tuple_width()].to_vec();
        let named = shape
            .record_fields
            .iter()
            .enumerate()
            .map(|(i, rf)| (*rf, packed[1 + shape.tuple_width() + i]))
            .collect();
        Ok(FlatCells { tag, positional, named })
    }

    /// A flat enum's runtime cells as a `ValueRepr::Group` in `value_shape` leaf
    /// order (`[tag, _0.., <rec>..]`) -- the scalar-ABI carrier for a flat enum.
    pub(crate) fn flat_cells_to_repr(&self, shape: &EnumFlatShape, cells: FlatCells) -> ValueRepr {
        let mut parts = Vec::with_capacity(shape.leaf_count());
        parts.push(ValueRepr::Scalar(cells.tag));
        parts.extend(cells.positional.into_iter().map(ValueRepr::Scalar));
        parts.extend(cells.named.into_iter().map(|(_, c)| ValueRepr::Scalar(c)));
        ValueRepr::Group {
            name: shape.name,
            parts,
            present: None,
        }
    }

    /// Read a boxed enum's keys back out into flattened cells (`__variant` -> tag,
    /// `[i+1]` -> positional, `.field` -> named) -- the box-to-slots spread.
    pub(crate) fn spread_box_to_cells(&mut self, bv: Idx<Inst>, shape: &EnumFlatShape) -> FlatCells {
        let tag = self.emit_cur(Inst::Field { base: bv, field: Ustr::from("__variant") });
        let positional = (0..shape.tuple_width())
            .map(|i| self.emit_cur(Inst::TupleIndex { base: bv, index: i }))
            .collect();
        let named = shape
            .record_fields
            .clone()
            .into_iter()
            .map(|rf| (rf, self.emit_cur(Inst::Field { base: bv, field: rf })))
            .collect();
        FlatCells { tag, positional, named }
    }

    /// Read a flat enum field from the physical slots of its parent struct.
    pub(crate) fn load_flat_cells(&mut self, parent: Idx<Inst>, prefix: Ustr, shape: &EnumFlatShape) -> FlatCells {
        let tag = self.emit_cur(Inst::Field { base: parent, field: flat_tag_slot(prefix) });
        let positional = (0..shape.tuple_width())
            .map(|i| self.emit_cur(Inst::Field { base: parent, field: flat_payload_slot(prefix, i) }))
            .collect();
        let named = shape
            .record_fields
            .clone()
            .into_iter()
            .map(|rf| (rf, self.emit_cur(Inst::Field { base: parent, field: flat_named_slot(prefix, rf) })))
            .collect();
        FlatCells { tag, positional, named }
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
            if self.variant_is_unit(shape.name, variant) {
                let global = Ustr::from(format!("__rock_enum_{}_{}", shape.name, variant).as_str());
                return self.emit_cur(Inst::Global(global));
            }
            return self.emit_cur(Inst::MakeEnumDyn {
                tag,
                ty: shape.name,
                positional: cells.positional,
                named: cells.named,
            });
        }

        let result = self.f.new_cell(None);
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(tag, nil));
        let nil_bb = self.f.new_block();
        let box_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: box_bb },
        );
        self.cur = nil_bb;
        let absent = self.emit_cur(Inst::Const(Lit::Nil));
        self.emit_cur(Inst::Store { cell: result, value: absent });
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = box_bb;
        let boxed = self.emit_cur(Inst::MakeEnumDyn {
            tag,
            ty: shape.name,
            positional: cells.positional,
            named: cells.named,
        });
        self.emit_cur(Inst::Store { cell: result, value: boxed });
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        self.emit_cur(Inst::Load(result))
    }

    /// Re-materialize a flattened enum field `parent.{prefix}` at a whole-value
    /// read boundary. A nil tag materializes to nil, never `{__variant = nil}`.
    pub(crate) fn materialize_flat(&mut self, parent: Idx<Inst>, prefix: Ustr, shape: &EnumFlatShape) -> Idx<Inst> {
        let cells = self.load_flat_cells(parent, prefix, shape);
        self.materialize_flat_cells(shape, cells)
    }

    /// Emit the per-slot stores for a flattened enum field assignment
    /// `parent.{prefix} = <cells>` (tag + every positional + every named slot).
    pub(crate) fn store_flat_cells(&mut self, parent: Idx<Inst>, prefix: Ustr, cells: FlatCells) {
        self.emit_cur(Inst::StoreField { base: parent, field: flat_tag_slot(prefix), value: cells.tag });
        for (i, p) in cells.positional.into_iter().enumerate() {
            self.emit_cur(Inst::StoreField { base: parent, field: flat_payload_slot(prefix, i), value: p });
        }
        for (rf, v) in cells.named {
            self.emit_cur(Inst::StoreField { base: parent, field: flat_named_slot(prefix, rf), value: v });
        }
    }

    /// The `(slot_name, value)` pairs for a flattened enum field in a struct literal
    /// (tag + every positional + every named slot), keyed by the parent field name.
    pub(crate) fn flat_literal_slots(&self, field: Ustr, cells: FlatCells) -> Vec<(Ustr, Idx<Inst>)> {
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
