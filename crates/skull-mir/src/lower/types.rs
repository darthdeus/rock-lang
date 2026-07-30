//! Type and shape queries over the body being lowered: nullability, tuple and
//! return shapes, spread operands, and the `TyId` -> `TypeExpr` resolution that
//! specialization substitutes through.
use super::*;

impl Builder<'_> {
    /// Whether `e`'s static type is an enum ALL of whose variants are unit (no
    /// payload). Those variants lower to interned singleton globals — reference-
    /// equal across uses — so `==`/`!=` is native Lua `==`/`~=`, not the dynamic
    /// `__op_eq` (type()/pairs()) walk (static-over-dynamic; trace-friendly).
    pub(crate) fn is_payloadless_enum(&self, e: Idx<Expr>) -> bool {
        let Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) = self.expr_ty(e) else {
            return false;
        };
        let Some(eid) = self.ws.enums_by_name.get(&name) else {
            return false;
        };
        let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
        ed.variants.iter().all(|v| matches!(v.payload, bone_semantic::hir::VariantPayload::Unit))
    }

    pub(crate) fn is_null_lit(&self, e: Idx<Expr>) -> bool {
        matches!(self.body.expr_arena[e], Expr::Null)
    }

    /// Whether `e`'s type is `T?` (a `Nullable`, chasing `Infer` but NOT peeling
    /// the `Nullable` like `expr_ty` does). Drives the `if let` nil-guard.
    pub(crate) fn expr_is_nullable(&self, e: Idx<Expr>) -> bool {
        self.inf
            .and_then(|i| i.expr_types.get(e).copied())
            .map(|ty| self.is_nullable_ty(ty))
            .unwrap_or(false)
    }

    pub(crate) fn is_nullable_ty(&self, ty: TyId) -> bool {
        // A generic bound to `E?` is nullable, and `ty_node` cannot say so: it
        // substitutes the PAYLOAD (a `Nullable` node needs an arena index).
        // Read the raw node first, before that substitution erases the `?`.
        if let Some(TypeExpr::Generic(var, _)) = self.raw_ty_node(ty) {
            if let Some(b) = self.spec_subst.get(&var) {
                return b.peel_nullable().1;
            }
        }
        match self.ty_node(ty) {
            Some(TypeExpr::Nullable(_)) => true,
            Some(TypeExpr::Infer(var, _)) => self
                .inf
                .and_then(|i| i.substitutions.get(var))
                .map(|s| self.is_nullable_ty(s))
                .unwrap_or(false),
            _ => false,
        }
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
        let ty = *self.inf?.expr_types.get(e)?;
        self.resolve_ty(ty)
    }

    /// `Some` iff `e`'s inferred type is a (non-nullable) tuple. Used to route a
    /// tuple result across a call boundary (the call expr's own type IS the
    /// callee's return type, so direct and INDIRECT calls agree).
    /// `resolve_tyid` (not `resolve_ty`) so a `Nullable(Tuple)` doesn't peel to a
    /// bare tuple and get mis-splatted.
    pub(crate) fn expr_tuple_shape(&self, e: Idx<Expr>) -> Option<TupleRetShape> {
        // A bound `next` answers from its DECLARATION, never from the type this
        // body's single generic check left on the call: the destructure that
        // consumes the shape and the call that emits it must read the same
        // oracle, or the binding takes a different number of leaves than the
        // call returns and the difference is made up with a constant.
        if let Some(&gidx) = self.inf?.bound_calls.get(&e) {
            return Some(self.bound_next_tuple_shape(gidx));
        }
        self.tyid_tuple_ret_shape(self.resolve_tyid(*self.inf?.expr_types.get(e)?)?)
    }

    pub(crate) fn repr_join_slot(&self, e: Idx<Expr>) -> Option<RetSlot> {
        if let Some(shape) = self.expr_tuple_shape(e).filter(TupleRetShape::splats) {
            return Some(RetSlot::Nested(shape));
        }
        let mut shape = self.nobox_value_shape_of(e)?;
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
        Some(RetSlot::Leaves(shape))
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

    /// `Some` iff `e` is a lambda whose RETURN type is a (non-nullable) tuple --
    /// the lambda body then splats its slots so an indirect call site captures
    /// them (multi-return ABI). Read off the lambda's inferred `fn(..) -> (..)`
    /// type so it agrees with the call site's `expr_tuple_shape`.
    pub(crate) fn lambda_ret_tuple_shape(&self, e: Idx<Expr>) -> Option<TupleRetShape> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::Function { ret, .. } = self.ty_node(ty)? else { return None };
        self.tyid_tuple_ret_shape(self.resolve_tyid(ty.with_idx(ret))?)
    }

    /// [`tuple_ret_shape`] for a type that may live in the BODY's inference arena
    /// (a call expression's own type, a lambda's return) rather than the
    /// workspace one. `ty_node` applies the enclosing specialization
    /// substitution, so a generic element resolves to the same slot the
    /// signature-side walker sees.
    pub(crate) fn tyid_tuple_ret_shape(&self, ty: TyId) -> Option<TupleRetShape> {
        let ty = self.resolve_tyid(ty)?;
        let TypeExpr::Tuple { elements } = self.ty_node(ty)? else { return None };
        let slots = elements
            .into_iter()
            .map(|el| self.body_ret_slot(ty.with_idx(el)))
            .collect();
        Some(TupleRetShape { slots, nullable: false })
    }

    /// The element slot of a placeholder `next` on the `Iterator`-bounded generic
    /// at `gidx`, read from the enclosing fn's DECLARED bound under this body's
    /// specialization.
    ///
    /// The declaration is the source because it is the only one that carries the
    /// instantiation. A bounded body is type-checked ONCE against its bound, so
    /// the `next(x)` call's inferred `(bool, E)` is a body-local type from that
    /// single check -- it neither names the generic nor tracks the site, and
    /// reading the element off it plans a shape for whatever that check happened
    /// to leave there. Same rule as the presence slot: read a declaration, never
    /// an expression's type.
    ///
    /// `Opaque` whenever the specialization does not resolve the element -- the
    /// shared generic snapshot keeps one opaque slot and the splice's repack
    /// keeps correcting it.
    pub(crate) fn bound_next_elem_slot(&self, gidx: usize) -> RetSlot {
        match self.generic_bounds.get(gidx).copied().flatten() {
            Some(elem) => ret_slot(self.ws, self.spec_subst, elem),
            None => RetSlot::Opaque,
        }
    }

    /// The full `(bool, E)` shape of a placeholder `next` on the generic at
    /// `gidx` -- THE oracle every consumer of that call routes through.
    pub(crate) fn bound_next_tuple_shape(&self, gidx: usize) -> TupleRetShape {
        TupleRetShape {
            nullable: false,
            slots: vec![RetSlot::Opaque, self.bound_next_elem_slot(gidx)],
        }
    }

    pub(crate) fn body_ret_slot(&self, ty: TyId) -> RetSlot {
        // The generic this slot stands for, if the substitution names it. An
        // element of a tuple literal's inferred type arrives as the declared
        // generic in one `next` body and as an inference VAR in another, so both
        // spellings are chased -- but only to answer THIS question. Resolving
        // `ty` for the rest of the function instead is wrong: it erases a
        // `Nullable` binding's `?` on the way through `ty_node`, which silently
        // re-planned every nullable-column body to a non-nullable shape.
        let bound_generic = match self.raw_ty_node(ty) {
            Some(TypeExpr::Generic(var, _)) => Some(var),
            Some(TypeExpr::Infer(..)) => match self.resolve_tyid(ty).and_then(|t| self.raw_ty_node(t)) {
                Some(TypeExpr::Generic(var, _)) => Some(var),
                _ => None,
            },
            _ => None,
        };
        // A generic bound to a TUPLE is the one binding `ty_node` cannot answer
        // (see `reified_type_node`), so it is read off the substitution directly.
        if let Some(bound @ ReifiedType::Tuple(_)) = bound_generic.and_then(|v| self.spec_subst.get(&v)) {
            return reified_ret_slot(self.ws, bound);
        }
        if let Some(shape) = self.body_nobox_shape(ty) {
            return RetSlot::Leaves(shape);
        }
        match self.tyid_tuple_ret_shape(ty) {
            Some(inner) if inner.splats() => RetSlot::Nested(inner),
            _ => RetSlot::Opaque,
        }
    }

    /// [`Self::body_sig_value_shape`] narrowed to `nobox` nominals -- the splat
    /// gate, mirroring [`nobox_sig_shape_spec`] on the body arena.
    pub(crate) fn body_nobox_shape(&self, ty: TyId) -> Option<ValueShape> {
        let ty = self.resolve_tyid(ty)?;
        match self.ty_node(ty)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if self.is_nobox_struct(n) => {
                sig_shape_of_nominal(self.ws, n)
            }
            TypeExpr::Nullable(inner) => {
                let inner = self.resolve_tyid(ty.with_idx(inner))?;
                match self.ty_node(inner)? {
                    TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if self.is_nobox_struct(n) => {
                        sig_shape_of_nullable_nominal(self.ws, n)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
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
        let inf = self.inf.ok_or_else(|| {
            CompilerError::new_with_label(span, "`native_buffer_alloc` needs inference".into())
        })?;
        let ret = *inf.expr_types.get(e).ok_or_else(|| {
            CompilerError::new_with_label(span, "`native_buffer_alloc` call is untyped".into())
        })?;
        // In a specialized splice the `NativeBuffer<T>` element resolves through
        // `spec_subst` to a concrete type, so the intrinsic can name the ctype the
        // shared generic body could not. An empty subst is exactly the shared path.
        let r = if self.spec_subst.is_empty() {
            native_buffer_alloc_elem_ctype(self.ws, inf, ret)
        } else {
            native_buffer_alloc_elem_ctype_spec(self.ws, inf, ret, self.spec_subst)
        };
        r.map_err(|msg| CompilerError::new_with_label(span, msg))
    }

    /// `Some` iff `e`'s type is a tuple OR a `(tuple)?` -- the nil-peeled form
    /// for safe-nav calls, whose result type is nullable but whose callee still
    /// splats the inner tuple.
    pub(crate) fn peeled_tuple_shape(&self, e: Idx<Expr>) -> Option<TupleRetShape> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let inner = match self.ty_node(ty)? {
            TypeExpr::Nullable(i) => self.resolve_tyid(ty.with_idx(i))?,
            _ => ty,
        };
        self.tyid_tuple_ret_shape(inner)
    }

    pub(crate) fn resolve_ty(&self, ty: TyId) -> Option<TypeExpr> {
        let te = self.ty_node(ty)?;
        match te {
            TypeExpr::Infer(var, _) => {
                let solved = self.inf?.substitutions.get(var)?;
                self.resolve_ty(solved)
            }
            TypeExpr::Nullable(inner) => self.resolve_ty(ty.with_idx(inner)),
            other => Some(other),
        }
    }

    /// The stored type node, WITHOUT the specialization substitution -- the only
    /// place a generic's own identity is still visible, which is what
    /// [`Self::is_nullable_ty`] needs (the substitution answers with the payload).
    pub(crate) fn raw_ty_node(&self, ty: TyId) -> Option<TypeExpr> {
        Some(match ty {
            TyId::Static(mid, idx) => self.ws.module(mid).ty_arena[idx],
            TyId::Local(idx) => self.inf?.ty_arena[idx],
        })
    }

    pub(crate) fn ty_node(&self, ty: TyId) -> Option<TypeExpr> {
        let node = self.raw_ty_node(ty)?;
        // In a specialized instantiation, a generic param resolves to its concrete
        // leaf so all downstream type-driven lowering (field projection, container
        // element, scalar shape) sees the real type. Synthesized leaves carry no
        // App params -- v0 specializes leaf types only -- so no consumer indexes
        // into `new_empty()`.
        if let TypeExpr::Generic(var, _) = node {
            // A tuple binding has no node to stand for it, and answers `None`.
            // Falling through to the generic is exactly what this saw before
            // tuples reified at all, so no consumer changes behaviour; the ones
            // that need a tuple's slots read the binding through
            // `reified_ret_slot` instead.
            if let Some(sub) = self.spec_subst.get(&var).and_then(reified_type_node) {
                return Some(sub);
            }
        }
        Some(node)
    }

    /// The final `TyId` after chasing `Infer` substitutions (NOT peeling
    /// `Nullable` -- unlike `resolve_ty`, this keeps the `TyId` so its App params
    /// can be indexed in the correct arena via `with_idx`).
    pub(crate) fn resolve_tyid(&self, ty: TyId) -> Option<TyId> {
        match self.ty_node(ty)? {
            TypeExpr::Infer(var, _) => {
                let solved = self.inf?.substitutions.get(var)?;
                self.resolve_tyid(solved)
            }
            _ => Some(ty),
        }
    }

    /// The `EnumFlatShape` of `Y` when `e`'s type is `Coroutine<Y, ..>` and `Y` is
    /// a flat enum -- the shape flowing through a `await` delegation to a flat-enum
    /// coroutine (so the delegate threads the inner's leaves instead of a box).
    pub(crate) fn coroutine_yield_flat_shape(&self, e: Idx<Expr>) -> Option<EnumFlatShape> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::App { ctor: TypeCtor::Nominal(n), params } = self.ty_node(ty)? else {
            return None;
        };
        if n.as_str() != "Coroutine" {
            return None;
        }
        let y_ty = self.resolve_tyid(ty.with_idx(params.into_iter().next()?))?;
        match self.ty_node(y_ty)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(yn), .. } => flat_enum_shape(self.ws, yn),
            _ => None,
        }
    }
}
