//! Type and shape queries over the body being lowered: nullability, tuple and
//! return shapes, spread operands, and the `TyId` -> `TypeExpr` resolution that
//! specialization substitutes through.
use super::*;
use crate::monoty::MonoTy;

impl Builder<'_> {
    pub(crate) fn is_null_lit(&self, e: Idx<Expr>) -> bool {
        self.dispatch().is_null_lit(e)
    }

    /// Whether `e`'s type is `T?` (a `Nullable`, chasing `Infer` but NOT peeling
    /// the `Nullable` like `expr_ty` does). Drives the `if let` nil-guard.
    pub(crate) fn expr_is_nullable(&self, e: Idx<Expr>) -> bool {
        self.dispatch().expr_is_nullable(e)
    }

    pub(crate) fn concrete_expr_ty(&self, e: Idx<Expr>) -> Option<MonoTy> {
        self.dispatch().concrete_expr_ty(e)
    }

    pub(crate) fn concrete_ty_id(&self, ty: TyId) -> Option<MonoTy> {
        self.dispatch().concrete_ty(ty)
    }

    pub(crate) fn source_mono_type(&self, ty: TyId) -> Option<MonoTy> {
        if let Some(concrete) = self.concrete_ty_id(ty) {
            return Some(concrete);
        }
        if self.concrete_plan.is_some() {
            return None;
        }
        let inf = self.inf?;
        crate::monoty::convert_in(
            self.ws,
            crate::monoty::TyEnv::Body(inf),
            ty,
            self.subst(),
        )
        .ok()
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

    pub(crate) fn source_rep_id(
        &self,
        ty: TyId,
        use_context: RepUse,
        tuple_spelled: bool,
        nobox: bool,
    ) -> Option<RepId> {
        let source = self.source_mono_type(ty)?;
        self.scx
            .rep_arena()
            .plan(
                self.ws,
                self.scx.target(),
                &source,
                use_context,
                &RepUseFacts { tuple_spelled, span: self.cur_span },
                RepGuarantees { nobox, allow_opaque: true },
            )
            .ok()
    }

    pub(crate) fn expr_rep_id(
        &self,
        e: Idx<Expr>,
        use_context: RepUse,
        nobox: bool,
    ) -> Option<RepId> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let tuple_spelled = matches!(ty.peel_nullable().0, MonoTy::Tuple(_));
            return self.concrete_rep_id(&ty, use_context, tuple_spelled, nobox).ok();
        }
        if self.concrete_plan.is_some() {
            return None;
        }
        let ty = *self.inf?.expr_types.get(e)?;
        self.source_rep_id(ty, use_context, self.source_tuple_spelled(ty), nobox)
    }

    pub(crate) fn nominal_rep_shape(&self, name: Ustr, nobox: bool) -> Option<ValueShape> {
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
            .ok()?;
        self.scx.rep_arena().value_shape(id)
    }

    pub(crate) fn rep_shape_for_ty(&self, ty: TyId, use_context: RepUse, nobox: bool) -> Option<ValueShape> {
        let id = self.source_rep_id(ty, use_context, self.source_tuple_spelled(ty), nobox)?;
        self.scx.rep_arena().value_shape(id)
    }

    pub(crate) fn source_rep_slot(
        &self,
        ty: TyId,
        use_context: RepUse,
        tuple_spelled: bool,
        nobox: bool,
    ) -> RetSlot {
        self.source_rep_id(ty, use_context, tuple_spelled, nobox)
            .map(|id| self.scx.rep_arena().ret_slot(id))
            .unwrap_or(RetSlot::Opaque)
    }

    pub(crate) fn declaration_rep_tuple(
        &self,
        ty: TyId,
        subst: &SpecSubst,
    ) -> Option<TupleRetShape> {
        let source = crate::monoty::convert_decl(self.ws, ty, subst).ok()?;
        let id = self
            .scx
            .rep_arena()
            .plan(
                self.ws,
                self.scx.target(),
                &source,
                RepUse::Return,
                &RepUseFacts { tuple_spelled: true, span: self.cur_span },
                RepGuarantees { nobox: true, allow_opaque: true },
            )
            .ok()?;
        self.scx.rep_arena().tuple_shape(id)
    }

    pub(crate) fn source_tuple_spelled(&self, ty: TyId) -> bool {
        let ty = match self.raw_ty_node(ty) {
            Some(TypeExpr::Infer(..)) => self.resolve_tyid(ty),
            Some(_) => Some(ty),
            None => None,
        };
        let Some(ty) = ty else { return false };
        match self.raw_ty_node(ty) {
            Some(TypeExpr::Tuple { .. }) => true,
            Some(TypeExpr::Nullable(inner)) => {
                matches!(self.raw_ty_node(ty.with_idx(inner)), Some(TypeExpr::Tuple { .. }))
            }
            _ => false,
        }
    }

    pub(crate) fn concrete_expr_is_untyped(&self, e: Idx<Expr>) -> bool {
        self.dispatch()
            .concrete_types
            .is_some_and(|types| types.is_untyped_expr(e))
    }

    pub(crate) fn is_nullable_ty(&self, ty: TyId) -> bool {
        self.dispatch().is_nullable_ty(ty)
    }

    /// The declared field names of a call-site spread operand `..op`, in struct
    /// order. bone-inference has already verified the operand is a nominal
    /// struct whose fields all match callee params.
    pub(crate) fn spread_operand_fields(&self, op: Idx<Expr>) -> Result<Vec<Ustr>, CompilerError> {
        match self.expr_ty(op) {
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) => self
                .struct_field_names(name)
                .ok_or_else(|| gap(format!("spread operand `{name}` has no known fields"))),
            _ => Err(gap("spread operand is not a nominal struct")),
        }
    }

    /// A spread operand `..op` decomposed to (field name, boxed value) pairs. A
    /// struct-LITERAL operand is ELIDED: its field exprs lower directly (base
    /// first, then explicit fields in source order for side effects; unwritten
    /// fields from a functional-update base or the struct default), so no
    /// intermediate table is built. Any other operand -- and a literal with a
    /// flattened-enum field, which needs slot expansion -- is lowered once and
    /// its fields read off it.
    pub(crate) fn spread_field_insts(&mut self, op: Idx<Expr>) -> Result<Vec<(Ustr, Idx<Inst>)>, CompilerError> {
        if let Expr::StructLiteral { name, fields, bases } = self.body.expr_arena[op].clone() {
            let decl = self.struct_field_names(name).unwrap_or_default();
            if !decl.iter().any(|f| self.flat_field_of_struct(name, *f).is_some()) {
                let base_inst = match bases.into_iter().next() {
                    Some(b) => Some(self.lower_expr(b)?),
                    None => None,
                };
                let mut explicit: Vec<(Ustr, Idx<Inst>)> = Vec::new();
                let mut by_value: HashMap<u32, Idx<Inst>> = HashMap::new();
                for fidx in fields {
                    let lf = self.body.struct_lit_field_arena[fidx];
                    let key: u32 = lf.value.into_raw().into();
                    let v = match by_value.get(&key) {
                        Some(&v) => v,
                        None => {
                            let v = self.lower_expr(lf.value)?;
                            by_value.insert(key, v);
                            v
                        }
                    };
                    explicit.push((lf.name, v));
                }
                let mut out = Vec::new();
                for fname in decl {
                    let v = if let Some((_, v)) = explicit.iter().find(|(n, _)| *n == fname) {
                        *v
                    } else if let Some(bi) = base_inst {
                        self.emit_cur(Inst::Field { base: bi, field: fname })
                    } else {
                        self.lower_struct_default(name, fname)?
                    };
                    out.push((fname, v));
                }
                return Ok(out);
            }
        }
        let base = self.lower_expr(op)?;
        let mut out = Vec::new();
        for f in self.spread_operand_fields(op)? {
            out.push((f, self.emit_cur(Inst::Field { base, field: f })));
        }
        Ok(out)
    }

    /// The `ValueRepr` form of [`spread_field_insts`], for the scalar-ABI arg
    /// path. Same literal-elision; a Group base projects its parts, a boxed base
    /// reads fields off the packed cdata.
    pub(crate) fn spread_field_reprs(&mut self, op: Idx<Expr>) -> Result<Vec<(Ustr, ValueRepr)>, CompilerError> {
        if let Expr::StructLiteral { name, fields, bases } = self.body.expr_arena[op].clone() {
            let decl = self.struct_field_names(name).unwrap_or_default();
            if !decl.iter().any(|f| self.flat_field_of_struct(name, *f).is_some()) {
                let base_repr = match bases.into_iter().next() {
                    Some(b) => Some(self.lower_expr_repr(b)?),
                    None => None,
                };
                let base_boxed = match &base_repr {
                    Some(ValueRepr::Group { .. }) | None => None,
                    Some(other) => Some(self.pack(other.clone())),
                };
                let mut explicit: Vec<(Ustr, ValueRepr)> = Vec::new();
                let mut by_value: HashMap<u32, ValueRepr> = HashMap::new();
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
                        self.spread_base_field_repr(&base_repr, base_boxed, name, fname)?
                    };
                    out.push((fname, r));
                }
                return Ok(out);
            }
        }
        let base_repr = self.lower_expr_repr(op)?;
        let base_boxed = match &base_repr {
            ValueRepr::Group { .. } => None,
            other => Some(self.pack(other.clone())),
        };
        let name = match self.expr_ty(op) {
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) => name,
            _ => return Err(gap("spread operand is not a nominal struct")),
        };
        let mut out = Vec::new();
        for fname in self.struct_field_names(name).unwrap_or_default() {
            let r = self.spread_base_field_repr(&Some(base_repr.clone()), base_boxed, name, fname)?;
            out.push((fname, r));
        }
        Ok(out)
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
        Ok(match (base_repr, base_boxed) {
            (Some(ValueRepr::Group { name: bn, parts, .. }), _) => {
                let i = self.field_index(*bn, fname).ok_or_else(|| gap("spread field"))?;
                parts[i].clone()
            }
            (Some(_), Some(bi)) => {
                ValueRepr::Scalar(self.emit_cur(Inst::Field { base: bi, field: fname }))
            }
            _ => ValueRepr::Scalar(self.lower_struct_default(name, fname)?),
        })
    }

    /// The resolved type of an expression (chasing `Infer` vars + peeling
    /// `Nullable`), or `None` when inference is absent/unsolved.
    pub(crate) fn expr_ty(&self, e: Idx<Expr>) -> Option<TypeExpr> {
        self.dispatch().expr_ty(e)
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
        if let Some(MonoTy::Tuple(parts)) = self.concrete_expr_ty(e) {
            let ty = MonoTy::Tuple(parts);
            return self.concrete_rep_tuple(&ty, RepUse::Return, true, false);
        }
        if self.concrete_plan.is_some() {
            return Ok(None);
        }
        Ok(self
            .inf
            .and_then(|inf| inf.expr_types.get(e))
            .and_then(|ty| self.resolve_tyid(*ty))
            .and_then(|ty| self.tyid_tuple_ret_shape(ty)))
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
        let Some(mut shape) = self.nobox_value_shape_of(e) else { return Ok(None) };
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

    /// The tuple plan for a type that may live in the BODY's inference arena (a
    /// call expression's own type, a lambda's return) rather than the workspace
    /// one.
    pub(crate) fn tyid_tuple_ret_shape(&self, ty: TyId) -> Option<TupleRetShape> {
        if let Some(concrete) = self.concrete_ty_id(ty) {
            return match concrete {
                MonoTy::Tuple(_) => self
                    .concrete_rep_tuple(&concrete, RepUse::Return, true, false)
                    .ok()
                    .flatten(),
                MonoTy::Nullable(_) => None,
                _ => None,
            };
        }
        if self.concrete_plan.is_some() {
            return None;
        }
        let ty = self.resolve_tyid(ty)?;
        if !matches!(self.ty_node(ty), Some(TypeExpr::Tuple { .. })) {
            return None;
        }
        let source = self.source_mono_type(ty)?;
        let id = self
            .scx
            .rep_arena()
            .plan(
                self.ws,
                self.scx.target(),
                &source,
                RepUse::Return,
                &RepUseFacts { tuple_spelled: true, span: self.cur_span },
                RepGuarantees { nobox: true, allow_opaque: true },
            )
            .ok()?;
        self.scx.rep_arena().tuple_shape(id)
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

    /// The element slot of a placeholder `next` on the `Iterator`-bounded generic
    /// at `gidx` in the legacy shared body. A concrete body uses its resolved
    /// witness's ABI plan instead.
    ///
    /// `Opaque` whenever the shared body's specialization does not resolve the
    /// element; the splice's repack corrects that shape for the concrete callee.
    pub(crate) fn bound_next_elem_slot(&self, gidx: usize) -> RetSlot {
        match self.generic_bounds.get(gidx).copied().flatten() {
            Some(elem) => self.source_rep_slot(elem, RepUse::TupleElement, false, true),
            None => RetSlot::Opaque,
        }
    }

    /// The full `(bool, E)` shape of a placeholder `next` on the generic at
    /// `gidx` -- the concrete bound plan or the legacy shared-body rule is the
    /// oracle every consumer of that call routes through.
    pub(crate) fn bound_next_tuple_shape(
        &self,
        gidx: usize,
    ) -> Result<TupleRetShape, CompilerError> {
        if let Some(plan) = self.concrete_plan {
            let next = plan.bound_tree.get(gidx).ok_or_else(|| {
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
            return Ok(shape);
        }
        Ok(TupleRetShape {
            nullable: false,
            slots: vec![RetSlot::Opaque, self.bound_next_elem_slot(gidx)],
        })
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
        let Ok(ctys) = leaf_view_ctys(self.ws, ctype) else {
            return Vec::new();
        };
        ctys.into_iter()
            .map(|cty| {
                let view = self.emit_cur(Inst::FfiViewCast { cty: Ustr::from(cty), ptr });
                (leaf_view_field(cty), view)
            })
            .collect()
    }

    pub(crate) fn native_buffer_alloc_ctype(&self, e: Idx<Expr>, span: Span) -> Result<Ustr, CompilerError> {
        if let Some(ret) = self.concrete_expr_ty(e) {
            return native_buffer_alloc_elem_ctype_mono(self.ws, &ret)
                .map_err(|msg| CompilerError::new_with_label(span, msg));
        }
        if self.concrete_plan.is_some() {
            return Err(CompilerError::new_with_label(
                span,
                "`native_buffer_alloc` call is missing from the concrete type plan".to_string(),
            ));
        }
        let inf = self.inf.ok_or_else(|| {
            CompilerError::new_with_label(span, "`native_buffer_alloc` needs inference".into())
        })?;
        let ret = *inf.expr_types.get(e).ok_or_else(|| {
            CompilerError::new_with_label(span, "`native_buffer_alloc` call is untyped".into())
        })?;
        // In a specialized splice the `NativeBuffer<T>` element resolves through
        // `spec_subst` to a concrete type, so the intrinsic can name the ctype the
        // shared generic body could not. An empty subst is exactly the shared path.
        let r = if self.subst().is_empty() {
            native_buffer_alloc_elem_ctype(self.ws, inf, ret)
        } else {
            native_buffer_alloc_elem_ctype_spec(self.ws, inf, ret, self.subst())
        };
        r.map_err(|msg| CompilerError::new_with_label(span, msg))
    }

    /// `Some` iff `e`'s type is a tuple OR a `(tuple)?` -- the nil-peeled form
    /// for safe-nav calls, whose result type is nullable but whose callee still
    /// splats the inner tuple.
    pub(crate) fn peeled_tuple_shape(&self, e: Idx<Expr>) -> Option<TupleRetShape> {
        if let Some(concrete) = self.concrete_expr_ty(e) {
            let (payload, _) = concrete.peel_nullable();
            return match payload {
                MonoTy::Tuple(_) => self
                    .concrete_rep_tuple(&concrete, RepUse::Return, true, false)
                    .ok()
                    .flatten(),
                _ => None,
            };
        }
        if self.concrete_plan.is_some() {
            return None;
        }
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let inner = match self.ty_node(ty)? {
            TypeExpr::Nullable(i) => self.resolve_tyid(ty.with_idx(i))?,
            _ => ty,
        };
        self.tyid_tuple_ret_shape(inner)
    }

    pub(crate) fn resolve_ty(&self, ty: TyId) -> Option<TypeExpr> {
        self.dispatch().resolve_ty(ty)
    }

    /// The stored type node, WITHOUT the specialization substitution -- the only
    /// place a generic's own identity is still visible, which is what
    /// [`Self::is_nullable_ty`] needs (the substitution answers with the payload).
    pub(crate) fn raw_ty_node(&self, ty: TyId) -> Option<TypeExpr> {
        self.dispatch().raw_ty_node(ty)
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
        if let Some(concrete) = self.concrete_expr_ty(e) {
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
            return EnumPlan::for_name(self.ws, *yield_name).and_then(|plan| plan.flat().cloned());
        }
        if self.concrete_plan.is_some() {
            return None;
        }
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::App { ctor: TypeCtor::Nominal(n), params } = self.ty_node(ty)? else {
            return None;
        };
        if n.as_str() != "Coroutine" {
            return None;
        }
        let y_ty = self.resolve_tyid(ty.with_idx(params.into_iter().next()?))?;
        match self.ty_node(y_ty)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(yn), .. } => {
                EnumPlan::for_name(self.ws, yn).and_then(|plan| plan.flat().cloned())
            }
            _ => None,
        }
    }
}

fn native_buffer_alloc_elem_ctype_mono(
    ws: &Workspace,
    ret: &MonoTy,
) -> Result<Ustr, String> {
    let crate::monoty::MonoTy::App {
        ctor: crate::monoty::MonoCtor::Nominal(nb),
        params,
    } = ret.peel_nullable().0
    else {
        return Err("`native_buffer_alloc` must produce a `NativeBuffer<T>`".to_string());
    };
    if nb.as_str() != "NativeBuffer" {
        return Err(format!("`native_buffer_alloc` result is `{nb}`, expected `NativeBuffer<T>`"));
    }
    let Some(elem) = params.first() else {
        return Err("`NativeBuffer` element type is missing".to_string());
    };
    if let Some(prim) = elem.prim_name() {
        return Ok(Ustr::from(if prim == "uint" { "uint32_t" } else { prim }));
    }
    let Some(name) = elem.nominal() else {
        return Err(
            "`native_buffer_alloc<T>` requires `T` to be an FFI-clean `value` struct or \
             `int`/`float`/`bool` -- a heap struct / non-C-array type has no flat layout."
                .to_string(),
        );
    };
    if !map_is_value_struct(ws, name) {
        return Err(format!("`native_buffer_alloc<{name}>` is invalid: `{name}` is not a `value` struct."));
    }
    if value_struct_is_gc(ws, name) {
        return Err(format!(
            "`native_buffer_alloc<{name}>` is invalid: `{name}` is a GC-carrying `value` \
             struct (immutable Lua table) with no C layout, so `ffi.new(\"{name}[?]\")` \
             names no ctype. Only FFI-clean value structs (C-mappable fields) and \
             `int`/`float`/`bool` are allocatable."
        ));
    }
    Ok(name)
}
