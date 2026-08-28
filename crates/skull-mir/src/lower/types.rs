//! Type and shape queries over the body being lowered: nullability, tuple and
//! return shapes, spread operands, and the `TyId` -> `TypeExpr` resolution that
//! specialization substitutes through.
use super::*;
use crate::monoty::MonoTy;
use std::rc::Rc;

impl Builder<'_> {
    pub(crate) fn is_null_lit(&self, e: Idx<Expr>) -> bool {
        self.dispatch().is_null_lit(e)
    }

    /// Whether `e`'s type is `T?` (a `Nullable`, chasing `Infer` but NOT peeling
    /// the `Nullable` like `expr_ty` does). Drives the `if let` nil-guard.
    pub(crate) fn expr_is_nullable(&self, e: Idx<Expr>) -> bool {
        self.dispatch().expr_is_nullable(e)
    }

    pub(crate) fn concrete_expr_ty(&self, e: Idx<Expr>) -> Option<Rc<MonoTy>> {
        self.dispatch().concrete_expr_ty(e)
    }

    pub(crate) fn concrete_ty_id(&self, ty: TyId) -> Option<Rc<MonoTy>> {
        self.dispatch().concrete_ty(ty)
    }

    pub(crate) fn concrete_rep_id(
        &self,
        ty: &MonoTy,
        use_context: RepUse,
        tuple_spelled: bool,
        nobox: bool,
    ) -> Result<RepId, CompilerError> {
        self.scx
            .rep_arena()
            .plan(
                self.ws,
                self.scx.target(),
                ty,
                use_context,
                &RepUseFacts { tuple_spelled, span: self.cur_span },
                RepGuarantees { nobox, allow_opaque: true },
            )
            .map_err(|error| match error.span.or(self.cur_span) {
                Some(span) => CompilerError::new_with_label(span, error.message),
                None => CompilerError::new_no_location(error.message),
            })
    }

    /// Select the representation of the operation that produced `value`.
    ///
    /// Most expressions can be planned from their concrete type. Storage
    /// intercepts and identity casts are the exceptions: their body type can
    /// still be an opaque generic even though the operation has already
    /// selected a concrete element plan. Re-planning those expressions from
    /// the generic spelling would throw away the plan the producer just used.
    pub(crate) fn lowered_expr_representation(
        &self,
        e: Idx<Expr>,
        ty: &MonoTy,
        value: &ValueRepr,
    ) -> Result<RepId, CompilerError> {
        if let Some(representation) = self.operation_representation(e)? {
            return Ok(representation);
        }

        let has_presence = matches!(
            value,
            ValueRepr::Group { present: Some(_), .. }
                | ValueRepr::Tuple { present: Some(_), .. }
        );
        let ty = if has_presence && !ty.peel_nullable().1 {
            MonoTy::Nullable(Box::new(ty.clone()))
        } else {
            ty.clone()
        };
        let tuple_spelled = matches!(ty.peel_nullable().0, MonoTy::Tuple(_));
        self.concrete_rep_id(
            &ty,
            if tuple_spelled { RepUse::Return } else { RepUse::Value },
            tuple_spelled,
            self.nobox_value_name(e).is_some(),
        )
    }

    pub(crate) fn value_bundle_for_expr(
        &self,
        e: Idx<Expr>,
        value: ValueRepr,
    ) -> Result<ValueBundle, CompilerError> {
        let ty = match self.concrete_expr_ty(e) {
            Some(ty) => ty,
            None => {
                let enum_name = match self.body.expr_arena[e] {
                    Expr::EnumVariant { enum_name, .. } => Some(enum_name),
                    _ => match self.expr_ty(e) {
                        Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. })
                            if EnumPlan::for_name(self.ws, name).is_some() =>
                            Some(name),
                        _ => None,
                    },
                };
                if let Some(enum_name) = enum_name {
                    Rc::new(crate::monoty::MonoTy::App {
                        ctor: crate::monoty::MonoCtor::Nominal(enum_name),
                        params: Vec::new(),
                    })
                } else {
                    let inf = self.inf.ok_or_else(|| {
                        self.gap_here("expression has no inference context for representation planning")
                    })?;
                    let ty_id = inf.expr_types.get(e).copied().ok_or_else(|| {
                        self.gap_here("expression has no type for representation planning")
                    })?;
                    Rc::new(crate::monoty::convert(
                        self.ws,
                        inf,
                        ty_id,
                        &crate::monoty::MonoSubst::default(),
                    )
                    .map_err(|reason| {
                        self.gap_here(format!(
                            "expression has no concrete type for representation planning: {reason:?}"
                        ))
                    })?)
                }
            }
        };
        let representation = self.lowered_expr_representation(e, &ty, &value)?;
        Ok(ValueBundle::new(representation, value))
    }

    fn operation_representation(&self, e: Idx<Expr>) -> Result<Option<RepId>, CompilerError> {
        match self.body.expr_arena[e].clone() {
            Expr::Index { base, .. } => {
                if let Some(elem) = self.vec_column_elem_of(base)? {
                    return Ok(Some(elem.representation()));
                }
                if let Some(elem) = self.map_column_value_of(base)? {
                    return Ok(Some(elem.representation()));
                }
                for family in [ContainerFamily::NativeBuffer, ContainerFamily::FlatVec] {
                    if let Some(plan) = self.container_plan_for(base, family)? {
                        if plan.element_shape(&self.scx.rep_arena()).is_some() {
                            return Ok(Some(plan.element_rep()));
                        }
                    }
                }
                Ok(None)
            }
            Expr::Call { callee, args, .. } => {
                let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
                if let Callee::Unresolved { name, .. } = &callee {
                    if name.as_str() == "cast" && arg_idxs.len() == 1 {
                        if let Some(representation) = self.operation_representation(arg_idxs[0])? {
                            return Ok(Some(representation));
                        }
                    }
                    if name.as_str() == "__vec_data_get" {
                        if let Some(&recv) = arg_idxs.first()
                            && let Some(elem) = self.vec_column_elem_of(recv)?
                        {
                            return Ok(Some(elem.representation()));
                        }
                    }
                    if matches!(name.as_str(), "get" | "map_get") {
                        if let Some(&recv) = arg_idxs.first()
                            && let Some(elem) = self.map_column_value_of(recv)?
                        {
                            return Ok(Some(elem.representation()));
                        }
                    }
                    if self.resolve_funcref(e, *name).is_ok()
                        && let Some(owner) = self.concrete_callee_for_site(e)?
                    {
                        let abi = self.callee_abi_plan_for_instance(&owner)?;
                        return Ok(abi.representation().result());
                    }
                }
                match callee {
                    Callee::Local(binding) => {
                        if let Some(nested) = self.nested_fn_globals.get(&raw(binding)) {
                            return Ok(nested.representation.result());
                        }
                        let Some(ty) = self.binding_ty(binding) else { return Ok(None) };
                        Ok(self.fn_value_rep_plan(ty)?.and_then(|plan| plan.result()))
                    }
                    Callee::Value(value) => {
                        let Some(ty) = self.inf.and_then(|inf| inf.expr_types.get(value)).copied()
                        else {
                            return Ok(None);
                        };
                        Ok(self.fn_value_rep_plan(ty)?.and_then(|plan| plan.result()))
                    }
                    Callee::Unresolved { .. } => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn concrete_rep_tuple(
        &self,
        ty: &MonoTy,
        use_context: RepUse,
        tuple_spelled: bool,
        nobox: bool,
    ) -> Result<Option<TupleRetShape>, CompilerError> {
        let arena = self.scx.rep_arena();
        let id = self.concrete_rep_id(ty, use_context, tuple_spelled, nobox)?;
        Ok(arena.tuple_shape(id))
    }

    pub(crate) fn expr_rep_id(
        &self,
        e: Idx<Expr>,
        use_context: RepUse,
        nobox: bool,
    ) -> Result<Option<RepId>, CompilerError> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let tuple_spelled = matches!(ty.peel_nullable().0, MonoTy::Tuple(_));
            return self
                .concrete_rep_id(&ty, use_context, tuple_spelled, nobox)
                .map(Some);
        }
        Ok(None)
    }

    pub(crate) fn nominal_value_representation(&self, name: Ustr) -> Result<RepId, CompilerError> {
        let ty = MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params: Vec::new(),
        };
        self.concrete_rep_id(&ty, RepUse::Value, false, false)
    }

    pub(crate) fn nominal_rep_shape(
        &self,
        name: Ustr,
        nobox: bool,
    ) -> Result<Option<ValueShape>, CompilerError> {
        let ty = MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params: Vec::new(),
        };
        let id = self
            .scx
            .rep_arena()
            .plan(
                self.ws,
                self.scx.target(),
                &ty,
                RepUse::Value,
                &RepUseFacts::default(),
                RepGuarantees { nobox, allow_opaque: true },
            )
            .map_err(|error| match error.span.or(self.cur_span) {
                Some(span) => CompilerError::new_with_label(span, error.message),
                None => CompilerError::new_no_location(error.message),
            })?;
        Ok(self.scx.rep_arena().value_shape(id))
    }

    pub(crate) fn nominal_rep_shape_required(
        &self,
        name: Ustr,
        nobox: bool,
    ) -> Result<ValueShape, CompilerError> {
        let ty = MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params: Vec::new(),
        };
        let id = self.concrete_rep_id(&ty, RepUse::Value, false, nobox)?;
        self.scx
            .rep_arena()
            .value_shape(id)
            .ok_or_else(|| self.gap_here(format!("`{name}` has no planned value shape")))
    }

    pub(crate) fn rep_shape_for_ty(
        &self,
        ty: TyId,
        use_context: RepUse,
        nobox: bool,
    ) -> Result<Option<ValueShape>, CompilerError> {
        let Some(mono) = self.concrete_ty_id(ty) else { return Ok(None) };
        let id = self.concrete_rep_id(&mono, use_context, false, nobox)?;
        Ok(self.scx.rep_arena().value_shape(id))
    }

    pub(crate) fn concrete_expr_is_untyped(&self, e: Idx<Expr>) -> bool {
        self.dispatch()
            .concrete_types
            .is_some_and(|types| types.is_untyped_expr(e))
    }

    pub(crate) fn is_nullable_ty(&self, ty: TyId) -> bool {
        self.dispatch().is_nullable_ty(ty)
    }

    /// The boxed-ABI view of [`Self::spread_field_bundles`]. Field extraction has
    /// one representation-aware implementation; only the final packing differs.
    pub(crate) fn spread_field_insts(&mut self, op: Idx<Expr>) -> Result<Vec<(Ustr, Idx<Inst>)>, CompilerError> {
        let bundles = self.spread_field_bundles(op)?;
        let mut out = Vec::with_capacity(bundles.len());
        for (field, bundle) in bundles {
            out.push((field, self.pack(bundle.into_value())?));
        }
        Ok(out)
    }

    /// Extract a spread into field bundles. Every field gets the child plan from
    /// the declaring struct before a caller places it into a parameter slot.
    pub(crate) fn spread_field_bundles(&mut self, op: Idx<Expr>) -> Result<Vec<(Ustr, ValueBundle)>, CompilerError> {
        if let Expr::StructLiteral { name, fields, bases } = self.body.expr_arena[op].clone() {
            let decl = self
                .struct_field_names(name)
                .ok_or_else(|| gap(format!("spread operand `{name}` has no known fields")))?;
            let mut has_flat = false;
            for f in &decl {
                if self
                    .struct_field_plan(name, *f, false)?
                    .flat_shape()
                    .is_some()
                {
                    has_flat = true;
                    break;
                }
            }
            if !has_flat {
                let base_bundle = match bases.into_iter().next() {
                    Some(b) => Some(self.lower_expr_repr(b)?),
                    None => None,
                };
                let base_repr = base_bundle.as_ref().map(|b| b.value().clone());
                let base_boxed = match &base_repr {
                    Some(ValueRepr::Group { .. }) | None => None,
                    Some(other) => Some(self.pack(other.clone())?),
                };
                let mut explicit: Vec<(Ustr, ValueBundle)> = Vec::new();
                let mut by_value: HashMap<u32, ValueBundle> = HashMap::new();
                for fidx in fields {
                    let lf = self.body.struct_lit_field_arena[fidx];
                    let key: u32 = lf.value.into_raw().into();
                    let r = match by_value.get(&key) {
                        Some(r) => r.clone(),
                        None => {
                            let r = self.lower_expr_repr(lf.value)?;
                            by_value.insert(key, r.clone());
                            r
                        }
                    };
                    explicit.push((lf.name, r));
                }
                let mut out = Vec::new();
                for fname in decl {
                    let r = if let Some((_, r)) = explicit.iter().find(|(n, _)| *n == fname) {
                        r.clone()
                    } else {
                        let raw = self.spread_base_field_repr(&base_repr, base_boxed, name, fname)?;
                        self.bundle_struct_field(name, fname, raw, Some(op))?
                    };
                    out.push((fname, r));
                }
                return Ok(out);
            }
        }
        let base_bundle = self.lower_expr_repr(op)?;
        let base_repr = base_bundle.value().clone();
        let base_boxed = match &base_repr {
            ValueRepr::Group { .. } => None,
            other => Some(self.pack(other.clone())?),
        };
        let name = match self.expr_ty(op) {
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) => name,
            _ => return Err(gap("spread operand is not a nominal struct")),
        };
        let fields = self
            .struct_field_names(name)
            .ok_or_else(|| gap(format!("spread operand `{name}` has no known fields")))?;
        let origin = Some(op);
        let mut out = Vec::new();
        for fname in fields {
            let r = self.spread_base_field_repr(&Some(base_repr.clone()), base_boxed, name, fname)?;
            out.push((fname, self.bundle_struct_field(name, fname, r, origin)?));
        }
        Ok(out)
    }

    fn bundle_struct_field(
        &self,
        sname: Ustr,
        field: Ustr,
        value: ValueRepr,
        origin: Option<Idx<Expr>>,
    ) -> Result<ValueBundle, CompilerError> {
        let representation = self.struct_field_representation(sname, field, origin)?;
        Ok(ValueBundle::new(representation, value))
    }

    fn struct_field_representation(
        &self,
        sname: Ustr,
        field: Ustr,
        origin: Option<Idx<Expr>>,
    ) -> Result<RepId, CompilerError> {
        let sid = *self
            .ws
            .struct_by_name
            .get(&sname)
            .ok_or_else(|| gap(format!("spread struct `{sname}` is not declared")))?;
        let fdef = self.ws.modules[sid.module.index()].structs[sid.def]
            .fields
            .iter()
            .find(|f| f.name == field)
            .ok_or_else(|| gap(format!("spread field `{sname}.{field}` is not declared")))?;
        let ty = origin
            .and_then(|literal| {
                self.dispatch()
                    .concrete_types
                    .and_then(|tables| tables.struct_field_type(literal, field))
                    .map(|id| self.concrete_mono_view(id, ConcreteMonoConsumer::Dispatch))
            });
        let ty = match ty {
            Some(ty) => Some(ty),
            None => match origin.and_then(|subject| self.concrete_expr_ty(subject)) {
                Some(ty)
                    if matches!(
                        ty.as_ref(),
                        MonoTy::App {
                            ctor: crate::monoty::MonoCtor::Nominal(base),
                            ..
                        } if *base == sname
                    ) => {
                        let MonoTy::App { params, .. } = ty.as_ref() else {
                            unreachable!()
                        };
                        let def = &self.ws.modules[sid.module.index()].structs[sid.def];
                        if def.generics.len() != params.len() {
                            return Err(gap(format!(
                                "spread struct `{sname}` has {} generic arguments for {} parameters",
                                params.len(),
                                def.generics.len()
                            )));
                        }
                        let mut subst = crate::monoty::MonoSubst::default();
                        for (generic, param) in def.generics.iter().zip(params) {
                            let TypeExpr::Generic(var, _) = self.dispatch().raw_ty_node(*generic)
                                .ok_or_else(|| gap("spread struct generic parameter has no type node"))?
                            else {
                                return Err(gap(format!(
                                    "spread struct `{sname}` has a non-generic parameter slot"
                                )));
                            };
                            subst.insert(var, param.clone());
                        }
                        Some(Rc::new(crate::monoty::convert_decl(self.ws, fdef.ty, &subst).map_err(|reason| {
                            gap(format!(
                                "spread field `{sname}.{field}` has no concrete type plan: {reason:?}"
                            ))
                        })?))
                    }
                Some(other) => {
                    return Err(gap(format!(
                        "spread operand for `{sname}.{field}` has type `{other:?}`"
                    )));
                }
                None => self.concrete_ty_id(fdef.ty),
            },
        };
        let ty = match ty {
            Some(ty) => ty,
            None => Rc::new(
                crate::monoty::convert_decl(
                    self.ws,
                    fdef.ty,
                    &crate::monoty::MonoSubst::default(),
                )
                .map_err(|reason| {
                    self.gap_here(format!(
                        "spread field `{sname}.{field}` has no concrete type plan: {reason:?}"
                    ))
                })?,
            ),
        };
        let tuple_spelled = matches!(ty.peel_nullable().0, MonoTy::Tuple(_));
        let nobox = matches!(
            ty.peel_nullable().0,
            MonoTy::App { ctor: crate::monoty::MonoCtor::Nominal(name), .. }
                if crate::lower::is_nobox_nominal(self.ws, *name)
        );
        self.concrete_rep_id(&ty, RepUse::Value, tuple_spelled, nobox)
    }

    /// Read field `fname` from an already-lowered spread base: project a Group
    /// part, or read the field off the packed cdata; a missing base falls back
    /// to the struct default.
    pub(crate) fn spread_base_field_repr(
        &mut self,
        base_repr: &Option<ValueRepr>,
        base_boxed: Option<Idx<Inst>>,
        name: Ustr,
        fname: Ustr,
    ) -> Result<ValueRepr, CompilerError> {
        if let Some(ValueRepr::Group { name: bn, parts, .. }) = base_repr {
            let i = self.field_index(*bn, fname).ok_or_else(|| gap("spread field"))?;
            return Ok(parts[i].clone());
        }
        let field_plan = self.struct_field_plan(name, fname, false)?;
        if let Some(shape) = field_plan.flat_shape() {
            let cells = match (base_repr, base_boxed) {
                (Some(_), Some(parent)) => self.load_flat_cells(parent, fname, &shape),
                (None, None) => {
                    let value = self.lower_struct_default(name, fname)?;
                    self.spread_maybe_null_box_to_cells(value, &shape)?
                }
                _ => return Err(gap("flat spread field has no physical parent")),
            };
            return Ok(self.flat_cells_to_repr(&shape, cells));
        }
        if let (Some((cname, nullable)), Some(parent)) =
            (field_plan.embed(), base_boxed)
        {
            return self.load_embed_repr(parent, fname, cname, nullable);
        }
        Ok(match (base_repr, base_boxed) {
            (Some(_), Some(bi)) => {
                ValueRepr::Scalar(self.emit_cur(Inst::Field { base: bi, field: fname }))
            }
            (None, None) => ValueRepr::Scalar(self.lower_struct_default(name, fname)?),
            _ => return Err(gap("spread field has no physical base representation")),
        })
    }

    /// The resolved type of an expression (chasing `Infer` vars + peeling
    /// `Nullable`), or `None` when inference is absent/unsolved.
    pub(crate) fn expr_ty(&self, e: Idx<Expr>) -> Option<TypeExpr> {
        self.dispatch().expr_ty(e)
    }

    pub(crate) fn current_expr_storage_ty_id(&self) -> Option<TyId> {
        let ty = self.cur_expr.and_then(|e| self.inf?.expr_types.get(e).copied())?;
        let resolved = self.resolve_tyid(ty)?;
        if matches!(
            self.dispatch().ty_node(resolved),
            Some(TypeExpr::Builtin(BuiltinTy::Never | BuiltinTy::Error))
        ) {
            None
        } else {
            Some(ty)
        }
    }

    pub(crate) fn tuple_opaque_slot_types(
        &self,
        e: Idx<Expr>,
        shape: &TupleRetShape,
    ) -> Vec<Option<TyId>> {
        fn append(
            b: &Builder<'_>,
            ty: TyId,
            shape: &TupleRetShape,
            out: &mut Vec<Option<TyId>>,
        ) -> Option<()> {
            if shape.nullable {
                out.push(None);
            }
            let ty = b.resolve_tyid(ty)?;
            let ty = match b.dispatch().ty_node(ty)? {
                TypeExpr::Nullable(inner) => ty.with_idx(inner),
                _ => ty,
            };
            let TypeExpr::Tuple { elements } = b.dispatch().ty_node(ty)? else {
                return None;
            };
            if elements.len() != shape.slots.len() {
                return None;
            }
            for (element, slot) in elements.into_iter().zip(&shape.slots) {
                let element = ty.with_idx(element);
                match slot {
                    RetSlot::Opaque => out.push(Some(element)),
                    RetSlot::Leaves(value) => out.extend((0..value.leaf_count()).map(|_| None)),
                    RetSlot::Nested(inner) => append(b, element, inner, out)?,
                }
            }
            Some(())
        }

        let Some(ty) = self.inf.and_then(|inf| inf.expr_types.get(e).copied()) else {
            return vec![None; shape.slot_count()];
        };
        let mut out = Vec::with_capacity(shape.slot_count());
        if append(self, ty, shape, &mut out).is_some() && out.len() == shape.slot_count() {
            out
        } else {
            vec![None; shape.slot_count()]
        }
    }

    /// `Some` iff `e`'s concrete or inferred type is a (non-nullable) tuple. Used
    /// to route a tuple result across a call boundary (the call expr's own type
    /// IS the callee's return type, so direct and INDIRECT calls agree).
    /// `resolve_tyid` (not `resolve_ty`) so a `Nullable(Tuple)` doesn't peel to a
    /// bare tuple and get mis-splatted.
    pub(crate) fn expr_tuple_shape(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<TupleRetShape>, CompilerError> {
        // A bound `next` answers from its DECLARATION, never from the type this
        // body's single generic check left on the call: the destructure that
        // consumes the shape and the call that emits it must read the same
        // oracle, or the binding takes a different number of leaves than the
        // call returns and the difference is made up with a constant.
        if let Some(&gidx) = self.inf.and_then(|inf| inf.bound_calls.get(&e)) {
            return self.bound_next_tuple_shape(gidx).map(Some);
        }
        if let Some(ty) = self.concrete_expr_ty(e) {
            if matches!(&*ty, MonoTy::Tuple(_)) {
                return self.concrete_rep_tuple(&ty, RepUse::Return, true, false);
            }
        }
        Ok(None)
    }

    pub(crate) fn repr_join_slot(&self, e: Idx<Expr>) -> Result<Option<RetSlot>, CompilerError> {
        if let Some(shape) = self.expr_tuple_shape(e)?.filter(TupleRetShape::splats) {
            return Ok(Some(RetSlot::Nested(shape)));
        }
        // A `?.` yielding a TUPLE. `expr_tuple_shape` declines a `Nullable(Tuple)`
        // on purpose -- it is the CALL-boundary oracle, and no callee puts that
        // presence on the wire. This join does: it mints the tag itself, exactly
        // as the arm-widening below mints one for a value.
        if let Some(shape) = self.nullable_tuple_wire_shape(e)? {
            return Ok(Some(RetSlot::Nested(shape)));
        }
        let Some(mut shape) = self.nobox_value_shape_of(e)? else { return Ok(None) };
        // A join's shape must represent every value that reaches it, and the
        // arms are right here. `if c { P { .. } } else { null }` infers as plain
        // `P` -- the `null` arm does not widen it -- so reading nullability off
        // the join EXPRESSION alone plans a shape with no presence leaf, and the
        // null arm then splits nil into fields and faults. Widening from the arms
        // cannot manufacture a phantom presence slot the way keying on a foreign
        // expression's nullability would: an arm that can be null is a value the
        // join has to carry, and if the sink is non-nullable bone has already
        // rejected the program.
        if !shape.nullable && self.join_arm_is_nullable(e) {
            shape.nullable = true;
        }
        Ok(Some(RetSlot::Leaves(shape)))
    }

    /// Whether any arm of a join expression can produce `null`. Arms are blocks,
    /// so each is peeled to its tail value before asking.
    fn join_arm_is_nullable(&self, e: Idx<Expr>) -> bool {
        let arms: Vec<Idx<Expr>> = match &self.body.expr_arena[e] {
            Expr::If { then_branch, else_branch, .. } => {
                std::iter::once(*then_branch).chain(*else_branch).collect()
            }
            Expr::Match { arms, .. } => {
                arms.clone().map(|a| self.body.match_arm_arena[a].body).collect()
            }
            _ => return false,
        };
        arms.into_iter().any(|a| {
            let v = self.peel_block_tail(a);
            self.is_null_lit(v) || self.expr_is_nullable(v)
        })
    }

    /// A block arm's value is its tail; anything else is its own value.
    fn peel_block_tail(&self, e: Idx<Expr>) -> Idx<Expr> {
        match &self.body.expr_arena[e] {
            Expr::Block { tail: Some(t), .. } => self.peel_block_tail(*t),
            _ => e,
        }
    }

    /// The slot layout of a tuple's BOX: `Some` exactly when some element splats,
    /// so its `nobox` leaves ride the box instead of a `<name>_ct` inside it.
    ///
    /// The SAME `TupleRetShape` the multi-return wire uses -- a boxed tuple is
    /// that wire written into a table rather than into registers. Producer
    /// ([`Expr::Tuple`]) and every reader (slot projection, destructure) derive
    /// the layout here and nowhere else, so they cannot disagree about how many
    /// table slots an element occupies. That disagreement is silent, not a
    /// rejection: Lua reads a missing slot as nil.
    ///
    /// `None` for every tuple that has no `nobox` element -- which is every tuple
    /// that compiled before this existed, so their boxes are byte-identical.
    pub(crate) fn tuple_box_shape(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<TupleRetShape>, CompilerError> {
        Ok(self.expr_tuple_shape(e)?.filter(|s| s.splats()))
    }

    /// The full `(bool, E)` shape of a placeholder `next` on the generic at
    /// `gidx` from the concrete bound plan.
    pub(crate) fn bound_next_tuple_shape(
        &self,
        gidx: usize,
    ) -> Result<TupleRetShape, CompilerError> {
        let next = self.concrete_plan.bound_tree.get(gidx).ok_or_else(|| {
            gap(format!("concrete plan is missing bound `next` witness {gidx}"))
        })?;
        let instance = match next {
            crate::bound::ResolvedBoundNext::Concrete { instance, .. } => instance.clone(),
            crate::bound::ResolvedBoundNext::CallerGeneric(inner) => {
                return Err(gap(format!(
                    "concrete bound `next` witness {gidx} remains caller-generic at {inner}"
                )))
            }
        };
        let shape = self
            .callee_abi_plan_for_instance(&instance)?
            .ret_tuple()
            .ok_or_else(|| gap("concrete bound `next` must return a tuple"))?;
        if shape.slots.len() < 2 {
            return Err(gap("concrete bound `next` must return `(bool, E)`"));
        }
        Ok(shape)
    }

    /// The C-array element type name for a `native_buffer_alloc<T>()` call whose
    /// result type is `NativeBuffer<T>` -- an FFI-clean value struct's cdef name,
    /// or `int`/`float`/`bool`. A GC-carrying value struct, an unresolved (opaque
    /// generic `T`), or any other element rejects with a spanned emit-time error:
    /// the intrinsic can only emit `ffi.new("<ctype>[?]")` when the concrete
    /// element type is known here. `span` labels the call expression.
    /// The `(field, view)` pairs a `NativeBuffer` of element `ctype` carries --
    /// one typed leaf view per distinct primitive C type among its leaves.
    /// Empty for a primitive or non-leaf-addressable element, which keeps those
    /// buffers exactly as they are today.
    ///
    /// Attached at CONSTRUCTION so the cast happens once per buffer. Casting per
    /// access would allocate a pointer cdata each time, which is the allocation
    /// leaf addressing exists to remove.
    pub(crate) fn leaf_view_fields(&mut self, ctype: Ustr, ptr: Idx<Inst>) -> Vec<(Ustr, Idx<Inst>)> {
        let Ok(ctys) = self.scx.rep_arena().nominal_leaf_view_ctys(
            self.ws,
            self.scx.target,
            ctype,
        ) else {
            return Vec::new();
        };
        ctys.into_iter()
            .map(|cty| {
                let view = self.emit_cur(Inst::FfiViewCast { cty: Ustr::from(cty), ptr });
                (leaf_view_field(cty), view)
            })
            .collect()
    }

    pub(crate) fn native_buffer_alloc_ctype(
        &self,
        e: Idx<Expr>,
        span: Span,
    ) -> Result<(Ustr, Option<crate::lower::RepId>), CompilerError> {
        let ret = self.concrete_expr_ty(e).ok_or_else(|| {
            CompilerError::new_with_label(
                span,
                "`native_buffer_alloc` call is missing from the concrete type plan".to_string(),
            )
        })?;
        let MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(nb),
            params,
        } = ret.peel_nullable().0
        else {
            return Err(CompilerError::new_with_label(
                span,
                "`native_buffer_alloc` must produce a `NativeBuffer<T>`".to_string(),
            ));
        };
        if nb.as_str() != "NativeBuffer" {
            return Err(CompilerError::new_with_label(
                span,
                format!("`native_buffer_alloc` result is `{nb}`, expected `NativeBuffer<T>`"),
            ));
        }
        let Some(element) = params.first() else {
            return Err(CompilerError::new_with_label(
                span,
                "`NativeBuffer` element type is missing".to_string(),
            ));
        };
        self.scx
            .rep_arena()
            .native_buffer_element_ctype(self.ws, self.scx.target, element)
            .map_err(|error| CompilerError::new_with_label(span, error.message))
    }

    /// `Some` iff `e`'s type is a tuple OR a `(tuple)?` -- the nil-peeled form
    /// for safe-nav calls, whose result type is nullable but whose callee still
    /// splats the inner tuple.
    pub(crate) fn peeled_tuple_shape(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<TupleRetShape>, CompilerError> {
        let Some(concrete) = self.concrete_expr_ty(e) else { return Ok(None) };
        let (payload, _) = concrete.peel_nullable();
        match payload {
            MonoTy::Tuple(_) => self.concrete_rep_tuple(&concrete, RepUse::Return, true, false),
            _ => Ok(None),
        }
    }

    pub(crate) fn resolve_ty(&self, ty: TyId) -> Option<TypeExpr> {
        self.dispatch().resolve_ty(ty)
    }

    pub(crate) fn ty_node(&self, ty: TyId) -> Option<TypeExpr> {
        self.dispatch().ty_node(ty)
    }

    /// The final `TyId` after chasing `Infer` substitutions (NOT peeling
    /// `Nullable` -- unlike `resolve_ty`, this keeps the `TyId` so its App params
    /// can be indexed in the correct arena via `with_idx`).
    pub(crate) fn resolve_tyid(&self, ty: TyId) -> Option<TyId> {
        self.dispatch().resolve_tyid(ty)
    }

    /// The `EnumFlatShape` of `Y` when `e`'s type is `Coroutine<Y, ..>` and `Y` is
    /// a flat enum -- the shape flowing through a `await` delegation to a flat-enum
    /// coroutine (so the delegate threads the inner's leaves instead of a box).
    pub(crate) fn coroutine_yield_flat_shape(&self, e: Idx<Expr>) -> Option<EnumFlatShape> {
        let concrete = self.concrete_expr_ty(e)?;
        let MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params,
        } = concrete.peel_nullable().0
        else {
            return None;
        };
        if name.as_str() != "Coroutine" {
            return None;
        }
        let Some(MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(yield_name),
            ..
        }) = params.first()
        else {
            return None;
        };
        EnumPlan::for_name(self.ws, *yield_name).and_then(|plan| plan.flat().cloned())
    }
}
