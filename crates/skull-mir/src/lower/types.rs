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

    pub(crate) fn repr_join_slot(&self, e: Idx<Expr>) -> Result<Option<RetSlot>, CompilerError> {
        if let Some(shape) = self.expr_tuple_shape(e).filter(TupleRetShape::splats) {
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
        // A generic bound to a TUPLE is the one binding `ty_node` cannot answer
        // (see `mono_type_node`), so it is read off the substitution here --
        // the same source `body_ret_slot` reads. Without it a specialized body's
        // indirect call sees a bare `Generic` and declines to splat, while the
        // fn value it calls splats from its own concrete type.
        if let Some(MonoTy::Tuple(parts)) = self.bound_generic_subst(ty) {
            return Some(mono_tuple_shape(self.ws, parts));
        }
        let ty = self.resolve_tyid(ty)?;
        let TypeExpr::Tuple { elements } = self.ty_node(ty)? else { return None };
        let slots = elements
            .into_iter()
            .map(|el| self.body_ret_slot(ty.with_idx(el)))
            .collect();
        Some(TupleRetShape { slots, nullable: false })
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
    pub(crate) fn tuple_box_shape(&self, e: Idx<Expr>) -> Option<TupleRetShape> {
        self.expr_tuple_shape(e).filter(|s| s.splats())
    }

    /// The tuple slot of a declared PARAM on the body arena -- [`tuple_param_slot`]'s
    /// spelling for a nested fn and for a fn TYPE's params.
    ///
    /// Separate from [`Self::tyid_tuple_ret_shape`] because the two answer
    /// different questions about the same `Nullable(Tuple)`: that one is the CALL
    /// boundary, where no callee puts a presence on the wire, so it must keep
    /// declining. A param type is a DECLARATION, which is the one source allowed
    /// to set the tag -- so `(NV, int)?` splats here, presence first, exactly as
    /// the matching return does.
    pub(crate) fn tuple_param_slot_of_ty(&self, ty: TyId) -> RetSlot {
        // The plain tuple answer FIRST, generic-bound-to-a-tuple included
        // (`zip`'s pair reaches a combinator param that way, and it has no type
        // node to read). Only what that declines gets the `?` peel.
        let shape = self.tyid_tuple_ret_shape(ty).or_else(|| self.nullable_tuple_param_shape(ty));
        match shape {
            Some(inner) if inner.splats() => RetSlot::Nested(inner),
            _ => RetSlot::Opaque,
        }
    }

    /// A spelled `(A, B)?` param, with the presence leading its slots. `None`
    /// unless an element actually splats: a nullable tuple of ordinary elements
    /// has nothing to gain from the multi-value form and keeps its single boxed
    /// slot, byte-for-byte the pre-existing ABI ([`tuple_ret_shape`]'s rule,
    /// restated because this peels the `?` itself).
    fn nullable_tuple_param_shape(&self, ty: TyId) -> Option<TupleRetShape> {
        let ty = self.resolve_tyid(ty)?;
        let TypeExpr::Nullable(inner) = self.ty_node(ty)? else { return None };
        let inner = self.tyid_tuple_ret_shape(self.resolve_tyid(ty.with_idx(inner))?)?;
        Some(TupleRetShape { nullable: true, ..inner }).filter(TupleRetShape::splats)
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

    /// What the substitution binds `ty` to, when `ty` is (or resolves to) a
    /// declared generic. An element of a tuple literal's inferred type arrives as
    /// the declared generic in one `next` body and as an inference VAR in
    /// another, so both spellings are chased -- but only to answer THIS question.
    /// Resolving `ty` for the rest of a caller instead is wrong: it erases a
    /// `Nullable` binding's `?` on the way through `ty_node`, which silently
    /// re-planned every nullable-column body to a non-nullable shape.
    pub(crate) fn bound_generic_subst(&self, ty: TyId) -> Option<&MonoTy> {
        let var = match self.raw_ty_node(ty) {
            Some(TypeExpr::Generic(var, _)) => var,
            Some(TypeExpr::Infer(..)) => match self.resolve_tyid(ty).and_then(|t| self.raw_ty_node(t)) {
                Some(TypeExpr::Generic(var, _)) => var,
                _ => return None,
            },
            _ => return None,
        };
        self.spec_subst.get(&var)
    }

    pub(crate) fn body_ret_slot(&self, ty: TyId) -> RetSlot {
        // A generic bound to a TUPLE is the one binding `ty_node` cannot answer
        // (see `mono_type_node`), so it is read off the substitution directly.
        if let Some(bound @ MonoTy::Tuple(_)) = self.bound_generic_subst(ty) {
            return mono_ret_slot(self.ws, bound);
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
