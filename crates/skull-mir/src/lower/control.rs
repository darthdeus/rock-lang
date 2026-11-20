use super::*;

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
        let equal = if shape.nullable && self.scx.target() == bone_core::hir::FnBackend::Lua {
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
            let equal = self.emit_all_equal(&left, &right);
            self.store_and_join(result, equal, join);
            self.cur = join;
            self.emit_cur(Inst::Load(result))
        } else {
            self.emit_all_equal(&lhs_leaves, &rhs_leaves)
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
                // `x == null` on a flat enum compares the tag slot (nil tag is null):
                // materializing a box would compare a table to nil, always false.
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
                        let tag = match self.lower_expr_repr(v)?.into_value() {
                            ValueRepr::Group { parts, .. } => self.pack(
                                parts
                                    .into_iter()
                                    .next()
                                    .expect("flat enum group has a tag leaf"),
                            )?,
                            // A boxed flat enum is nil-or-table and compares directly.
                            other => self.pack(other)?,
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
                        let equal = self.emit_all_equal(&lhs_leaves, &rhs_leaves);
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
                        let lhs_repr = self.lower_expr_repr(lhs)?;
                        let lhs_leaves = self.repr_to_storage_leaves(
                            &lhs_repr,
                            &lhs_shape,
                            "flat enum equality",
                        )?;
                        let rhs_repr = self.lower_expr_repr(rhs)?;
                        let rhs_leaves = self.repr_to_storage_leaves(
                            &rhs_repr,
                            &rhs_shape,
                            "flat enum equality",
                        )?;
                        let equal = self.emit_all_equal(&lhs_leaves, &rhs_leaves);
                        return Ok(if matches!(op, BinOp::Eq) {
                            equal
                        } else {
                            self.emit_cur(Inst::Not(equal))
                        });
                    }
                }
                let route = self.dispatch().eq_route(lhs, rhs);
                let lv = self.lower_expr(lhs)?;
                let rv = self.lower_expr(rhs)?;
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
        self.lower_value_arms(
            arms,
            else_bb,
            join,
            |s| s.lower_expr_repr(then_branch),
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
        let then_value = then_value(self)?;
        let representation = then_value.representation();
        self.join_arm(&mut arms, then_value)?;
        self.cur = else_bb;
        let else_value = match else_branch {
            Some(expr) => self.lower_expr_repr(expr)?,
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

    /// Lower the receiver once and preserve the result's per-slot representation.
    pub(crate) fn safe_nav_repr(
        &mut self,
        receiver: Idx<Expr>,
        shape: RepLayout,
        value_arm: impl FnOnce(&mut Self, Idx<Inst>) -> Result<ValueBundle, CompilerError>,
    ) -> Result<ValueRepr, CompilerError> {
        let recv = self.lower_safe_nav_receiver(receiver)?;
        let is_nil = self.safe_nav_absent_cond(&recv);
        let result_ty = matches!(shape, RepLayout::Opaque)
            .then(|| self.current_expr_storage_ty_id())
            .flatten();
        let values = self.value_join_typed(shape, result_ty, false);
        let nil_bb = self.f.new_block();
        let val_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_to(is_nil, nil_bb, val_bb);

        self.cur = val_bb;
        self.lower_value_arms(
            values,
            nil_bb,
            join,
            |s| {
                let recv_v = s.safe_nav_present_value(recv)?;
                value_arm(s, recv_v)
            },
            None,
        )
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
        let nil_bb = self.f.new_block();
        let val_bb = self.f.new_block();
        let join = self.f.new_block();
        if cond_is_presence {
            self.branch_to(cond, val_bb, nil_bb);
        } else {
            self.branch_to(cond, nil_bb, val_bb);
        }

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(absent));
        self.store_and_join(result, n, join);

        self.cur = val_bb;
        let v = value_arm(self)?;
        self.store_and_join(result, v, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
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
        let rhs_bb = self.f.new_block();
        let lhs_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_on_nil(lv, rhs_bb, lhs_bb);
        self.cur = rhs_bb;
        let rv = self.lower_expr(rhs)?;
        self.store_and_join(result, rv, join);
        // Reuse `lv`: its defining block dominates this one.
        self.cur = lhs_bb;
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
