//! Type and shape queries over the body being lowered: nullability, tuple and
//! return shapes, spread operands, and the `TyId` -> `TypeExpr` resolution that
//! specialization substitutes through.
use super::*;
use crate::monoty::MonoTy;

impl<'a> Builder<'a> {
    pub(crate) fn is_null_lit(&self, e: Idx<Expr>) -> bool {
        self.dispatch().is_null_lit(e)
    }

    /// Whether `e`'s type is `T?` (a `Nullable`, chasing `Infer` but NOT peeling
    /// the `Nullable` like `expr_ty` does). Drives the `if let` nil-guard.
    pub(crate) fn expr_is_nullable(&self, e: Idx<Expr>) -> bool {
        self.dispatch().expr_is_nullable(e)
    }
}

/// Concrete type reads. The types live in the build's interner, so the
/// borrows outlive any borrow of the builder itself.
impl<'a> Builder<'a> {
    pub(crate) fn concrete_mono(&self, id: crate::monoty::MonoTyId) -> &'a MonoTy {
        let scx: &'a ScalarCtx = self.scx;
        scx.interner().get_ref(id)
    }

    pub(crate) fn concrete_expr_ty(&self, e: Idx<Expr>) -> Option<&'a MonoTy> {
        let plan: &'a ConcreteBodyPlan = self.concrete_plan;
        let id = plan.domain_types(&self.cur_domain)?.expr_type(e)?;
        Some(self.concrete_mono(id))
    }

    pub(crate) fn concrete_struct_field_ty(
        &self,
        literal: Idx<Expr>,
        field: Ustr,
    ) -> Option<&'a MonoTy> {
        let plan: &'a ConcreteBodyPlan = self.concrete_plan;
        let id = plan
            .domain_types(&self.cur_domain)?
            .struct_field_type(literal, field)?;
        Some(self.concrete_mono(id))
    }

    pub(crate) fn concrete_ty_id(&self, ty: TyId) -> Option<&'a MonoTy> {
        let plan: &'a ConcreteBodyPlan = self.concrete_plan;
        let id = plan.domain_types(&self.cur_domain)?.ty_type(ty)?;
        Some(self.concrete_mono(id))
    }
}

impl<'a> Builder<'a> {
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
                &RepUseFacts {
                    tuple_spelled,
                    span: self.cur_span,
                },
                RepGuarantees {
                    nobox,
                    allow_opaque: true,
                },
            )
            .map_err(|error| self.rep_error(error))
    }

    /// Plan ordinary expressions from their concrete type and presence carrier.
    pub(crate) fn lowered_expr_representation(
        &self,
        e: Idx<Expr>,
        ty: &MonoTy,
        value: &ValueRepr,
    ) -> Result<RepId, CompilerError> {
        let has_presence = matches!(
            value,
            ValueRepr::Group {
                present: Some(_),
                ..
            } | ValueRepr::Tuple {
                present: Some(_),
                ..
            }
        );
        let ty = if has_presence && !ty.peel_nullable().1 {
            std::borrow::Cow::Owned(MonoTy::Nullable(Box::new(ty.clone())))
        } else {
            std::borrow::Cow::Borrowed(ty)
        };
        let tuple_spelled = matches!(ty.peel_nullable().0, MonoTy::Tuple(_));
        self.concrete_rep_id(
            &ty,
            if tuple_spelled {
                RepUse::Return
            } else {
                RepUse::Value
            },
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
            Some(ty) => std::borrow::Cow::Borrowed(ty),
            None => {
                let enum_name = match self.body.expr_arena[e] {
                    Expr::EnumVariant { enum_name, .. } => Some(enum_name),
                    _ => match self.expr_ty(e) {
                        Some(TypeExpr::App {
                            ctor: TypeCtor::Nominal(name),
                            ..
                        }) if EnumPlan::for_name(self.ws, name).is_some() => Some(name),
                        _ => None,
                    },
                };
                if let Some(enum_name) = enum_name {
                    std::borrow::Cow::Owned(crate::monoty::MonoTy::App {
                        ctor: crate::monoty::MonoCtor::Nominal(enum_name),
                        params: Vec::new(),
                    })
                } else {
                    let inf = self.inf.ok_or_else(|| {
                        self.gap_here(
                            "expression has no inference context for representation planning",
                        )
                    })?;
                    let ty_id = inf.expr_types.get(e).copied().ok_or_else(|| {
                        self.gap_here("expression has no type for representation planning")
                    })?;
                    std::borrow::Cow::Owned(crate::monoty::convert(
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

    pub(crate) fn concrete_rep_tuple(
        &self,
        ty: &MonoTy,
        use_context: RepUse,
        tuple_spelled: bool,
        nobox: bool,
    ) -> Result<Option<TupleLayout>, CompilerError> {
        let arena = self.scx.rep_arena();
        let id = self.concrete_rep_id(ty, use_context, tuple_spelled, nobox)?;
        Ok(arena.tuple_shape(id).cloned())
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
                .concrete_rep_id(ty, use_context, tuple_spelled, nobox)
                .map(Some);
        }
        Ok(None)
    }

    fn nominal_rep_id(&self, name: Ustr, nobox: bool) -> Result<RepId, CompilerError> {
        let ty = MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params: Vec::new(),
        };
        self.concrete_rep_id(&ty, RepUse::Value, false, nobox)
    }

    pub(crate) fn nominal_value_representation(&self, name: Ustr) -> Result<RepId, CompilerError> {
        self.nominal_rep_id(name, false)
    }

    pub(crate) fn nominal_rep_shape(
        &self,
        name: Ustr,
        nobox: bool,
    ) -> Result<Option<ValueShape>, CompilerError> {
        let id = self.nominal_rep_id(name, nobox)?;
        Ok(self.scx.rep_arena().value_shape(id).cloned())
    }

    pub(crate) fn nominal_rep_shape_required(
        &self,
        name: Ustr,
        nobox: bool,
    ) -> Result<ValueShape, CompilerError> {
        self.nominal_rep_shape(name, nobox)?
            .ok_or_else(|| self.gap_here(format!("`{name}` has no planned value shape")))
    }

    pub(crate) fn rep_shape_for_ty(
        &self,
        ty: TyId,
        use_context: RepUse,
        nobox: bool,
    ) -> Result<Option<ValueShape>, CompilerError> {
        let Some(mono) = self.concrete_ty_id(ty) else {
            return Ok(None);
        };
        let id = self.concrete_rep_id(mono, use_context, false, nobox)?;
        Ok(self.scx.rep_arena().value_shape(id).cloned())
    }

    pub(crate) fn concrete_expr_is_untyped(&self, e: Idx<Expr>) -> bool {
        self.dispatch()
            .concrete_types
            .is_some_and(|types| types.is_untyped_expr(e))
    }

    pub(crate) fn is_nullable_ty(&self, ty: TyId) -> bool {
        self.dispatch().is_nullable_ty(ty)
    }

    /// Extract a spread into field bundles. Every field gets the child plan from
    /// the declaring struct before a caller places it into a parameter slot.
    pub(crate) fn spread_field_bundles(
        &mut self,
        op: Idx<Expr>,
    ) -> Result<Vec<(Ustr, ValueBundle)>, CompilerError> {
        if let Expr::StructLiteral {
            name,
            fields,
            bases,
        } = self.body.expr_arena[op]
        {
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
                let base = bases.into_iter().next();
                let base_bundle = match base {
                    Some(b) => Some(self.lower_expr_repr(b)?),
                    None => None,
                };
                let base_repr = base_bundle.as_ref().map(|b| b.value().clone());
                let base_boxed = match &base_repr {
                    Some(ValueRepr::Group { .. }) | None => None,
                    Some(other) => Some(self.pack(other.clone())?),
                };
                let mut explicit: Vec<(Ustr, ValueBundle)> = Vec::new();
                let mut by_value: HashMap<u32, ValueBundle> = HashMap::default();
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
                        let raw =
                            self.spread_base_field_repr(&base_repr, base_boxed, name, fname, base)?;
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
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) => name,
            _ => return Err(gap("spread operand is not a nominal struct")),
        };
        let fields = self
            .struct_field_names(name)
            .ok_or_else(|| gap(format!("spread operand `{name}` has no known fields")))?;
        let origin = Some(op);
        let mut out = Vec::new();
        for fname in fields {
            let r = self.spread_base_field_repr(
                &Some(base_repr.clone()),
                base_boxed,
                name,
                fname,
                origin,
            )?;
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

    pub(crate) fn struct_field_mono_ty(
        &self,
        sname: Ustr,
        field: Ustr,
        origin: Option<Idx<Expr>>,
    ) -> Result<std::borrow::Cow<'a, MonoTy>, CompilerError> {
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
        let ty = origin.and_then(|literal| {
            self.dispatch()
                .concrete_types
                .and_then(|tables| tables.struct_field_type(literal, field))
                .map(|id| self.concrete_mono(id))
        });
        let ty = match ty {
            Some(ty) => Some(std::borrow::Cow::Borrowed(ty)),
            None => match origin.and_then(|subject| self.concrete_expr_ty(subject)) {
                Some(owner) => Some(self.struct_field_ty_in(sname, field, owner)?),
                None => self.concrete_ty_id(fdef.ty).map(std::borrow::Cow::Borrowed),
            },
        };
        Ok(match ty {
            Some(ty) => ty,
            None => std::borrow::Cow::Owned(
                crate::monoty::convert_decl(self.ws, fdef.ty, &crate::monoty::MonoSubst::default())
                    .map_err(|reason| {
                        self.gap_here(format!(
                            "spread field `{sname}.{field}` has no concrete type plan: {reason:?}"
                        ))
                    })?,
            ),
        })
    }

    /// The type of `sname.field` in a value of concrete type `owner`
    /// (`sname<..>`, possibly nullable): the declaration with the owner's
    /// generic arguments substituted.
    pub(crate) fn struct_field_ty_in(
        &self,
        sname: Ustr,
        field: Ustr,
        owner: &MonoTy,
    ) -> Result<std::borrow::Cow<'a, MonoTy>, CompilerError> {
        let sid = *self
            .ws
            .struct_by_name
            .get(&sname)
            .ok_or_else(|| gap(format!("struct `{sname}` is not declared")))?;
        let def = &self.ws.modules[sid.module.index()].structs[sid.def];
        let fdef = def
            .fields
            .iter()
            .find(|f| f.name == field)
            .ok_or_else(|| gap(format!("field `{sname}.{field}` is not declared")))?;
        let MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(base),
            params,
        } = owner.peel_nullable().0
        else {
            return Err(gap(format!("a `{sname}` value has type `{owner:?}`")));
        };
        if *base != sname {
            return Err(gap(format!("a `{sname}` value has type `{owner:?}`")));
        }
        if def.generics.len() != params.len() {
            return Err(gap(format!(
                "struct `{sname}` has {} generic arguments for {} parameters",
                params.len(),
                def.generics.len()
            )));
        }
        let mut subst = crate::monoty::MonoSubst::default();
        for (generic, param) in def.generics.iter().zip(params) {
            let TypeExpr::Generic(var, _) = self
                .dispatch()
                .raw_ty_node(*generic)
                .ok_or_else(|| gap("struct generic parameter has no type node"))?
            else {
                return Err(gap(format!(
                    "struct `{sname}` has a non-generic parameter slot"
                )));
            };
            subst.insert(var, param.clone());
        }
        crate::monoty::convert_decl(self.ws, fdef.ty, &subst)
            .map(std::borrow::Cow::Owned)
            .map_err(|reason| {
                gap(format!(
                    "field `{sname}.{field}` has no concrete type plan: {reason:?}"
                ))
            })
    }

    fn struct_field_representation(
        &self,
        sname: Ustr,
        field: Ustr,
        origin: Option<Idx<Expr>>,
    ) -> Result<RepId, CompilerError> {
        let ty = self.struct_field_mono_ty(sname, field, origin)?;
        let tuple_spelled = matches!(ty.peel_nullable().0, MonoTy::Tuple(_));
        let nobox = matches!(
            ty.peel_nullable().0,
            MonoTy::App { ctor: crate::monoty::MonoCtor::Nominal(name), .. }
                if crate::lower::is_nobox_nominal(self.ws, *name)
        );
        self.concrete_rep_id(&ty, RepUse::Value, tuple_spelled, nobox)
    }

    pub(crate) fn spread_base_field_repr(
        &mut self,
        base_repr: &Option<ValueRepr>,
        base_boxed: Option<Idx<Inst>>,
        name: Ustr,
        fname: Ustr,
        origin: Option<Idx<Expr>>,
    ) -> Result<ValueRepr, CompilerError> {
        if let Some(ValueRepr::Group {
            name: bn, parts, ..
        }) = base_repr
        {
            let i = self
                .field_index(*bn, fname)
                .ok_or_else(|| gap("spread field"))?;
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
        if let (Some((cname, nullable)), Some(parent)) = (field_plan.embed(), base_boxed) {
            return self.load_embed_repr(parent, fname, cname, nullable);
        }
        Ok(match (base_repr, base_boxed) {
            // The base keeps its own value fields.
            (Some(_), Some(bi)) => {
                let v = self.read_field(bi, fname);
                let ty = self.struct_field_mono_ty(name, fname, origin)?;
                self.copied_value_repr(&ty, v)?
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

    /// Field lookup sees through a reference to its referent.
    pub(crate) fn field_base_ty(&self, base: Idx<Expr>) -> Option<TypeExpr> {
        let dispatch = self.dispatch();
        let mut ty = *self.inf?.expr_types.get(base)?;
        loop {
            match dispatch.ty_node(ty)? {
                TypeExpr::Infer(var, _) => ty = self.inf?.substitutions.get(var)?,
                TypeExpr::Nullable(inner) => ty = ty.with_idx(inner),
                TypeExpr::App {
                    ctor: TypeCtor::Ref,
                    params,
                } => {
                    return dispatch.resolve_ty(ty.with_idx(params.into_iter().next()?));
                }
                other => return Some(other),
            }
        }
    }

    pub(crate) fn current_expr_storage_ty_id(&self) -> Option<TyId> {
        let ty = self
            .cur_expr
            .and_then(|e| self.inf?.expr_types.get(e).copied())?;
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

    pub(crate) fn current_expr_unit_storage_ty_id(&self) -> Option<TyId> {
        let ty = self
            .cur_expr
            .and_then(|e| self.inf?.expr_types.get(e).copied())?;
        let resolved = self.resolve_tyid(ty)?;
        matches!(
            self.dispatch().ty_node(resolved),
            Some(TypeExpr::Builtin(BuiltinTy::Unit))
        )
        .then_some(ty)
    }

    pub(crate) fn tuple_opaque_slot_types(
        &self,
        e: Idx<Expr>,
        shape: &TupleLayout,
    ) -> Vec<Option<TyId>> {
        fn append(
            b: &Builder<'_>,
            ty: TyId,
            shape: &TupleLayout,
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
                match b.scx.rep_arena().layout(*slot) {
                    RepLayout::Opaque => out.push(Some(element)),
                    RepLayout::Leaves(value) => out.extend((0..value.leaf_count()).map(|_| None)),
                    RepLayout::Tuple(inner) => append(b, element, inner, out)?,
                }
            }
            Some(())
        }

        let Some(ty) = self.inf.and_then(|inf| inf.expr_types.get(e).copied()) else {
            return vec![None; shape.slot_count(self.scx.rep_arena())];
        };
        let mut out = Vec::with_capacity(shape.slot_count(self.scx.rep_arena()));
        if append(self, ty, shape, &mut out).is_some()
            && out.len() == shape.slot_count(self.scx.rep_arena())
        {
            out
        } else {
            vec![None; shape.slot_count(self.scx.rep_arena())]
        }
    }

    pub(crate) fn expr_tuple_shape(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<TupleLayout>, CompilerError> {
        // Use the bound declaration's return ABI instead of the generic call type.
        if let Some(&gidx) = self.inf.and_then(|inf| inf.bound_calls.get(&e)) {
            return self.bound_next_tuple_shape(gidx).map(Some);
        }
        if let Some(ty) = self.concrete_expr_ty(e) {
            if matches!(ty, MonoTy::Tuple(_)) {
                return self.concrete_rep_tuple(ty, RepUse::Return, true, false);
            }
        }
        Ok(None)
    }

    pub(crate) fn repr_join_slot(&self, e: Idx<Expr>) -> Result<Option<RepLayout>, CompilerError> {
        if let Some(shape) = self
            .expr_tuple_shape(e)?
            .filter(|shape| shape.splats(self.scx.rep_arena()))
        {
            return Ok(Some(RepLayout::Tuple(shape)));
        }
        // A join can keep a nullable tuple's presence slot.
        if let Some(shape) = self.nullable_tuple_wire_shape(e)? {
            return Ok(Some(RepLayout::Tuple(shape)));
        }
        if let Some(shape) = self.flat_enum_value_shape_of(e)? {
            let non_null = self
                .concrete_expr_ty(e)
                .is_some_and(|ty| !matches!(*ty, MonoTy::Nullable(_)));
            if !self.scx.native_flat_enums && non_null {
                return Ok(Some(RepLayout::Leaves(shape)));
            }
        }
        let shape = match self
            .nobox_value_shape_of(e)?
            .or(self.nullable_value_shape_of(e)?)
        {
            Some(shape) => shape,
            None => {
                let Some(name) = self.value_struct_of(e) else {
                    return Ok(None);
                };
                if self.scx.values_first_class || !planned_value_contains_flat_enum(self.ws, name) {
                    return Ok(None);
                }
                let Some(rep) = self.expr_rep_id(e, RepUse::Value, false)? else {
                    return Ok(None);
                };
                let Some(shape) = self.scx.rep_arena().value_shape(rep).cloned() else {
                    return Ok(None);
                };
                shape
            }
        };
        Ok(Some(RepLayout::Leaves(shape)))
    }

    /// A boxed tuple can carry `nobox` fields as flattened slots.
    pub(crate) fn tuple_box_shape(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<TupleLayout>, CompilerError> {
        Ok(self
            .expr_tuple_shape(e)?
            .filter(|s| s.splats(self.scx.rep_arena())))
    }

    /// The full `(bool, E)` shape of a placeholder `next` on the generic at
    /// `gidx` from the concrete bound plan.
    pub(crate) fn bound_next_tuple_shape(&self, gidx: usize) -> Result<TupleLayout, CompilerError> {
        let next = self.concrete_plan.bound_tree.get(gidx).ok_or_else(|| {
            gap(format!(
                "concrete plan is missing bound `next` witness {gidx}"
            ))
        })?;
        let instance = match next {
            crate::bound::ResolvedBoundNext::Concrete { instance, .. } => instance.clone(),
            crate::bound::ResolvedBoundNext::CallerGeneric(inner) => {
                return Err(gap(format!(
                    "concrete bound `next` witness {gidx} remains caller-generic at {inner}"
                )));
            }
        };
        let shape = self
            .callee_abi_plan_for_instance(&instance)?
            .ret_tuple()
            .cloned()
            .ok_or_else(|| gap("concrete bound `next` must return a tuple"))?;
        if shape.slots.len() < 2 {
            return Err(gap("concrete bound `next` must return `(bool, E)`"));
        }
        Ok(shape)
    }

    /// Derive the FFI element type from the allocation's concrete result type.
    pub(crate) fn leaf_view_fields(
        &mut self,
        ctype: Ustr,
        ptr: Idx<Inst>,
    ) -> Vec<(Ustr, Idx<Inst>)> {
        let Ok(ctys) = self
            .scx
            .rep_arena()
            .nominal_leaf_view_ctys(self.ws, self.scx.target, ctype)
        else {
            return Vec::new();
        };
        ctys.into_iter()
            .map(|cty| {
                let view = self.emit_cur(Inst::FfiViewCast {
                    cty: crate::names::ustr(cty),
                    ptr,
                });
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
        if !bone_core::modules::is_intrinsic_key(*nb, "NativeBuffer") {
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
    ) -> Result<Option<TupleLayout>, CompilerError> {
        let Some(concrete) = self.concrete_expr_ty(e) else {
            return Ok(None);
        };
        let (payload, _) = concrete.peel_nullable();
        match payload {
            MonoTy::Tuple(_) => self.concrete_rep_tuple(concrete, RepUse::Return, true, false),
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
        if !bone_core::modules::is_intrinsic_key(*name, "Coroutine") {
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
