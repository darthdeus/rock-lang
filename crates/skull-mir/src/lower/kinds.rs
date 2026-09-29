//! Collection and type KIND queries the walk dispatches on: which vec/map/
//! native-buffer flavour a receiver is, element types behind a column, and the
//! numeric coercions (`int`->`float`, binding and param types).
use super::*;
use crate::monoty::MonoTy;
use std::rc::Rc;

impl Builder<'_> {
    pub(crate) fn container_plan_for_type(
        &self,
        ty: &MonoTy,
        family: ContainerFamily,
    ) -> Result<ContainerPlan, CompilerError> {
        ContainerPlan::for_type(self.ws, self.scx.target, &self.scx.rep_arena(), family, ty)
            .map(|plan| {
                if self.scx.row_value_containers {
                    plan.into_row_storage()
                } else {
                    plan
                }
            })
            .map_err(|error| match error.span.or(self.cur_span) {
                Some(span) => CompilerError::new_with_label(span, error.message),
                None => CompilerError::new_no_location(error.message),
            })
    }

    pub(crate) fn container_plan_for(
        &self,
        e: Idx<Expr>,
        family: ContainerFamily,
    ) -> Result<Option<ContainerPlan>, CompilerError> {
        let Some(ty) = self.concrete_ty(e)? else { return Ok(None) };
        if !ContainerPlan::matches_type(family, &ty) {
            return Ok(None);
        }
        self.container_plan_for_type(&ty, family).map(Some)
    }

    pub(crate) fn expr_is_flat_vec(&self, e: Idx<Expr>) -> bool {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let (payload, _) = ty.peel_nullable();
            return matches!(
                payload,
                MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(n), ..
                } if bone_semantic::prelude::is_intrinsic_key(*n, "FlatVec")
            );
        }
        false
    }

    /// The value-STRUCT element of `e`'s `NativeBuffer<E>` / `FlatVec<E>` type,
    /// or `None` when `E` is a primitive / unresolved generic / not value-shaped.
    /// Drives [`ContainerKind::NativeBufferValue`]; an opaque generic `E` stays
    /// plain `NativeBuffer` (matching the alloc intrinsic, which also requires a
    /// concrete element).
    pub(crate) fn nb_value_elem(&self, e: Idx<Expr>) -> Result<Option<Ustr>, CompilerError> {
        let Some(concrete) = self.concrete_expr_ty(e) else { return Ok(None) };
        let family = match concrete.peel_nullable().0 {
            MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name), ..
            } => {
                if bone_semantic::prelude::is_intrinsic_key(*name, "NativeBuffer") {
                    ContainerFamily::NativeBuffer
                } else if bone_semantic::prelude::is_intrinsic_key(*name, "FlatVec") {
                    ContainerFamily::FlatVec
                } else {
                    return Ok(None);
                }
            }
            _ => return Ok(None),
        };
        let arena = self.scx.rep_arena();
        let plan = self.container_plan_for_type(&concrete, family)?;
        if !plan.is_leaf_addressed() {
            return Ok(None);
        }
        Ok(plan.ffi_element_name(&arena))
    }

    /// The concrete `value`-struct element of `e`'s `NativeBuffer<E>` /
    /// `FlatVec<E>` TYPE, independent of the container's storage
    /// representation. [`Self::nb_value_elem`] answers the representation-gated
    /// question element access and literal leaf views need; the `@vertex_expand`
    /// expander consumes this one, because the vertex layout is derived from the
    /// element's C leaf layout whether the container stores rows, columns, or
    /// leaf views.
    pub(crate) fn nb_type_value_elem(&self, e: Idx<Expr>) -> Result<Option<Ustr>, CompilerError> {
        let Some(concrete) = self.concrete_expr_ty(e) else { return Ok(None) };
        let element = match concrete.peel_nullable().0 {
            MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name),
                params,
            } if matches!(name.as_str(), "NativeBuffer" | "FlatVec") => params.first(),
            _ => return Ok(None),
        };
        let Some(element) = element else { return Ok(None) };
        let (payload, nullable) = element.peel_nullable();
        if nullable {
            return Ok(None);
        }
        let crate::monoty::MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            ..
        } = payload
        else {
            return Ok(None);
        };
        let Some(&sid) = self.ws.struct_by_name.get(&name) else {
            return Ok(None);
        };
        if !has_value_plan(&self.ws.modules[sid.module.index()].structs[sid.def]) {
            return Ok(None);
        }
        let arena = self.scx.rep_arena();
        let rep = arena
            .plan_container_element(self.ws, self.scx.target, &payload)
            .map_err(|error| match error.span.or(self.cur_span) {
                Some(span) => CompilerError::new_with_label(span, error.message),
                None => CompilerError::new_no_location(error.message),
            })?;
        Ok(arena.value_shape(rep).map(|shape| shape.name))
    }

    /// The column-eligible element of `e`'s `Arena<E>` type, if any. The twin of
    /// [`Self::vec_column_elem_of`] one container out: an arena's `data` field is
    /// an ordinary `[E]`, so it columnizes on exactly the same rule, and the
    /// hardcoded `for`-over-arena walk has to read it the same way the Vec path
    /// would. A thin caller of the one oracle, like its Vec and Map siblings.
    pub(crate) fn arena_column_elem_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<ColumnElem>, CompilerError> {
        Ok(self
            .container_plan_for(e, ContainerFamily::Arena)?
            .and_then(|plan| plan.column_elem(&self.scx.rep_arena())))
    }

    /// The Vec container kind for base expr `e`: an all-scalar value-struct
    /// element stores as flat columns, everything else keeps the single `_data`
    /// backing. An unresolved / generic element is NOT eligible -- the columns
    /// need a concrete value-struct leaf list, and an opaque `T` has none.
    pub(crate) fn vec_kind(&self, e: Idx<Expr>) -> Result<ContainerKind, CompilerError> {
        if let Some(elem) = self.vec_column_elem_of(e)? {
            return Ok(ContainerKind::VecValueColumns(elem));
        }
        if let Some(elem) = self.vec_strided_elem_of(e)? {
            return Ok(ContainerKind::VecValueStrided(elem));
        }
        Ok(ContainerKind::Vec)
    }

    /// The column-eligible value-struct element of `e`'s `[E]` type, if any.
    ///
    /// Resolves through the active [`SpecSubst`], so inside a SPECIALIZED body
    /// (`VecIter<Vec2>.next`, `fn f<T>(xs: [T])` at a `[Vec2]` site) the element
    /// is the concrete struct rather than an opaque `T`. Without this the
    /// stdlib iterator lowers `it.vec[it.i]` against the boxed representation
    /// and reads a `_data` that column storage does not have.
    pub(crate) fn vec_column_elem_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<ColumnElem>, CompilerError> {
        Ok(self
            .container_plan_for(e, ContainerFamily::Vec)?
            .and_then(|plan| plan.column_elem(&self.scx.rep_arena())))
    }

    pub(crate) fn vec_column_elem_for_type(
        &self,
        ty: &MonoTy,
    ) -> Result<Option<ColumnElem>, CompilerError> {
        if !ContainerPlan::matches_type(ContainerFamily::Vec, ty) {
            return Ok(None);
        }
        self.container_plan_for_type(ty, ContainerFamily::Vec)
            .map(|plan| plan.column_elem(&self.scx.rep_arena()))
    }

    pub(crate) fn vec_strided_elem_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<ColumnElem>, CompilerError> {
        Ok(self
            .container_plan_for(e, ContainerFamily::Vec)?
            .and_then(|plan| plan.strided_elem(&self.scx.rep_arena())))
    }

    pub(crate) fn vec_strided_elem_for_type(
        &self,
        ty: &MonoTy,
    ) -> Result<Option<ColumnElem>, CompilerError> {
        if !ContainerPlan::matches_type(ContainerFamily::Vec, ty) {
            return Ok(None);
        }
        self.container_plan_for_type(ty, ContainerFamily::Vec)
            .map(|plan| plan.strided_elem(&self.scx.rep_arena()))
    }

    /// The column shape of `elem`: its value shape, marked nullable when the
    /// ELEMENT type is. Every column site builds its shape here, so the leaf
    /// wire (`repr_to_leaves` / `rebuild_from_leaves`, which both lead a nullable
    /// shape with presence) and the column keys ([`column_leaf_paths`]) can only
    /// agree.
    pub(crate) fn column_shape(&self, elem: ColumnElem) -> Result<ValueShape, CompilerError> {
        let shape = self
            .scx
            .rep_arena()
            .value_shape(elem.representation())
            .ok_or_else(|| gap("column element is not a value struct"))?;
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
    pub(crate) fn map_column_value_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<ColumnElem>, CompilerError> {
        Ok(self
            .container_plan_for(e, ContainerFamily::Map)?
            .and_then(|plan| plan.column_elem(&self.scx.rep_arena())))
    }

    /// The expression's CONCRETE type under the active specialization.
    ///
    /// Failure is not uniformly "boxed". Three outcomes, and which reason lands
    /// in which is the whole point -- reading them all as a negative picks a
    /// REPRESENTATION for a type the compiler could not resolve, and that is the
    /// silent fallback this codebase rejects.
    ///
    /// - `UnboundGeneric` -> `None`, correctly. This body is the SHARED generic
    ///   snapshot, lowered with no instantiation, so its element is opaque and
    ///   not column-eligible; it must keep the representation every
    ///   instantiation can read. Same answer the `TypeExpr` walk gave by falling
    ///   out of its `Generic` arm.
    /// - `UnresolvedInfer` -> `None`, and NOT because it is benign. It is the
    ///   S 0h decided rejection (a type argument no constraint reaches:
    ///   `let v = vec_new()` whose element is never used), which the instance
    ///   graph already records as `RejectionKind::UnconstrainedTypeVar` with a
    ///   span. Emission still runs the pre-mono path, so making it fatal HERE
    ///   would reject `tests/vec_sort_test.rock` and `tests/enumerate_tests.rock`
    ///   ahead of the cutover -- measured, both fail to emit. The rejection lands
    ///   with Stage 3, in one place, not twice on two schedules.
    /// - A missing `BodyInference`, or an expression bone typed nothing for ->
    ///   a spanned error for the same reason: absence of a type is not evidence
    ///   of a representation.
    /// - Everything else -> a spanned error. Recovery poison means the program
    ///   should have been rejected upstream, a missing node means a `TyId` names
    ///   nothing, and a depth limit means a cyclic substitution chain. None of
    ///   the three occurs over the corpus or the Rock suite, so this arm has
    ///   teeth without a population.
    pub(crate) fn concrete_ty(&self, e: Idx<Expr>) -> Result<Option<Rc<MonoTy>>, CompilerError> {
        if self.concrete_expr_is_untyped(e) {
            return Ok(None);
        }
        self.concrete_expr_ty(e)
            .map(Some)
            .ok_or_else(|| self.gap_here("concrete plan has no type for this expression"))
    }

    /// `m.data`, the column Vec a column-stored Map's values live in. Every
    /// leaf read/write goes through the ordinary Vec column machinery on this.
    pub(crate) fn map_col_data(&mut self, m: Idx<Inst>) -> Idx<Inst> {
        self.emit_cur(Inst::Field { base: m, field: Ustr::from("data") })
    }

    /// The NativeBuffer container kind for base expr `e`: value-struct elements
    /// carry their name so the JS emitter can lower flat slots (Lua identical).
    pub(crate) fn nb_kind(&self, e: Idx<Expr>) -> Result<ContainerKind, CompilerError> {
        match self.nb_value_elem(e)? {
            Some(elem) => Ok(ContainerKind::NativeBufferValue(elem)),
            None => Ok(ContainerKind::NativeBuffer),
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
            let kind = self.nb_kind(base)?;
            let b = self.lower_expr(base)?;
            let buf = self.emit_cur(Inst::Field { base: b, field: Ustr::from("buf") });
            return Ok((buf, kind));
        }
        let kind = self.coll_kind(base)?;
        let b = self.lower_expr(base)?;
        Ok((b, kind))
    }

    /// The collection kind of a base expression, from its inferred type — drives
    /// the index/length shape (decision 9). bone's index/iter whitelist is closed
    /// (`Vec`/`Map`/`NativeBuffer`+sized), so any other type reaching here is a
    /// front-end/back-end disagreement, not a program the user could write.
    pub(crate) fn coll_kind(&self, base: Idx<Expr>) -> Result<ContainerKind, CompilerError> {
        if let Some(ty) = self.concrete_expr_ty(base) {
            let (payload, _) = ty.peel_nullable();
            return match payload {
                MonoTy::App { ctor: crate::monoty::MonoCtor::Array, .. } => self.vec_kind(base),
                MonoTy::App { ctor: crate::monoty::MonoCtor::Map, .. } => Ok(ContainerKind::Map),
                MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } if is_native_buffer(*name) => Ok(self.nb_kind(base)?),
                _ => Err(self.gap_here("concrete type is not an indexable container")),
            };
        }
        if self.concrete_expr_is_untyped(base) && matches!(self.body.expr_arena[base], Expr::Array(..)) {
            return Ok(ContainerKind::Vec);
        }
        Err(self.gap_here("container expression is missing from the concrete type plan"))
    }

    // --- type queries (decision: ops are typed at BUILD time) ---

    pub(crate) fn arith_ty(&self, e: Idx<Expr>) -> Option<ArithTy> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            return self.arith_ty_of_mono(&ty);
        }
        self.arith_ty_of(self.expr_ty(e)?)
    }

    fn arith_ty_of_mono(&self, ty: &MonoTy) -> Option<ArithTy> {
        let (payload, _) = ty.peel_nullable();
        match payload {
            MonoTy::Int => Some(ArithTy::Int),
            MonoTy::Uint => Some(ArithTy::Uint),
            MonoTy::Float => Some(ArithTy::Float),
            MonoTy::String => Some(ArithTy::Str),
            MonoTy::App { ctor: crate::monoty::MonoCtor::Nominal(name), .. }
                if self.ws.newtypes_by_name.contains_key(name) =>
            {
                let nid = self.ws.newtypes_by_name.get(name)?;
                let wrapped = self.ws.modules[nid.module.index()].newtypes[nid.def].wrapped;
                self.arith_ty_of(self.resolve_ty(wrapped)?)
            }
            _ => None,
        }
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
        let to_float = self
            .concrete_ty_id(expected)
            .as_ref()
            .and_then(|t| self.arith_ty_of_mono(t))
            == Some(ArithTy::Float);
        self.coerce_numeric_to_float(
            value,
            self.arith_ty(value_expr).unwrap_or(ArithTy::Str),
            to_float,
        )
    }

    pub(crate) fn coerced_mono(
        &mut self,
        value: Idx<Inst>,
        value_expr: Idx<Expr>,
        expected: &MonoTy,
    ) -> Idx<Inst> {
        self.coerce_numeric_to_float(
            value,
            self.arith_ty(value_expr).unwrap_or(ArithTy::Str),
            self.arith_ty_of_mono(expected) == Some(ArithTy::Float),
        )
    }

    /// A variable binding's type: the inferred type, falling back to the declared
    /// annotation. The expected type at a `let`/assignment coercion sink and the
    /// [`Cell::ty`] for a binding cell.
    pub(crate) fn binding_ty(&self, name: Idx<BindingInfo>) -> Option<TyId> {
        self.inf
            .and_then(|i| i.binding_types.get(name).copied())
            .or(self.body.binding_arena[name].declared_ty)
    }

    pub(crate) fn binding_is_ref(&self, name: Idx<BindingInfo>) -> bool {
        self.binding_ty(name)
            .and_then(|ty| self.concrete_ty_id(ty))
            .is_some_and(|ty| matches!(ty.as_ref(), MonoTy::App { ctor: MonoCtor::Ref, .. }))
    }

    /// [`Self::coerced`] against a `let`/assignment target binding's type.
    pub(crate) fn coerced_to_binding(&mut self, value: Idx<Inst>, value_expr: Idx<Expr>, name: Idx<BindingInfo>) -> Idx<Inst> {
        match self.binding_ty(name) {
            Some(t) => self.coerced(value, value_expr, t),
            None => value,
        }
    }

    pub(crate) fn coerce_numeric_to_float(
        &mut self,
        value: Idx<Inst>,
        from: ArithTy,
        to_float: bool,
    ) -> Idx<Inst> {
        if !to_float {
            return value;
        }
        match from {
            ArithTy::Int => self.emit_cur(Inst::IntToFloat(value)),
            ArithTy::Uint => self.emit_cur(Inst::UintToFloat(value)),
            _ => value,
        }
    }

}
