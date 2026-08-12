//! Flattened enums: an enum whose payload lives in dotted parent slots rather
//! than a nested table, plus the cell group/repr conversions a coroutine frame
//! and a flat literal both go through.
use super::*;
use crate::monoty::MonoTy;

impl Builder<'_> {
    /// Lower an enum-typed value into its flattened cells: the discriminant
    /// (variant-name string) plus ALL union slots (every positional + every record
    /// field), nil where the live variant doesn't fill them so stale slots from a
    /// prior wider/other-shaped variant are cleared. A direct `E::V(..)` ctor fuses
    /// to a string const + the lowered args (no box); any other enum expression is
    /// lowered to a box once and spread (`__variant` + `[i+1]` + `.field`).
    pub(crate) fn lower_flat_value(
        &mut self,
        shape: &EnumFlatShape,
        value: Idx<Expr>,
    ) -> Result<FlatCells, CompilerError> {
        if let Expr::EnumVariant { enum_name, variant, args, .. } = self.body.expr_arena[value].clone() {
            if enum_name == shape.name {
                let tag = self.emit_cur(Inst::Const(Lit::Str(variant)));
                // Positional args are placed at their POSITION's slot offset, and a
                // `nobox` one spreads into its leaves there -- the layout comes
                // from the enum declaration, so this and the match arm that reads
                // it back agree without either seeing the other.
                let mut slots: Vec<Option<Idx<Inst>>> = vec![None; shape.tuple_width()];
                let rec_args: Vec<(Ustr, Idx<Inst>)> = match args {
                    VariantArgs::Unit => Vec::new(),
                    VariantArgs::Tuple(range) => {
                        for (pos, a) in range.into_iter().enumerate() {
                            let off = shape.slot_offset(pos);
                            // Widened only for THIS variant's own shape: a
                            // sibling carrying an ordinary payload, or a `nobox`
                            // value of another shape, addresses its own leading
                            // slots and the rest of the position nil-pads.
                            match shape.position_shape(pos, self.nobox_value_name(a)) {
                                Some(vs) => {
                                    let vs = vs.clone();
                                    let r = self.lower_expr_repr(a)?;
                                    for (k, lv) in self.repr_to_leaves(&r, &vs).into_iter().enumerate()
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
        let r = self.lower_expr_repr(value)?;
        match r {
            ValueRepr::Group { name, parts, .. }
                if EnumPlan::for_name(self.ws, name).is_some_and(|plan| plan.flat().is_some()) =>
            {
                Ok(self.repr_group_to_cells(shape, parts))
            }
            other => {
                let bv = self.pack(other);
                Ok(self.spread_box_to_cells(bv, shape))
            }
        }
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
    pub(crate) fn flat_enum_value_shape_of(&self, e: Idx<Expr>) -> Option<ValueShape> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name),
                ..
            } = ty.peel_nullable().0
            else {
                return None;
            };
            return EnumPlan::for_name(self.ws, *name).and_then(|plan| plan.flat().map(|s| s.value_shape()));
        }
        if self.concrete_plan.is_some() {
            return None;
        }
        match self.expr_ty(e)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
                EnumPlan::for_name(self.ws, n).and_then(|plan| plan.flat().map(|s| s.value_shape()))
            }
            _ => None,
        }
    }

    /// The `EnumFlatShape` of `e`'s (`Nullable`-peeled) inferred type, else `None`.
    pub(crate) fn flat_enum_shape_of_expr(&self, e: Idx<Expr>) -> Option<EnumFlatShape> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name),
                ..
            } = ty.peel_nullable().0
            else {
                return None;
            };
            return EnumPlan::for_name(self.ws, *name).and_then(|plan| plan.flat().cloned());
        }
        if self.concrete_plan.is_some() {
            return None;
        }
        match self.expr_ty(e)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
                EnumPlan::for_name(self.ws, n).and_then(|plan| plan.flat().cloned())
            }
            _ => None,
        }
    }

    /// Whether `e`'s inferred type is a `Coroutine<..>` -- the guard that a
    /// `resume(e)` call is a real coroutine resume (not a user fn named `resume`).
    pub(crate) fn is_coroutine_recv(&self, e: Idx<Expr>) -> bool {
        if let Some(ty) = self.concrete_expr_ty(e) {
            return matches!(
                ty.peel_nullable().0,
                MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name), ..
                } if name.as_str() == "Coroutine"
            );
        }
        if self.concrete_plan.is_some() {
            return false;
        }
        matches!(
            self.expr_ty(e),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) if n.as_str() == "Coroutine"
        )
    }

    /// Read a resumed flat-enum yield value from the coroutine's frame slots as an
    /// unboxed `Group` -- the concrete `c.resume()` channel. The object IS the
    /// frame: `cobj.__y*` holds the leaves the `__step` wrote; when the coroutine
    /// is terminal (`__state < 0`) the value is null, threaded as an all-nil group
    /// (`tag = nil` sentinel, matching the nullable flat-enum ABI). The consumer
    /// must have already advanced the coroutine (`cobj.__advance(cobj, arg)`)
    /// before this read.
    pub(crate) fn resume_frame_read(&mut self, cobj: Idx<Inst>, shape: &EnumFlatShape) -> ValueRepr {
        let value_shape = shape.value_shape();
        self.resume_frame_read_with_shape(cobj, shape.leaf_count(), &value_shape)
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
    pub(crate) fn repr_group_to_cells(&mut self, shape: &EnumFlatShape, parts: Vec<ValueRepr>) -> FlatCells {
        let packed: Vec<Idx<Inst>> = parts.into_iter().map(|p| self.pack(p)).collect();
        let tag = packed[0];
        let positional = packed[1..1 + shape.tuple_width()].to_vec();
        let named = shape
            .record_fields
            .iter()
            .enumerate()
            .map(|(i, rf)| (*rf, packed[1 + shape.tuple_width() + i]))
            .collect();
        FlatCells { tag, positional, named }
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

    /// Re-materialize a flattened enum field `parent.{prefix}` to a boxed enum
    /// (`MakeEnumDyn`) at a whole-value-read boundary.
    pub(crate) fn materialize_flat(&mut self, parent: Idx<Inst>, prefix: Ustr, shape: &EnumFlatShape) -> Idx<Inst> {
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
        self.emit_cur(Inst::MakeEnumDyn { tag, ty: shape.name, positional, named })
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
