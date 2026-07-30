//! Collection and type KIND queries the walk dispatches on: which vec/map/
//! native-buffer flavour a receiver is, element types behind a column, and the
//! numeric coercions (`int`->`float`, binding and param types).
use super::*;

impl Builder<'_> {
    pub(crate) fn expr_is_flat_vec(&self, e: Idx<Expr>) -> bool {
        matches!(self.expr_ty(e),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) if n.as_str() == "FlatVec")
    }

    /// The value-STRUCT element of `e`'s `NativeBuffer<E>` / `FlatVec<E>` type,
    /// or `None` when `E` is a primitive / unresolved generic / not value-shaped.
    /// Drives [`ContainerKind::NativeBufferValue`]; an opaque generic `E` stays
    /// plain `NativeBuffer` (matching the alloc intrinsic, which also requires a
    /// concrete element).
    pub(crate) fn nb_value_elem(&self, e: Idx<Expr>) -> Option<Ustr> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::App { ctor: TypeCtor::Nominal(n), params } = self.ty_node(ty)? else {
            return None;
        };
        if !matches!(n.as_str(), "NativeBuffer" | "FlatVec") {
            return None;
        }
        let p = params.into_iter().next()?;
        let elem = self.resolve_tyid(ty.with_idx(p))?;
        let en = match self.ty_node(elem)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n,
            // Resolve through the active specialization, exactly as
            // `vec_column_elem_of` does. Inside a SPECIALIZED body (the `@inline`
            // FlatVec surface at a concrete site) the element is the real struct
            // rather than an opaque `T`; without this the stdlib bodies keep the
            // boxed path, which for a `nobox` element is not merely slower but a
            // guarantee violation. An UNSUBSTITUTED generic stays boxed on
            // purpose -- the shared snapshot must keep the representation every
            // instantiation can read.
            TypeExpr::Generic(var, _) => match self.spec_subst.get(&var) {
                Some(ty) => ty.nominal()?,
                _ => return None,
            },
            _ => return None,
        };
        is_ffi_clean_value_struct(self.ws, en).then_some(en)
    }

    /// The column-eligible element of `e`'s `Arena<E>` type, if any. The twin of
    /// [`Self::vec_column_elem_of`] one container out: an arena's `data` field is
    /// an ordinary `[E]`, so it columnizes on exactly the same rule, and the
    /// hardcoded `for`-over-arena walk has to read it the same way the Vec path
    /// would.
    pub(crate) fn arena_column_elem_of(&self, e: Idx<Expr>) -> Option<Ustr> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::App { ctor: TypeCtor::Nominal(_), params } = self.ty_node(ty)? else {
            return None;
        };
        let p = params.into_iter().next()?;
        let elem = self.resolve_tyid(ty.with_idx(p))?;
        let en = match self.ty_node(elem)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n,
            TypeExpr::Generic(var, _) => match self.spec_subst.get(&var) {
                Some(ty) => ty.nominal()?,
                _ => return None,
            },
            _ => return None,
        };
        vec_column_elem(self.ws, en, false)
    }

    /// The Vec container kind for base expr `e`: an all-scalar value-struct
    /// element stores as flat columns, everything else keeps the single `_data`
    /// backing. An unresolved / generic element is NOT eligible -- the columns
    /// need a concrete value-struct leaf list, and an opaque `T` has none.
    pub(crate) fn vec_kind(&self, e: Idx<Expr>) -> ContainerKind {
        match self.vec_column_elem_of(e) {
            Some(elem) => ContainerKind::VecValueColumns(elem),
            None => ContainerKind::Vec,
        }
    }

    /// The column-eligible value-struct element of `e`'s `[E]` type, if any.
    ///
    /// Resolves through the active [`SpecSubst`], so inside a SPECIALIZED body
    /// (`VecIter<Vec2>.next`, `fn f<T>(xs: [T])` at a `[Vec2]` site) the element
    /// is the concrete struct rather than an opaque `T`. Without this the
    /// stdlib iterator lowers `it.vec[it.i]` against the boxed representation
    /// and reads a `_data` that column storage does not have.
    pub(crate) fn vec_column_elem_of(&self, e: Idx<Expr>) -> Option<ColumnElem> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::App { ctor: TypeCtor::Array, params } = self.ty_node(ty)? else {
            return None;
        };
        let p = params.into_iter().next()?;
        let mut elem = self.resolve_tyid(ty.with_idx(p))?;
        // A NULLABLE element columnizes only when the payload is `nobox`. For an
        // unmarked value struct the boxed element is a legal representation and
        // already works, while columnizing it would newly route the materializing
        // helpers (`__rock_col_sort`, `to_lua_array`, serialization) at a shape
        // they have no presence concept for. The mark is what makes the box
        // forbidden, so the mark is what obliges the presence column.
        //
        // Asked BEFORE the peel, and through the one nullability query, so a
        // spelled-out `[E?]` and a generic bound to `E?` answer alike -- the
        // latter's `?` lives in the binding, and `ty_node` substitutes the
        // payload over it.
        let nullable = self.is_nullable_ty(elem);
        if let Some(TypeExpr::Nullable(inner)) = self.ty_node(elem) {
            elem = self.resolve_tyid(elem.with_idx(inner))?;
        }
        let en = match self.ty_node(elem)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n,
            // An UNSUBSTITUTED generic element stays boxed: the shared
            // (non-specialized) snapshot of a generic body must keep the
            // representation that every instantiation can read.
            TypeExpr::Generic(var, _) => match self.spec_subst.get(&var) {
                Some(ty) => ty.peel_nullable().0.nominal()?,
                _ => return None,
            },
            _ => return None,
        };
        if nullable && !is_nobox_nominal(self.ws, en) {
            return None;
        }
        vec_column_elem(self.ws, en, nullable)
            .filter(|&name| columns_available(self.ws, self.scx.target, name))
            .map(|name| ColumnElem { name, nullable })
    }

    /// The column shape of `elem`: its value shape, marked nullable when the
    /// ELEMENT type is. Every column site builds its shape here, so the leaf
    /// wire (`repr_to_leaves` / `rebuild_from_leaves`, which both lead a nullable
    /// shape with presence) and the column keys ([`column_leaf_paths`]) can only
    /// agree.
    pub(crate) fn column_shape(&self, elem: ColumnElem) -> Result<ValueShape, CompilerError> {
        let mut shape = value_shape(self.ws, elem.name)
            .ok_or_else(|| gap("column element is not a value struct"))?;
        shape.nullable = elem.nullable;
        Ok(shape)
    }

    /// The column-eligible value-struct VALUE of `e`'s `Map<K, V>` type, if any.
    ///
    /// The Map sibling of [`Self::vec_column_elem_of`], resolving through the
    /// active [`SpecSubst`] for the same reason: inside a specialized generic
    /// body the value must read as the concrete struct, or the body lowers
    /// against a bare table the column representation does not have.
    ///
    /// The KEY is deliberately not consulted -- a value-struct key rejects
    /// upstream (LuaJIT hashes cdata by pointer), and the spine stores whatever
    /// key the boxed representation already stored.
    pub(crate) fn map_column_value_of(&self, e: Idx<Expr>) -> Option<ColumnElem> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::App { ctor: TypeCtor::Map, params } = self.ty_node(ty)? else {
            return None;
        };
        let v = params.into_iter().nth(1)?;
        let mut val = self.resolve_tyid(ty.with_idx(v))?;
        // `Map<K, P?>` is column-stored too, and unlike `[P?]` it needs no
        // presence column: an absent value collapses to key REMOVAL, because a
        // bare Map already cannot tell an absent key from a key holding null.
        if let Some(TypeExpr::Nullable(inner)) = self.ty_node(val) {
            val = self.resolve_tyid(val.with_idx(inner))?;
        }
        let vn = match self.ty_node(val)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n,
            TypeExpr::Generic(var, _) => match self.spec_subst.get(&var) {
                Some(ty) => ty.nominal()?,
                _ => return None,
            },
            _ => return None,
        };
        // `nullable: false` is the point of the peel above -- the spine carries
        // presence, so the value columns never need a presence leaf.
        vec_column_elem(self.ws, vn, false).map(|name| ColumnElem { name, nullable: false })
    }

    /// `m.data`, the column Vec a column-stored Map's values live in. Every
    /// leaf read/write goes through the ordinary Vec column machinery on this.
    pub(crate) fn map_col_data(&mut self, m: Idx<Inst>) -> Idx<Inst> {
        self.emit_cur(Inst::Field { base: m, field: Ustr::from("data") })
    }

    /// The NativeBuffer container kind for base expr `e`: value-struct elements
    /// carry their name so the JS emitter can lower flat slots (Lua identical).
    pub(crate) fn nb_kind(&self, e: Idx<Expr>) -> ContainerKind {
        match self.nb_value_elem(e) {
            Some(elem) => ContainerKind::NativeBufferValue(elem),
            None => ContainerKind::NativeBuffer,
        }
    }

    /// Lower an index-expr base to `(base inst, container kind)`. A
    /// `FlatVec<T>` base indexes its `buf` NativeBuffer storage -- project the
    /// field and index with NativeBuffer semantics, so the value-element slot
    /// machinery and debug bounds check apply unchanged. Note `fv[i]` is
    /// therefore CAPACITY-checked in debug; `fv.get(i)` is the len-checked
    /// accessor.
    pub(crate) fn lower_index_base(
        &mut self,
        base: Idx<Expr>,
    ) -> Result<(Idx<Inst>, ContainerKind), CompilerError> {
        if self.expr_is_flat_vec(base) {
            let kind = self.nb_kind(base);
            let b = self.lower_expr(base)?;
            let buf = self.emit_cur(Inst::Field { base: b, field: Ustr::from("buf") });
            return Ok((buf, kind));
        }
        let kind = self.coll_kind(base);
        let b = self.lower_expr(base)?;
        Ok((b, kind))
    }

    /// The collection kind of a base expression, from its inferred type — drives
    /// the index/length shape (decision 9). bone's index/iter whitelist is closed
    /// (`Vec`/`Map`/`NativeBuffer`+sized), so any other type reaching here is a
    /// front-end/back-end disagreement, not a program the user could write.
    pub(crate) fn coll_kind(&self, base: Idx<Expr>) -> ContainerKind {
        match self.expr_ty(base) {
            Some(TypeExpr::App {
                ctor: TypeCtor::Array,
                ..
            }) => self.vec_kind(base),
            Some(TypeExpr::App {
                ctor: TypeCtor::Map,
                ..
            }) => ContainerKind::Map,
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) if is_native_buffer(name.as_str()) => self.nb_kind(base),
            _ => {
                let rendered = self
                    .inf
                    .and_then(|i| i.expr_types.get(base).copied())
                    .map(|t| self.ws.render(self.inf, t))
                    .unwrap_or_else(|| "<unknown>".to_string());
                unreachable!(
                    "coll_kind: expected an indexable container (Vec/Map/NativeBuffer); \
                     bone admitted an index/iter on `{rendered}`"
                )
            }
        }
    }

    // --- type queries (decision: ops are typed at BUILD time) ---

    pub(crate) fn arith_ty(&self, e: Idx<Expr>) -> Option<ArithTy> {
        self.arith_ty_of(self.expr_ty(e)?)
    }

    /// int->float subsumption funnel. Wraps `value` in [`Inst::IntToFloat`] iff
    /// bone typed `value_expr` int and `expected` resolves to float -- the single
    /// sink for general int->float coercion (`fn f(i: int) -> float { return i }`,
    /// `takes_float(int_expr)`, an int literal in a float slot). Fires ONLY
    /// int-expr-in-float-slot; never int->int (loop counters/bounds, idiv/imod
    /// operands, int==int stay untouched -- the counted-loop latch recognition
    /// keeps matching `Const(Lit::Int(1))`). Identity on luajit; materialized by
    /// non-Lua backends. See `docs/shaders.md`.
    pub(crate) fn coerced(&mut self, value: Idx<Inst>, value_expr: Idx<Expr>, expected: TyId) -> Idx<Inst> {
        let to_float = self.resolve_ty(expected).and_then(|t| self.arith_ty_of(t)) == Some(ArithTy::Float);
        self.coerce_i2f(value, self.arith_ty(value_expr) == Some(ArithTy::Int), to_float)
    }

    /// A variable binding's type: the inferred type, falling back to the declared
    /// annotation. The expected type at a `let`/assignment coercion sink and the
    /// [`Cell::ty`] for a binding cell.
    pub(crate) fn binding_ty(&self, name: Idx<BindingInfo>) -> Option<TyId> {
        self.inf
            .and_then(|i| i.binding_types.get(name).copied())
            .or(self.body.binding_arena[name].declared_ty)
    }

    /// [`Self::coerced`] against a `let`/assignment target binding's type.
    pub(crate) fn coerced_to_binding(&mut self, value: Idx<Inst>, value_expr: Idx<Expr>, name: Idx<BindingInfo>) -> Idx<Inst> {
        match self.binding_ty(name) {
            Some(t) => self.coerced(value, value_expr, t),
            None => value,
        }
    }

    /// [`Self::coerced`] keyed on already-computed operand classes -- the scalar-op
    /// path knows the operand's [`ArithTy`] and whether the resolved op is float,
    /// but has no single expected `TyId` to pass to `coerced`.
    pub(crate) fn coerce_i2f(&mut self, value: Idx<Inst>, from_int: bool, to_float: bool) -> Idx<Inst> {
        if from_int && to_float {
            self.emit_cur(Inst::IntToFloat(value))
        } else {
            value
        }
    }

    /// The declared type of a callee's positional parameter `pidx` (the expected
    /// type at a call-arg coercion sink), or `None` past the fixed arity.
    pub(crate) fn param_ty(&self, func: FuncRef, pidx: usize) -> Option<TyId> {
        self.ws.module(func.module).functions[func.func]
            .param_decls
            .get(pidx)
            .map(|pd| pd.ty)
    }
}
