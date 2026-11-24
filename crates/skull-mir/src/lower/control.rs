use super::*;

#[derive(Default)]
struct EqualityLane {
    nil_equivalent: bool,
    structural_when: Vec<Vec<(usize, Ustr)>>,
}

impl<'a> Builder<'a> {
    pub(crate) fn lower_native_arith(
        &mut self,
        op: &str,
        lt: ArithTy,
        rt: ArithTy,
        a: Idx<Expr>,
        b: Idx<Expr>,
    ) -> Result<ValueBundle, CompilerError> {
        let lv = self.lower_expr(a)?;
        let rv = self.lower_expr(b)?;
        // An int operand of a Float* op is used-as-float; `/` is always float
        // division. String concat and pure int ops leave operands untouched.
        let str_op = op == "__op_add" && (lt == ArithTy::Str || rt == ArithTy::Str);
        let to_float =
            !str_op && (op == "__op_div" || lt == ArithTy::Float || rt == ArithTy::Float);
        let lv = self.coerce_numeric_to_float(lv, lt, to_float);
        let rv = self.coerce_numeric_to_float(rv, rt, to_float);
        let inst = arith_inst(op, lt, rt, lv, rv)?;
        // Desugared compound assignments can omit the call's inferred type; this
        // producer has already selected a single scalar result.
        let representation = self.concrete_rep_id(&MonoTy::Any, RepUse::Value, false, false)?;
        let value = ValueRepr::Scalar(self.emit_cur(inst));
        Ok(ValueBundle::new(representation, value))
    }

    fn value_equality_lanes(
        &self,
        shape: &ValueShape,
        lhs: Idx<Expr>,
    ) -> Result<Vec<EqualityLane>, CompilerError> {
        if super::shape::planned_value(self.ws, shape.name)
            .is_some_and(|plan| plan.storage == ValueStorage::CData)
        {
            return Ok(Vec::new());
        }
        let ty = self
            .concrete_expr_ty(lhs)
            .ok_or_else(|| self.gap_here("value equality has no concrete type"))?;
        let indices: Vec<_> = (0..shape.leaf_count()).collect();
        let mut lanes: Vec<_> = indices.iter().map(|_| EqualityLane::default()).collect();
        self.plan_equality_lanes(shape, ty, &indices, &mut Vec::new(), &mut lanes)?;
        Ok(lanes)
    }

    fn plan_equality_lanes(
        &self,
        shape: &ValueShape,
        ty: &MonoTy,
        indices: &[usize],
        guards: &mut Vec<(usize, Ustr)>,
        lanes: &mut [EqualityLane],
    ) -> Result<(), CompilerError> {
        let indices = &indices[usize::from(shape.nullable)..];
        if let Some(plan) = EnumPlan::for_name(self.ws, shape.name) {
            if let Some(flat) = plan.flat() {
                self.plan_enum_equality_lanes(flat, ty, indices, guards, lanes)?;
            }
            return Ok(());
        }
        let mut start = 0;
        for (field, layout) in &shape.fields {
            let ty = self.struct_field_ty_in(shape.name, *field, ty)?;
            let end = start + layout.leaf_count();
            match layout {
                FieldShape::Nested(child) => {
                    self.plan_equality_lanes(child, &ty, &indices[start..end], guards, lanes)?;
                }
                FieldShape::Prim if self.opaque_leaf_is_structural(&ty, false) => {
                    // An opaque lane can hold a boxed value.
                    lanes[indices[start]].structural_when.push(guards.clone());
                }
                FieldShape::Prim => {}
            }
            start = end;
        }
        Ok(())
    }

    fn opaque_leaf_is_structural(&self, ty: &MonoTy, enum_payload: bool) -> bool {
        let js = self.scx.target() == bone_core::hir::FnBackend::Js;
        let ty = ty.peel_nullable().0;
        if let Some(name) = ty.nominal() {
            if super::shape::is_value_struct(self.ws, name) {
                return js;
            }
            if let Some(plan) = EnumPlan::for_name(self.ws, name) {
                return plan.flat().is_some() || (js && plan.cdata().is_some());
            }
        }
        enum_payload
            && !matches!(
                ty,
                MonoTy::Unit
                    | MonoTy::Int
                    | MonoTy::Uint
                    | MonoTy::Float
                    | MonoTy::Bool
                    | MonoTy::String
                    | MonoTy::Never
                    | MonoTy::Function { .. }
            )
    }

    fn plan_enum_equality_lanes(
        &self,
        flat: &EnumFlatShape,
        ty: &MonoTy,
        indices: &[usize],
        guards: &mut Vec<(usize, Ustr)>,
        lanes: &mut [EqualityLane],
    ) -> Result<(), CompilerError> {
        let (tag, indices) = indices.split_first().expect("flat enum has a tag");
        let Some(eid) = self.ws.nominals.enum_id(flat.name()) else {
            return Err(self.gap_here("flat enum has no declaration"));
        };
        let def = &self.ws.module(eid.module).enums[eid.def];
        let mut subst = crate::monoty::MonoSubst::default();
        if let MonoTy::App { params, .. } = ty.peel_nullable().0 {
            for (generic, param) in def.generics.iter().zip(params) {
                if let Some(TypeExpr::Generic(var, _)) = super::shape::static_ty(self.ws, *generic)
                {
                    subst.insert(var, param.clone());
                }
            }
        }
        if self.scx.target() == bone_core::hir::FnBackend::Js {
            // A box omits nil payload fields; the scalar ABI passes null.
            for &index in indices {
                lanes[index].nil_equivalent = true;
            }
        }
        let mut visit = |ty, shape: Option<&ValueShape>, indices: &[usize], guards: &mut Vec<_>| {
            let ty = crate::monoty::convert_decl(self.ws, ty, &subst).map_err(|reason| {
                self.gap_here(format!("enum equality payload type: {reason:?}"))
            })?;
            if let Some(shape) = shape {
                self.plan_equality_lanes(shape, &ty, indices, guards, lanes)?;
            } else if self.opaque_leaf_is_structural(&ty, true) {
                lanes[indices[0]].structural_when.push(guards.clone());
            }
            Ok::<_, CompilerError>(())
        };
        for variant in &def.variants {
            guards.push((*tag, variant.name));
            match &variant.payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(types) => {
                    for (pos, ty) in types.iter().enumerate() {
                        let shape = flat.payload_shape(self.ws, variant.name, pos);
                        let start = flat.slot_offset(pos);
                        let width = shape.map_or(1, ValueShape::leaf_count);
                        visit(*ty, shape, &indices[start..start + width], guards)?;
                    }
                }
                VariantPayload::Record(fields) => {
                    let column = |key| {
                        let offset = flat
                            .record_fields()
                            .iter()
                            .position(|&f| f == key)
                            .expect("named payload has a physical slot");
                        indices[flat.tuple_width() + offset]
                    };
                    for field in fields {
                        if let Some(payload) = flat.named_payload(variant.name, field.name) {
                            let slots: Vec<_> = payload.keys.iter().copied().map(column).collect();
                            visit(field.ty, Some(&payload.shape), &slots, guards)?;
                        } else {
                            visit(field.ty, None, &[column(field.name)], guards)?;
                        }
                    }
                }
            }
            guards.pop();
        }
        Ok(())
    }

    fn emit_value_leaves_equal(
        &mut self,
        lhs: &[Idx<Inst>],
        rhs: &[Idx<Inst>],
        lanes: &[EqualityLane],
    ) -> Result<Idx<Inst>, CompilerError> {
        if lanes.is_empty() {
            return Ok(self.emit_all_equal(lhs, rhs, |b, _, a, c| b.emit_cur(Inst::Eq(a, c))));
        }
        let structural = if lanes.iter().any(|lane| !lane.structural_when.is_empty()) {
            // The prelude supplies the intrinsic alongside ordinary operator overloads.
            let func = self
                .ws
                .functions_by_name
                .get(&crate::names::ustr("__op_eq"))
                .into_iter()
                .flatten()
                .find_map(|&(module, func)| {
                    let decl = &self.ws.module(module).functions[func];
                    (decl.inline_body.is_some() && crate::intrinsics::is_intrinsic(decl))
                        .then_some(FuncRef { module, func })
                })
                .ok_or_else(|| self.gap_here("structural equality intrinsic is missing"))?;
            Some(func)
        } else {
            None
        };
        Ok(self.emit_all_equal(lhs, rhs, |b, i, a, c| {
            let lane = &lanes[i];
            let native = |b: &mut Self| {
                let [a, c] = [a, c].map(|value| {
                    if lane.nil_equivalent {
                        let nil = b.is_nil(value);
                        let present = b.emit_cur(Inst::Not(nil));
                        b.emit_cur(Inst::MakeNullableValue { present, value })
                    } else {
                        value
                    }
                });
                b.emit_cur(Inst::Eq(a, c))
            };
            if lane.structural_when.is_empty()
                || matches!(b.f.insts[a], Inst::Const(Lit::Nil))
                || matches!(b.f.insts[c], Inst::Const(Lit::Nil))
            {
                return native(b);
            }
            let compare = |b: &mut Self| {
                b.emit_cur(Inst::Call {
                    func: structural.expect("structural equality intrinsic"),
                    args: vec![a, c],
                })
            };
            if lane.structural_when.iter().any(Vec::is_empty) {
                return compare(b);
            }
            // The same physical slot can hold a reference in another variant.
            let structural = b.f.new_block();
            for guards in &lane.structural_when {
                let miss = b.f.new_block();
                for (i, &(tag, variant)) in guards.iter().enumerate() {
                    let variant = b.emit_cur(Inst::Const(Lit::Str(variant)));
                    let matched = b.emit_cur(Inst::Eq(lhs[tag], variant));
                    let hit = if i + 1 == guards.len() {
                        structural
                    } else {
                        b.f.new_block()
                    };
                    b.branch_to(matched, hit, miss);
                    b.cur = hit;
                }
                b.cur = miss;
            }
            let result = b.f.new_cell(None);
            let join = b.f.new_block();
            let equal = native(b);
            b.store_and_join(result, equal, join);
            b.cur = structural;
            let equal = compare(b);
            b.store_and_join(result, equal, join);
            b.cur = join;
            b.emit_cur(Inst::Load(result))
        }))
    }

    fn emit_all_equal(
        &mut self,
        lhs: &[Idx<Inst>],
        rhs: &[Idx<Inst>],
        mut compare: impl FnMut(&mut Self, usize, Idx<Inst>, Idx<Inst>) -> Idx<Inst>,
    ) -> Idx<Inst> {
        debug_assert_eq!(lhs.len(), rhs.len());
        let result = self.f.new_cell(None);
        let fail = self.f.new_block();
        let join = self.f.new_block();
        for (i, (&a, &b)) in lhs.iter().zip(rhs).enumerate() {
            let equal = compare(self, i, a, b);
            let next = self.f.new_block();
            self.branch_to(equal, next, fail);
            self.cur = next;
        }
        let yes = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.store_cell(result, yes);
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = fail;
        let no = self.emit_cur(Inst::Const(Lit::Bool(false)));
        self.store_cell(result, no);
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = join;
        self.emit_cur(Inst::Load(result))
    }

    fn lower_leafwise_value_eq(
        &mut self,
        op: BinOp,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
        shape: &ValueShape,
    ) -> Result<Idx<Inst>, CompilerError> {
        let lhs_repr = self.lower_expr_repr(lhs)?;
        let mut lhs_leaves =
            self.repr_to_storage_leaves(&lhs_repr, shape, "value equality left operand")?;
        let rhs_repr = self.lower_expr_repr(rhs)?;
        let mut rhs_leaves =
            self.repr_to_storage_leaves(&rhs_repr, shape, "value equality right operand")?;
        // A nested child's presence tag spells absence `false` on the scalar ABI and
        // `nil` once read back from storage, so tags compare by truth.
        let top = usize::from(shape.nullable);
        for (i, tag) in super::shape::value_leaf_presence_flags(shape)
            .into_iter()
            .enumerate()
        {
            if tag {
                lhs_leaves[top + i] = self.emit_cur(Inst::Not(lhs_leaves[top + i]));
                rhs_leaves[top + i] = self.emit_cur(Inst::Not(rhs_leaves[top + i]));
            }
        }
        let lanes = self.value_equality_lanes(shape, lhs)?;
        let equal = if shape.nullable
            && matches!(
                self.scx.target(),
                bone_core::hir::FnBackend::Lua | bone_core::hir::FnBackend::Js
            ) {
            let left_absent = self.emit_cur(Inst::Not(lhs_leaves[0]));
            let right_absent = self.emit_cur(Inst::Not(rhs_leaves[0]));
            let result = self.f.new_cell(None);
            let absent = self.f.new_block();
            let present = self.f.new_block();
            let join = self.f.new_block();
            self.branch_to(left_absent, absent, present);
            self.cur = absent;
            self.store_and_join(result, right_absent, join);
            self.cur = present;
            let no = self.emit_cur(Inst::Const(Lit::Bool(false)));
            let mut left = vec![no];
            left.extend_from_slice(&lhs_leaves[1..]);
            let mut right = vec![right_absent];
            right.extend_from_slice(&rhs_leaves[1..]);
            let equal = self.emit_value_leaves_equal(&left, &right, &lanes)?;
            self.store_and_join(result, equal, join);
            self.cur = join;
            self.emit_cur(Inst::Load(result))
        } else {
            self.emit_value_leaves_equal(&lhs_leaves, &rhs_leaves, &lanes)?
        };
        Ok(if matches!(op, BinOp::Eq) {
            equal
        } else {
            self.emit_cur(Inst::Not(equal))
        })
    }

    pub(crate) fn lower_binop(
        &mut self,
        op: BinOp,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
        condition: bool,
    ) -> Result<Idx<Inst>, CompilerError> {
        match op {
            BinOp::Lt | BinOp::Leq | BinOp::Gt | BinOp::Geq => {
                let lf = self.arith_ty(lhs);
                let rf = self.arith_ty(rhs);
                let float = lf == Some(ArithTy::Float) || rf == Some(ArithTy::Float);
                let uint = lf == Some(ArithTy::Uint) || rf == Some(ArithTy::Uint);
                let lv = self.lower_expr(lhs)?;
                let rv = self.lower_expr(rhs)?;
                // A mixed float compare (`x > 0`, `i < f`) promotes its int side --
                // both Float* operands must arrive float (GLSL ES has no implicit
                // int->float in a compare).
                let lv = self.coerce_numeric_to_float(lv, lf.unwrap_or(ArithTy::Str), float);
                let rv = self.coerce_numeric_to_float(rv, rf.unwrap_or(ArithTy::Str), float);
                let inst = match (op, float, uint) {
                    (BinOp::Lt, false, true) => Inst::UintLt(lv, rv),
                    (BinOp::Lt, false, false) => Inst::IntLt(lv, rv),
                    (BinOp::Lt, true, _) => Inst::FloatLt(lv, rv),
                    (BinOp::Leq, false, true) => Inst::UintLeq(lv, rv),
                    (BinOp::Leq, false, false) => Inst::IntLeq(lv, rv),
                    (BinOp::Leq, true, _) => Inst::FloatLeq(lv, rv),
                    (BinOp::Gt, false, true) => Inst::UintGt(lv, rv),
                    (BinOp::Gt, false, false) => Inst::IntGt(lv, rv),
                    (BinOp::Gt, true, _) => Inst::FloatGt(lv, rv),
                    (BinOp::Geq, false, true) => Inst::UintGeq(lv, rv),
                    (BinOp::Geq, false, false) => Inst::IntGeq(lv, rv),
                    (BinOp::Geq, true, _) => Inst::FloatGeq(lv, rv),
                    _ => unreachable!(),
                };
                Ok(self.emit_cur(inst))
            }
            BinOp::Eq | BinOp::NotEq => {
                // A flat enum's nil tag encodes absence without its payload.
                let null_side = if self.is_null_lit(rhs) {
                    Some(lhs)
                } else if self.is_null_lit(lhs) {
                    Some(rhs)
                } else {
                    None
                };
                if let Some(v) = null_side {
                    if let Some(test) = self.flat_ref_presence(v, op)? {
                        return Ok(test);
                    }
                    // A `V?` place is read here for its presence; the copy such a read otherwise
                    // takes would allocate a cdata per nil test.
                    let boxed_local = matches!(self.body.expr_arena[v], Expr::Var(b)
                        if self.nullable_boxed_value_local(b, v).is_some());
                    if self.nobox_value_name(v).is_none()
                        && !self.expr_is_flat_enum(v)
                        && (boxed_local || self.nullable_place_value_of(v)?.is_some())
                    {
                        let outer = self.presence_only.replace(v);
                        let value = self.lower_expr(v);
                        self.presence_only = outer;
                        let value = value?;
                        let nil = self.emit_cur(Inst::Const(Lit::Nil));
                        return Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                            Inst::Eq(value, nil)
                        } else {
                            Inst::NotEq(value, nil)
                        }));
                    }
                    // A group-bound nullable value carries its presence as a leaf even when its
                    // read's rep layout is boxed, so `nullable_value_shape_of` alone misses it.
                    let group_bound_nullable = matches!(self.body.expr_arena[v], Expr::Var(b)
                        if self.group_binds.get(&raw(b)).is_some_and(|(_, shape)| shape.nullable));
                    if self.nobox_value_name(v).is_some()
                        || self.nullable_value_shape_of(v)?.is_some()
                        || group_bound_nullable
                    {
                        let repr = self.lower_expr_repr(v)?.into_value();
                        let present = match repr {
                            ValueRepr::Group {
                                present: Some(value),
                                ..
                            } => value,
                            ValueRepr::Group { present: None, .. } => {
                                self.emit_cur(Inst::Const(Lit::Bool(true)))
                            }
                            ValueRepr::Scalar(value) => {
                                // Packing a borrowed call result can copy its payload.
                                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                                return Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                                    Inst::Eq(value, nil)
                                } else {
                                    Inst::NotEq(value, nil)
                                }));
                            }
                            other => {
                                let value = self.pack(other)?;
                                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                                self.emit_cur(Inst::NotEq(value, nil))
                            }
                        };
                        return Ok(self.lower_presence_test(op, present, condition));
                    }
                    // A splatting `(..)?` carries its own presence slot. Packing would build a
                    // `MakeTuple`, a table that is never nil, so `== null` would be false.
                    if self.nullable_tuple_wire_shape(v)?.is_some() {
                        let repr = self.lower_expr_repr(v)?.into_value();
                        let present = match repr {
                            ValueRepr::Tuple {
                                present: Some(value),
                                ..
                            } => value,
                            other => {
                                let value = self.pack(other)?;
                                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                                self.emit_cur(Inst::NotEq(value, nil))
                            }
                        };
                        return Ok(self.lower_presence_test(op, present, condition));
                    }
                    if self.expr_is_flat_enum(v) {
                        let tag = if let Some(tag) = self.lower_flat_enum_field_tag(v, None)? {
                            tag
                        } else {
                            match self.lower_expr_repr(v)?.into_value() {
                                ValueRepr::Group { parts, .. } => self.pack(
                                    parts
                                        .into_iter()
                                        .next()
                                        .expect("flat enum group has a tag leaf"),
                                )?,
                                // A boxed flat enum is nil-or-table and compares directly.
                                other => self.pack(other)?,
                            }
                        };
                        let nil = self.emit_cur(Inst::Const(Lit::Nil));
                        return Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                            Inst::Eq(tag, nil)
                        } else {
                            Inst::NotEq(tag, nil)
                        }));
                    }
                }
                if let Some(test) = self.cdata_enum_unit_eq(op, lhs, rhs)? {
                    return Ok(test);
                }
                if let Some(test) = self.flat_enum_field_unit_eq(op, lhs, rhs)? {
                    return Ok(test);
                }
                if let (Some(ln), Some(rn)) =
                    (self.nobox_value_name(lhs), self.nobox_value_name(rhs))
                {
                    if ln == rn {
                        let mut shape = self.nobox_value_shape_of(lhs)?.ok_or_else(|| {
                            gap(format!("nobox value `{ln}` has no scalar shape"))
                        })?;
                        let rhs_shape = self.nobox_value_shape_of(rhs)?.ok_or_else(|| {
                            gap(format!("nobox value `{rn}` has no scalar shape"))
                        })?;
                        if shape.name != rhs_shape.name || shape.fields != rhs_shape.fields {
                            return Err(gap(format!(
                                "nobox equality operands for `{ln}` have different scalar shapes"
                            )));
                        }
                        shape.nullable |= rhs_shape.nullable;
                        return self.lower_leafwise_value_eq(op, lhs, rhs, &shape);
                    }
                }
                // Ordinary values can arrive as leaves under a scalar ABI.
                let same_value = self.scx.target() == bone_core::hir::FnBackend::Lua
                    && !self.scx.values_first_class
                    && matches!(
                        (self.dispatch().value_struct_name(lhs), self.dispatch().value_struct_name(rhs)),
                        (Some(ln), Some(rn)) if ln == rn
                    );
                if same_value {
                    if let Some(shape) = self
                        .nullable_value_shape_of(lhs)?
                        .or(self.nullable_value_shape_of(rhs)?)
                    {
                        return self.lower_leafwise_value_eq(op, lhs, rhs, &shape);
                    }
                    // Identity lanes are integral. Other values retain their metatype's rounding,
                    // presence and structural field rules.
                    let integral = self
                        .value_struct_name_of(lhs)
                        .and_then(|name| value_struct_field_tys(self.ws, name))
                        .is_some_and(|fields| {
                            fields.iter().all(|(_, ty)| {
                                matches!(
                                    super::shape::static_ty(self.ws, *ty),
                                    Some(TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Uint))
                                )
                            })
                        });
                    if integral {
                        let ty = self
                            .concrete_expr_ty(lhs)
                            .ok_or_else(|| self.gap_here("value equality has no concrete type"))?;
                        let id = self.concrete_rep_id(
                            ty.peel_nullable().0,
                            RepUse::Parameter,
                            false,
                            false,
                        )?;
                        if let Some(mut shape) = self.scx.rep_arena().value_shape(id).cloned() {
                            shape.nullable =
                                self.expr_is_nullable(lhs) || self.expr_is_nullable(rhs);
                            return self.lower_leafwise_value_eq(op, lhs, rhs, &shape);
                        }
                    }
                    let lhs_repr = self.lower_expr_repr(lhs)?.into_value();
                    let rhs_repr = self.lower_expr_repr(rhs)?.into_value();
                    let mut pairs = Vec::new();
                    let shape = match &lhs_repr {
                        ValueRepr::Group { name, .. } => self.nominal_rep_shape(*name, false)?,
                        _ => None,
                    };
                    if let Some(shape) = shape
                        && matching_scalar_leaves(
                            self.ws,
                            self.scx.values_first_class,
                            &lhs_repr,
                            &rhs_repr,
                            &shape,
                            &mut pairs,
                        )
                    {
                        let (mut lhs_leaves, mut rhs_leaves) = (Vec::new(), Vec::new());
                        for pair in pairs {
                            match pair {
                                LeafPair::Leaf(l, r) => {
                                    lhs_leaves.push(l);
                                    rhs_leaves.push(r);
                                }
                                LeafPair::Split(l, r, sub) => {
                                    lhs_leaves.extend(self.repr_to_leaves_shape(&l, &sub)?);
                                    rhs_leaves.extend(self.repr_to_leaves_shape(&r, &sub)?);
                                }
                            }
                        }
                        let lanes = self.value_equality_lanes(&shape, lhs)?;
                        let equal =
                            self.emit_value_leaves_equal(&lhs_leaves, &rhs_leaves, &lanes)?;
                        return Ok(if matches!(op, BinOp::Eq) {
                            equal
                        } else {
                            self.emit_cur(Inst::Not(equal))
                        });
                    }
                    let lv = self.pack(lhs_repr)?;
                    let rv = self.pack(rhs_repr)?;
                    return Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                        Inst::Eq(lv, rv)
                    } else {
                        Inst::NotEq(lv, rv)
                    }));
                }
                if let (Some(lhs_shape), Some(rhs_shape)) = (
                    self.flat_enum_value_shape_of(lhs)?,
                    self.flat_enum_value_shape_of(rhs)?,
                ) {
                    if lhs_shape.name == rhs_shape.name
                        && lhs_shape.fields == rhs_shape.fields
                        && flat_enum_equality_is_leafwise(self.ws, lhs_shape.name)
                    {
                        return self.lower_leafwise_value_eq(op, lhs, rhs, &lhs_shape);
                    }
                }
                let route = self.dispatch().eq_route(lhs, rhs);
                let lhs_ty = self.concrete_expr_ty(lhs);
                let rhs_ty = self.concrete_expr_ty(rhs);
                let lv = self.lower_expr(lhs)?;
                let lv = self.promote_unit(lv, lhs_ty, rhs_ty);
                let rv = self.lower_expr(rhs)?;
                let rv = self.promote_unit(rv, rhs_ty, lhs_ty);
                if self.scx.values_first_class && matches!(route, EqRoute::Structural) {
                    return Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                        Inst::Eq(lv, rv)
                    } else {
                        Inst::NotEq(lv, rv)
                    }));
                }
                if let Some(name) = route.synthesized_callee(op) {
                    return self.dispatched_call(name, Vec::new(), vec![lv, rv]);
                }
                // Mixed `float == int` promotes the int side (untyped `Eq` renders a
                // single `==`; both operands must arrive same-typed for GLSL).
                let lf = self.arith_ty(lhs);
                let rf = self.arith_ty(rhs);
                let float = lf == Some(ArithTy::Float) || rf == Some(ArithTy::Float);
                let lv = self.coerce_numeric_to_float(lv, lf.unwrap_or(ArithTy::Str), float);
                let rv = self.coerce_numeric_to_float(rv, rf.unwrap_or(ArithTy::Str), float);
                let [lv, rv] = if self.scx.target() == bone_core::hir::FnBackend::Js
                    && null_side.is_none()
                    && (self.expr_is_nullable(lhs) || self.expr_is_nullable(rhs))
                {
                    // Reading a missing JS slot yields undefined. Rock has one null.
                    [lv, rv].map(|value| {
                        let nil = self.is_nil(value);
                        let present = self.emit_cur(Inst::Not(nil));
                        self.emit_cur(Inst::MakeNullableValue { present, value })
                    })
                } else {
                    [lv, rv]
                };
                Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                    Inst::Eq(lv, rv)
                } else {
                    Inst::NotEq(lv, rv)
                }))
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                let lv = self.lower_expr(lhs)?;
                let rv = self.lower_expr(rhs)?;
                let uint = self.arith_ty(lhs) == Some(ArithTy::Uint)
                    || self.arith_ty(rhs) == Some(ArithTy::Uint);
                let inst = match op {
                    BinOp::BitAnd if uint => Inst::UintBitAnd(lv, rv),
                    BinOp::BitOr if uint => Inst::UintBitOr(lv, rv),
                    BinOp::BitXor if uint => Inst::UintBitXor(lv, rv),
                    BinOp::Shl if uint => Inst::UintShl(lv, rv),
                    BinOp::Shr if uint => Inst::UintShr(lv, rv),
                    BinOp::BitAnd => Inst::BitAnd(lv, rv),
                    BinOp::BitOr => Inst::BitOr(lv, rv),
                    BinOp::BitXor => Inst::BitXor(lv, rv),
                    BinOp::Shl => Inst::Shl(lv, rv),
                    BinOp::Shr => Inst::Shr(lv, rv),
                    _ => unreachable!(),
                };
                Ok(self.emit_cur(inst))
            }
            // The rhs may not evaluate, so short-circuit operators lower to branches with
            // the result flowing through a cell.
            BinOp::And => self.lower_short_circuit(lhs, rhs, condition, true),
            BinOp::Or => self.lower_short_circuit(lhs, rhs, condition, false),
            BinOp::NullCoalesce => self.lower_nullcoalesce(lhs, rhs),
        }
    }

    fn flat_enum_field_unit_eq(
        &mut self,
        op: BinOp,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<Option<Idx<Inst>>, CompilerError> {
        let unit = |e: Idx<Expr>| match self.body.expr_arena[e] {
            Expr::EnumVariant {
                enum_name,
                variant,
                args: VariantArgs::Unit,
                ..
            } => Some((self.literal_layout(e, enum_name), variant)),
            _ => None,
        };
        let (value, (layout, variant)) = match (unit(lhs), unit(rhs)) {
            (_, Some(unit)) => (lhs, unit),
            (Some(unit), None) => (rhs, unit),
            (None, None) => return Ok(None),
        };
        let Some(tag) = self.lower_flat_enum_field_tag(value, Some(layout))? else {
            return Ok(None);
        };
        let want = self.emit_cur(Inst::Const(Lit::Str(variant)));
        Ok(Some(self.emit_cur(if matches!(op, BinOp::Eq) {
            Inst::Eq(tag, want)
        } else {
            Inst::NotEq(tag, want)
        })))
    }

    /// A payload-free variant's cdata has zeroed payload slots, so a value equals
    /// it exactly when its tag does; the generic `==` would go through `__eq`.
    fn cdata_enum_unit_eq(
        &mut self,
        op: BinOp,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<Option<Idx<Inst>>, CompilerError> {
        let unit_tag = |e: Idx<Expr>| match self.body.expr_arena[e] {
            Expr::EnumVariant {
                enum_name,
                variant,
                args: VariantArgs::Unit,
                ..
            } => Some((
                enum_name,
                EnumPlan::for_name(self.ws, enum_name)?
                    .cdata()?
                    .tag_of(variant)?,
            )),
            _ => None,
        };
        let (value, (enum_name, tag)) = match (unit_tag(lhs), unit_tag(rhs)) {
            (_, Some(unit)) => (lhs, unit),
            (Some(unit), None) => (rhs, unit),
            (None, None) => return Ok(None),
        };
        let same_enum = self.concrete_expr_ty(value).is_some_and(|ty| {
            let (inner, nullable) = ty.peel_nullable();
            !nullable && inner.nominal() == Some(enum_name)
        });
        if !same_enum {
            return Ok(None);
        }
        let actual = match self.lower_expr_repr(value)?.into_value() {
            ValueRepr::Group {
                parts,
                present: None,
                ..
            } => self.pack(parts.into_iter().next().expect("cdata enum has a tag leaf"))?,
            other => {
                let boxed = self.pack(other)?;
                self.read_field(boxed, crate::names::ustr("__tag"))
            }
        };
        let want = self.emit_cur(Inst::Const(Lit::Int(tag)));
        Ok(Some(self.emit_cur(if matches!(op, BinOp::Eq) {
            Inst::Eq(actual, want)
        } else {
            Inst::NotEq(actual, want)
        })))
    }

    fn lower_presence_test(&mut self, op: BinOp, present: Idx<Inst>, condition: bool) -> Idx<Inst> {
        if matches!(op, BinOp::Eq) {
            self.emit_cur(Inst::Not(present))
        } else if condition {
            present
        } else {
            // An absent place can carry nil; escaped bools must carry false.
            let absent = self.emit_cur(Inst::Not(present));
            self.emit_cur(Inst::Not(absent))
        }
    }

    /// Both arms inherit the return destination, so no result cell or join is
    /// required.
    pub(crate) fn lower_if_to_return(
        &mut self,
        cond: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let then_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        self.lower_branch(cond, then_bb, else_bb, true)?;
        self.cur = then_bb;
        self.lower_return(Some(then_branch))?;
        self.cur = else_bb;
        self.lower_return(Some(else_branch))
    }

    /// An arm that diverges skips its store and join-goto.
    pub(crate) fn lower_if(
        &mut self,
        cond: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let repr = self.lower_if_repr(cond, then_branch, else_branch, RepLayout::Opaque)?;
        self.pack_maybe_absent(repr)
    }

    pub(crate) fn lower_if_repr(
        &mut self,
        cond: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
        shape: RepLayout,
    ) -> Result<ValueRepr, CompilerError> {
        let result_ty = matches!(shape, RepLayout::Opaque)
            .then(|| self.current_expr_unit_storage_ty_id())
            .flatten();
        // An opaque slot is the whole value in one cell, so the expression's type
        // decides nil; splatting slots hold leaves this type does not describe.
        let never_nil = !shape.splats()
            && self
                .cur_expr
                .and_then(|e| self.concrete_expr_ty(e))
                .is_some_and(|ty| crate::nilness::monoty_never_nil(self.ws, ty));
        let arms = self.value_join_typed(shape, result_ty, never_nil);
        let then_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.lower_branch(cond, then_bb, else_bb, true)?;

        self.cur = then_bb;
        let expected = self.current_expr_mono();
        self.lower_value_arms(
            arms,
            else_bb,
            join,
            |s| s.lower_expr_repr_as(then_branch, expected),
            else_branch,
        )
    }

    pub(super) fn lower_value_arms(
        &mut self,
        mut arms: ValueJoin,
        else_bb: Idx<Block>,
        join: Idx<Block>,
        then_value: impl FnOnce(&mut Self) -> Result<ValueBundle, CompilerError>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<ValueRepr, CompilerError> {
        let expected = self.current_expr_mono();
        let then_value = then_value(self)?;
        let representation = then_value.representation();
        self.join_arm(&mut arms, then_value)?;
        self.cur = else_bb;
        let else_value = match else_branch {
            Some(expr) => self.lower_expr_repr_as(expr, expected)?,
            None => ValueBundle::new(
                representation,
                ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))),
            ),
        };
        self.join_arm(&mut arms, else_value)?;
        self.finish_join(arms, join)
    }

    pub(crate) fn value_join(&mut self, shape: RepLayout) -> ValueJoin {
        self.value_join_typed(shape, None, false)
    }

    fn value_join_typed(
        &mut self,
        shape: RepLayout,
        cell_ty: Option<TyId>,
        never_nil: bool,
    ) -> ValueJoin {
        let results = (0..shape.slot_count(self.scx.rep_arena()))
            .map(|_| {
                let cell = self.f.new_cell_typed(None, cell_ty);
                self.f.cells[cell].never_nil = never_nil;
                cell
            })
            .collect();
        ValueJoin {
            shape,
            results,
            arms: Vec::new(),
        }
    }

    /// Record one arm's value; its block stays open until [`Self::finish_join`]
    /// knows every arm and can pick the form they meet in.
    pub(crate) fn join_arm(
        &mut self,
        join: &mut ValueJoin,
        value: ValueBundle,
    ) -> Result<(), CompilerError> {
        if !self.is_open() {
            return Ok(());
        }
        let owned = self.snapshot_opaque_value(value.value().clone())?;
        let value = ValueBundle::new(value.representation(), owned);
        join.arms.push((self.cur, JoinArm::Value(value)));
        Ok(())
    }

    /// For an arm whose slots are already in hand, such as a `??` lhs read before
    /// the branch.
    pub(crate) fn join_slots_arm(&mut self, join: &mut ValueJoin, slots: Vec<Idx<Inst>>) {
        if self.is_open() {
            join.arms.push((self.cur, JoinArm::Slots(slots)));
        }
    }

    /// Arms that are all one flat tuple box meet as that box, so a whole-value use
    /// shares it; anything else meets as slots.
    pub(crate) fn finish_join(
        &mut self,
        values: ValueJoin,
        join: Idx<Block>,
    ) -> Result<ValueRepr, CompilerError> {
        let ValueJoin {
            shape,
            results,
            arms,
        } = values;
        let boxes = match &shape {
            RepLayout::Tuple(tuple) if !tuple.nullable && !arms.is_empty() => arms
                .iter()
                .map(|(_, arm)| match arm {
                    JoinArm::Value(value) => self.flat_box_of_repr(value.value()),
                    JoinArm::Slots(slots) => self.flat_box_of_slots(slots),
                })
                .collect::<Option<Vec<_>>>(),
            _ => None,
        };
        if let (Some(boxes), RepLayout::Tuple(tuple)) = (boxes, &shape) {
            let cell = self.f.new_cell(None);
            for ((block, _), boxed) in arms.into_iter().zip(boxes) {
                self.cur = block;
                self.store_slots_and_join(&[cell], &[boxed], join)?;
            }
            self.cur = join;
            let boxed = self.emit_cur(Inst::Load(cell));
            let slots = self.flat_box_slots(boxed, tuple.slot_count(self.scx.rep_arena()));
            return Ok(self.tuple_from_slots(tuple, &slots, &mut 0));
        }
        for (block, arm) in arms {
            self.cur = block;
            let slots = match arm {
                JoinArm::Value(value) => self.repr_to_slot_values_bundle(&value, &shape)?,
                JoinArm::Slots(slots) => slots,
            };
            self.store_slots_and_join(&results, &slots, join)?;
        }
        self.cur = join;
        let slots = self.load_cells(&results);
        Ok(self.slot_repr(&shape, &slots, &mut 0))
    }

    /// Arms recurse in statement position, so nested if/block/match also skip
    /// their result cells.
    pub(crate) fn lower_if_stmt(
        &mut self,
        cond: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<(), CompilerError> {
        if let Some(take_then) = self.static_sort_key_condition(cond)? {
            return match (take_then, else_branch) {
                (true, _) => self.lower_stmt_expr(then_branch),
                (false, Some(eb)) => self.lower_stmt_expr(eb),
                (false, None) => Ok(()),
            };
        }
        let then_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.lower_branch(cond, then_bb, else_bb, true)?;

        self.cur = then_bb;
        self.lower_statement_arms(then_branch, else_branch, else_bb, join)
    }

    pub(super) fn lower_statement_arms(
        &mut self,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
        else_bb: Idx<Block>,
        join: Idx<Block>,
    ) -> Result<(), CompilerError> {
        self.lower_stmt_expr(then_branch)?;
        self.goto_if_open(join);

        self.cur = else_bb;
        if let Some(eb) = else_branch {
            self.lower_stmt_expr(eb)?;
        }
        self.goto_if_open(join);

        self.cur = join;
        Ok(())
    }

    pub(crate) fn safe_nav_repr(
        &mut self,
        recv: SafeNavRecv,
        shape: Option<RepLayout>,
        value_arm: impl FnOnce(&mut Self, SafeNavRecv) -> Result<SafeNavValue, CompilerError>,
    ) -> Result<ValueRepr, CompilerError> {
        let scalar = shape.is_none();
        let (cond, present_test) = match &recv {
            SafeNavRecv::Boxed(value) => (self.is_nil(*value), false),
            SafeNavRecv::Unboxed { present, .. } if scalar => (*present, true),
            SafeNavRecv::Unboxed { present, .. } => (self.emit_cur(Inst::Not(*present)), false),
        };
        let shape = shape.unwrap_or(RepLayout::Opaque);
        let result_ty = matches!(shape, RepLayout::Opaque)
            .then(|| self.current_expr_storage_ty_id())
            .flatten();
        let values = self.value_join_typed(shape, result_ty, false);
        let [nil_bb, val_bb, join] = self.optional_blocks(cond, present_test);

        if scalar {
            self.cur = nil_bb;
            let nil = self.emit_cur(Inst::Const(Lit::Nil));
            self.store_and_join(values.results[0], nil, join);
        }
        self.cur = val_bb;
        match value_arm(self, recv)? {
            // Field projection has already applied the copy policy for this
            // wire slot. Only general value arms need the join's snapshot.
            SafeNavValue::Slot(value) => {
                self.store_and_join(values.results[0], value, join);
                self.finish_join(values, join)
            }
            SafeNavValue::Repr(value) => {
                self.lower_value_arms(values, nil_bb, join, |_| Ok(value), None)
            }
        }
    }

    /// `absent` when `cond` differs from `cond_is_presence`, else the value arm.
    pub(crate) fn absent_or_join(
        &mut self,
        result_ty: Option<TyId>,
        cond: Idx<Inst>,
        cond_is_presence: bool,
        absent: Lit,
        value_arm: impl FnOnce(&mut Self) -> Result<Idx<Inst>, CompilerError>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let result = self.f.new_cell_typed(None, result_ty);
        let [nil_bb, val_bb, join] = self.optional_blocks(cond, cond_is_presence);

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(absent));
        self.store_and_join(result, n, join);

        self.cur = val_bb;
        let v = value_arm(self)?;
        self.store_and_join(result, v, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    fn optional_blocks(&mut self, cond: Idx<Inst>, is_presence: bool) -> [Idx<Block>; 3] {
        let blocks @ [absent, present, _] = std::array::from_fn(|_| self.f.new_block());
        if is_presence {
            self.branch_to(cond, present, absent);
        } else {
            self.branch_to(cond, absent, present);
        }
        blocks
    }

    pub(crate) fn nil_or_join(
        &mut self,
        cond: Idx<Inst>,
        cond_is_presence: bool,
        value_arm: impl FnOnce(&mut Self) -> Result<Idx<Inst>, CompilerError>,
    ) -> Result<Idx<Inst>, CompilerError> {
        self.absent_or_join(None, cond, cond_is_presence, Lit::Nil, value_arm)
    }

    /// A diverged arm leaves the join untouched.
    pub(crate) fn store_and_join(&mut self, result: Idx<Cell>, value: Idx<Inst>, join: Idx<Block>) {
        if self.is_open() {
            self.store_cell(result, value);
            self.f.set_term(self.cur, Term::Goto(join));
        }
    }

    /// The open-block guard is here, not at callers: a diverged arm has nothing to
    /// store, and storing would read leaves off a nil before the divergence.
    fn store_slots_and_join(
        &mut self,
        results: &[Idx<Cell>],
        slots: &[Idx<Inst>],
        join: Idx<Block>,
    ) -> Result<(), CompilerError> {
        if !self.is_open() {
            return Ok(());
        }
        if results.len() != slots.len() {
            return Err(gap(format!(
                "aggregate join produced {} slots for {} result cells",
                slots.len(),
                results.len()
            )));
        }
        for (&cell, &value) in results.iter().zip(slots) {
            self.store_cell(cell, value);
        }
        self.f.set_term(self.cur, Term::Goto(join));
        Ok(())
    }

    fn lower_short_circuit(
        &mut self,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
        condition: bool,
        and: bool,
    ) -> Result<Idx<Inst>, CompilerError> {
        let result = self
            .f
            .new_cell_typed(None, self.current_expr_storage_ty_id());
        let lv = self.lower_condition(lhs)?;
        let then_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_to(lv, then_bb, else_bb);
        for (bb, evaluates_rhs) in [(then_bb, and), (else_bb, !and)] {
            self.cur = bb;
            let v = if !evaluates_rhs {
                self.emit_cur(Inst::Const(Lit::Bool(!and)))
            } else if condition {
                self.lower_condition(rhs)?
            } else {
                self.lower_expr(rhs)?
            };
            self.store_and_join(result, v, join);
        }
        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// `a ?? b` == `if a == nil then b else a`, with `a` evaluated once.
    pub(crate) fn lower_nullcoalesce(
        &mut self,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let result = self
            .f
            .new_cell_typed(None, self.current_expr_storage_ty_id());
        let lv = self.lower_expr(lhs)?;
        let expected = self.current_expr_mono();
        let rhs_bb = self.f.new_block();
        let lhs_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_on_nil(lv, rhs_bb, lhs_bb);
        self.cur = rhs_bb;
        let rv = self.lower_unit_coercion(rhs, expected)?;
        self.store_and_join(result, rv, join);
        self.cur = lhs_bb;
        let lv = if matches!(expected, Some(MonoTy::Unit))
            && Self::optional_unit(self.concrete_expr_ty(lhs))
        {
            self.emit_cur(Inst::Const(Lit::Nil))
        } else {
            // Reuse `lv`: its defining block dominates this one.
            lv
        };
        self.store_and_join(result, lv, join);
        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }
}

enum LeafPair {
    Leaf(Idx<Inst>, Idx<Inst>),
    Split(ValueRepr, ValueRepr, ValueShape),
}

/// Answers false when either side is packed or carries a presence leaf. A
/// first-class target compares a boxed child whole.
fn matching_scalar_leaves(
    ws: &Workspace,
    first_class: bool,
    lhs: &ValueRepr,
    rhs: &ValueRepr,
    shape: &ValueShape,
    out: &mut Vec<LeafPair>,
) -> bool {
    match (lhs, rhs) {
        // A flat enum with a structurally compared payload is not equal leaf for
        // leaf; the packed `__eq` decides it.
        (ValueRepr::Group { name, .. }, _)
            if EnumPlan::for_name(ws, *name).is_some_and(|plan| plan.flat().is_some())
                && !flat_enum_equality_is_leafwise(ws, *name) =>
        {
            false
        }
        (
            ValueRepr::Group {
                name: ln,
                parts: lp,
                present: None,
            },
            ValueRepr::Group {
                name: rn,
                parts: rp,
                present: None,
            },
        ) if ln == rn && lp.len() == rp.len() && lp.len() == shape.fields.len() => shape
            .fields
            .iter()
            .zip(lp.iter().zip(rp))
            .all(|((_, fs), (l, r))| match (fs, l, r) {
                (FieldShape::Prim, ValueRepr::Scalar(l), ValueRepr::Scalar(r)) => {
                    out.push(LeafPair::Leaf(*l, *r));
                    true
                }
                (FieldShape::Prim, ..) => false,
                (FieldShape::Nested(sub), ValueRepr::Group { .. }, ValueRepr::Group { .. }) => {
                    matching_scalar_leaves(ws, first_class, l, r, sub, out)
                }
                (FieldShape::Nested(_), ValueRepr::Scalar(l), ValueRepr::Scalar(r))
                    if first_class =>
                {
                    out.push(LeafPair::Leaf(*l, *r));
                    true
                }
                // A boxed child is not one leaf: raw `==` on two enum tables compares identity.
                (FieldShape::Nested(sub), ..) if shape_equality_is_leafwise(ws, sub) => {
                    out.push(LeafPair::Split(l.clone(), r.clone(), sub.clone()));
                    true
                }
                (FieldShape::Nested(_), ..) => false,
            }),
        _ => false,
    }
}

/// No child may be nullable: an absent child's presence leaf is `nil` read out
/// of a box but `false` on a literal, so raw leaf `==` calls them different.
fn shape_equality_is_leafwise(ws: &Workspace, shape: &ValueShape) -> bool {
    if shape.nullable {
        return false;
    }
    if EnumPlan::for_name(ws, shape.name).is_some_and(|plan| plan.flat().is_some())
        && !flat_enum_equality_is_leafwise(ws, shape.name)
    {
        return false;
    }
    shape.fields.iter().all(|(_, fs)| match fs {
        FieldShape::Prim => true,
        FieldShape::Nested(sub) => shape_equality_is_leafwise(ws, sub),
    })
}

pub(crate) struct ValueJoin {
    shape: RepLayout,
    results: Vec<Idx<Cell>>,
    arms: Vec<(Idx<Block>, JoinArm)>,
}

enum JoinArm {
    Value(ValueBundle),
    Slots(Vec<Idx<Inst>>),
}

pub(crate) enum SafeNavValue {
    Slot(Idx<Inst>),
    Repr(ValueBundle),
}
