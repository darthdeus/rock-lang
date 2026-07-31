//! `nobox` policy: the authorized-box mints (`materialize`, the `@gpu_only`
//! call boundary), the rejections that keep an implicit box from being emitted,
//! and the fn-value ABI compatibility check.
//!
//! This is the ENFORCEMENT side of the mark. The representation it enforces is
//! described in `shape`.
use super::*;

impl Builder<'_> {
    /// The caller reaches this only when [`fn_scalar_abi_spec`] returns `None`.
    /// Under `values_first_class`, a `value` struct is already a first-class
    /// `vecN`, so there is no box to authorize.
    /// Re-mark a nobox box packed for a `@gpu_only` callee as an EXPLICIT one.
    ///
    /// Emission is identical (both render `<name>_ct`), the verifier ignores
    /// `MaterializeValueStruct`, and the flatten pass already treats it as
    /// sanctioned -- so this authorizes exactly the box the GPU boundary needs
    /// through the channel that already exists for authorized boxes, instead of
    /// switching the guarantee off for the calling body.
    ///
    /// Args are lowered per call site, so each box here is this call's own.
    pub(crate) fn authorize_gpu_boundary_boxes(&mut self, func: &FuncRef, args: &[Idx<Inst>]) {
        let callee = &self.ws.module(func.module).functions[func.func];
        if !crate::intrinsics::is_gpu_only(callee) {
            return;
        }
        for &a in args {
            self.authorize_nobox_box(a);
        }
    }

    /// Re-mark one implicit nobox box as AUTHORIZED, in place. The single
    /// conversion point: every boundary the compiler MINTS a sanctioned box at
    /// goes through here, so "which boxes are authorized" is one list of callers
    /// rather than a repeated `MakeValueStruct` -> `MaterializeValueStruct`
    /// rewrite. A no-op on anything that is not an implicit box of a `nobox`
    /// struct.
    pub(crate) fn authorize_nobox_box(&mut self, inst: Idx<Inst>) {
        let Inst::MakeValueStruct { name, fields } = &self.f.insts[inst] else { return };
        let Some(sid) = self.ws.struct_by_name.get(name) else { return };
        if !self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
            return;
        }
        let (name, fields) = (*name, fields.clone());
        self.f.insts[inst] = Inst::MaterializeValueStruct { name, fields };
    }

    pub(crate) fn reject_nobox_boxed_call_args(
        &self,
        func: FuncRef,
        args: &[Idx<Expr>],
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        // A `@gpu_only` intrinsic's boxed param is not a real boundary: on the
        // GPU the arg is a native `vec2`/`vec4` and no box exists, and the Lua
        // body only `error(...)`s. Rather than exempting the CALLER's whole
        // body, the box is AUTHORIZED at this one boundary
        // ([`Self::authorize_gpu_boundary_boxes`]) -- the guarantee keeps
        // applying to everything else the caller does.
        // Rationale for shader work: docs/shaders.md, "nobox values and shaders".
        if crate::intrinsics::is_gpu_only(callee) {
            return Ok(());
        }
        for &arg in args {
            if self.is_explicit_materialize_call(arg) {
                continue;
            }
            let Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) = self.expr_ty(arg)
            else {
                continue;
            };
            let Some(sid) = self.ws.struct_by_name.get(&name) else {
                continue;
            };
            if self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
                return Err(CompilerError::new_with_label(
                    self.body.expr_spans[arg],
                    format!(
                        "nobox value `{name}` cannot cross boxed call boundary `{}`",
                        callee.name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The return-position companion of [`Self::reject_nobox_boxed_call_args`]:
    /// a `nobox` element only rides a tuple return as leaves if skull emits the
    /// callee's body. An `extern`, or an `inline(lua)` body that has not opted
    /// in with `@scalar_abi`, writes its own `return` -- so the call site would
    /// read leaves the body never produced. That is a boxed boundary; reject it
    /// where an argument would be rejected.
    pub(crate) fn reject_nobox_opaque_tuple_return(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        shape: &TupleRetShape,
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class || !shape.splats() {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        let opaque = callee.is_extern
            || (callee.inline_body.is_some()
                && !callee.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi"));
        if !opaque {
            return Ok(());
        }
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "nobox value cannot cross the boxed tuple return of `{}` \
                 (an `inline(lua)` body must opt in with `@scalar_abi` and return the leaves)",
                callee.name
            ),
        ))
    }

    /// The BARE-return companion of [`Self::reject_nobox_opaque_tuple_return`].
    /// That one covers `-> (bool, NV2)`; a plain `-> NV2` had no check in any
    /// position, so an opaque body handing back a hand-built box compiled clean
    /// AND PASSED -- Lua's `.field` does not distinguish a table from cdata, so
    /// the guarantee was violated with no diagnostic. Live shape:
    /// `stone_measure_text_vec2_for` and its `inline(js)` twin.
    ///
    /// Keyed on the SIGNATURE, which is transport-independent -- the JS backend
    /// packs leaves into an array rather than multi-returning them, and this
    /// check is correct for both.
    pub(crate) fn reject_nobox_opaque_value_return(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        // Same authorization as the arg direction: a `@gpu_only` intrinsic's
        // boxed return is not a real boundary (on the GPU it is a native vector,
        // and the Lua body only `error(...)`s).
        if crate::intrinsics::is_gpu_only(callee) {
            return Ok(());
        }
        let opaque = callee.is_extern
            || (callee.inline_body.is_some()
                && !callee.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi"));
        if !opaque {
            return Ok(());
        }
        let Some(ret) = callee.return_ty else {
            return Ok(());
        };
        let Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        }) = static_ty(self.ws, ret)
        else {
            return Ok(());
        };
        let Some(sid) = self.ws.struct_by_name.get(&name) else {
            return Ok(());
        };
        if !self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
            return Ok(());
        }
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "nobox value `{name}` cannot cross the boxed return of `{}` \
                 (an `inline(lua)` body must opt in with `@scalar_abi` and return the leaves)",
                callee.name
            ),
        ))
    }

    pub(crate) fn is_explicit_materialize_call(&self, expr: Idx<Expr>) -> bool {
        let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            ..
        } = self.body.expr_arena[expr]
        else {
            return false;
        };
        self.resolve_funcref(expr, name)
            .ok()
            .is_some_and(|func| matches!(self.fn_expand_kind(func), Some(ExpandKind::Materialize)))
    }

    /// A (value) struct field's declared type, for the value-struct-literal
    /// construction sink (`Vec2 { x: 0 }` -> the `x` field coerces int->float).
    pub(crate) fn struct_field_ty(&self, name: Ustr, field: Ustr) -> Option<TyId> {
        let sid = *self.ws.struct_by_name.get(&name)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        sd.fields.iter().find(|f| f.name == field).map(|f| f.ty)
    }

    /// Coerce a value-struct field's repr int->float at CONSTRUCTION (before any
    /// scalar-ABI splat, per the plan): only a `Scalar` leaf can be an int; a
    /// `Group` / nested-value repr is already composite.
    pub(crate) fn coerce_field_repr(&mut self, r: ValueRepr, value_expr: Idx<Expr>, struct_name: Ustr, field: Ustr) -> ValueRepr {
        match r {
            ValueRepr::Scalar(s) => match self.struct_field_ty(struct_name, field) {
                Some(fty) => ValueRepr::Scalar(self.coerced(s, value_expr, fty)),
                None => ValueRepr::Scalar(s),
            },
            other => other,
        }
    }


    /// The scalar class of a resolved type, seeing through a TRANSPARENT newtype
    /// to its wrapped type (a `newtype Score(int)` is the raw int at runtime, so
    /// `<`/`+`/... on it are native int ops — matching its identity lowering).
    pub(crate) fn arith_ty_of(&self, te: TypeExpr) -> Option<ArithTy> {
        match te {
            TypeExpr::Builtin(BuiltinTy::Int) => Some(ArithTy::Int),
            TypeExpr::Builtin(BuiltinTy::Float) => Some(ArithTy::Float),
            TypeExpr::Builtin(BuiltinTy::String) => Some(ArithTy::Str),
            TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }
                if self.ws.newtypes_by_name.contains_key(&name) =>
            {
                let nid = self.ws.newtypes_by_name.get(&name)?;
                let wrapped = self.ws.modules[nid.module.index()].newtypes[nid.def].wrapped;
                self.arith_ty_of(self.resolve_ty(wrapped)?)
            }
            _ => None,
        }
    }

    pub(crate) fn eq_is_scalar(&self, e: Idx<Expr>) -> bool {
        matches!(
            self.expr_ty(e),
            Some(TypeExpr::Builtin(
                BuiltinTy::Int | BuiltinTy::Float | BuiltinTy::Bool | BuiltinTy::String
            ))
        ) || self.is_payloadless_enum(e)
    }

    pub(crate) fn nobox_value_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        let TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } = self.expr_ty(e)?
        else {
            return None;
        };
        self.is_nobox_struct(name).then_some(name)
    }

    /// Under `values_first_class` NOTHING is nobox: the flag says the target has no
    /// boxed representation to avoid, so a `value` struct is already a first-class
    /// `vecN` and there is no leaf form to prefer. Answering `true` here would put
    /// a local into leaf cells named `<name>__<i>`, which is both a duplicated
    /// initializer and a RESERVED GLSL identifier (spec 3.7).
    ///
    /// The gate lives on the predicate rather than on each shape helper because
    /// every representation decision reaches nobox-ness through here -- binding
    /// shapes, expression shapes, signature splat gates and the column-storage
    /// rejections alike. Gating one consumer leaves the others to remember it, and
    /// they did not: `Statement::Let` falls through `nobox_binding_shape` to
    /// `nobox_value_shape_of`, so gating the binding shape alone still emitted
    /// `shift__0`.
    pub(crate) fn is_nobox_struct(&self, name: Ustr) -> bool {
        if self.scx.values_first_class {
            return false;
        }
        self.ws
            .struct_by_name
            .get(&name)
            .is_some_and(|sid| self.ws.modules[sid.module.index()].structs[sid.def].is_nobox)
    }

    pub(crate) fn nobox_value_shape_of(&self, e: Idx<Expr>) -> Option<ValueShape> {
        let mut shape = value_shape_of_nominal(self.ws, self.nobox_value_name(e)?)?;
        shape.nullable = self.expr_is_nullable(e);
        Some(shape)
    }

    pub(crate) fn nobox_binding_shape(&self, binding: Idx<BindingInfo>) -> Option<ValueShape> {
        self.nobox_shape_of_ty(self.binding_ty(binding)?)
    }

    /// The leaf shape of a `nobox`-typed `ty` (nullable included: presence is
    /// leaf 0), `None` for every other type.
    /// Argument positions of a `ShaderPipeline` expansion, where a bare fn name is
    /// consumed by the compiler and never becomes a runtime fn value:
    /// `rock_shader(frag)` lowers its argument to the entry's NAME as a string
    /// literal, so no function-value ABI applies there.
    pub(crate) fn expansion_arg_exprs(&self) -> HashSet<Idx<Expr>> {
        let mut out = HashSet::new();
        for (e, expr) in self.body.expr_arena.iter() {
            let Expr::Call { callee, args } = expr else { continue };
            let Callee::Unresolved { name, .. } = callee else { continue };
            let Ok(func) = self.resolve_funcref(e, *name) else { continue };
            // ONLY `ShaderPipeline`. It resolves its argument to a fn NAME and emits a
            // string literal, so the name never becomes a runtime value. The sibling
            // kinds evaluate theirs -- `expand_materialize` calls `lower_expr_repr` on
            // it -- so exempting them let `materialize(<nobox-carrying fn>)` bypass the
            // ABI rejection entirely, which is a hole, not a narrowing.
            if matches!(self.fn_expand_kind(func), Some(ExpandKind::ShaderPipeline)) {
                out.extend(args.clone().into_iter());
            }
        }
        out
    }

    pub(crate) fn reject_incompatible_nobox_fn_value_abi(&self) -> Result<(), CompilerError> {
        if self.scx.values_first_class {
            return Ok(());
        }
        let expansion_args = self.expansion_arg_exprs();
        for (e, expr) in self.body.expr_arena.iter() {
            let Expr::Global(name) = expr else { continue };
            if expansion_args.contains(&e) {
                continue;
            }
            let Some(overloads) = self.ws.functions_by_name.get(name) else { continue };
            let incompatible = overloads.iter().any(|(mid, fid)| {
                let fref = FuncRef { module: *mid, func: *fid };
                let expected = fn_nobox_abi_spec(self.ws, fref, &SpecSubst::new());
                expected.is_some() && fn_scalar_abi(self.ws, self.scx, fref) != expected
            });
            if incompatible {
                return Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    format!(
                        "`{name}` cannot be used as a fn value because its fixed ABI does not \
                         match the type-derived `nobox` fn-value ABI"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The leaf ABI of a FN TYPE, or `None` when nothing in the signature is a
    /// `nobox` value and the boxed fn-value ABI stands unchanged.
    ///
    /// Keyed on the TYPE and nothing else. An indirect call site cannot see its
    /// callee, so the definition, a direct call and a call through a fn value
    /// can only agree by all three reading the same signature -- the rule the
    /// tuple-return ABI already follows (`lambda_ret_tuple_shape` next to
    /// `expr_tuple_shape`). Any callee-identity test here would make the two
    /// ends disagree exactly where the callee is opaque, which is the case that
    /// matters.
    ///
    /// Narrowed to `nobox` types on purpose: an ordinary value struct has no
    /// guarantee to keep, so widening this to every FFI-clean value would
    /// re-ABI every lambda in the tree for no correctness gain. Both ends
    /// compute the same predicate from the same type, so they still agree.
    pub(crate) fn fn_value_abi(&self, ty: TyId) -> Option<(Vec<RetSlot>, Option<ValueShape>)> {
        let ty = self.resolve_tyid(ty)?;
        let TypeExpr::Function { params, ret } = self.ty_node(ty)? else { return None };
        let ps: Vec<RetSlot> =
            params.into_iter().map(|p| self.nobox_param_slot_of_ty(ty.with_idx(p))).collect();
        let rs = self.nobox_shape_of_ty(ty.with_idx(ret));
        if rs.is_none() && !ps.iter().any(RetSlot::splats) {
            return None;
        }
        Some((ps, rs))
    }

    /// [`Self::nobox_shape_of_ty`] widened to the param SLOT vocabulary, so a
    /// TUPLE param carrying a `nobox` element rides its leaves like any other
    /// splatting slot. Both ends of a fn value read this same predicate off the
    /// same type, so they agree without either seeing the other.
    pub(crate) fn nobox_param_slot_of_ty(&self, ty: TyId) -> RetSlot {
        if let Some(shape) = self.nobox_shape_of_ty(ty) {
            return RetSlot::Leaves(shape);
        }
        match self.tyid_tuple_ret_shape(ty) {
            Some(inner) if inner.splats() => RetSlot::Nested(inner),
            _ => RetSlot::Opaque,
        }
    }

    /// [`Self::fn_value_abi`] for the fn VALUE an expression denotes.
    pub(crate) fn expr_fn_value_abi(
        &self,
        e: Idx<Expr>,
    ) -> Option<(Vec<RetSlot>, Option<ValueShape>)> {
        self.fn_value_abi(*self.inf?.expr_types.get(e)?)
    }

    pub(crate) fn nobox_shape_of_ty(&self, ty: TyId) -> Option<ValueShape> {
        // The BODY arena's spelling of [`nullable_nominal_shape`]: same rule, its
        // own type-node lookup. `is_nobox_struct` (not `is_nobox_nominal`) is the
        // one real difference -- it also honours `scx.values_first_class`, which
        // the GLSL backend sets -- so the guard stays here and the core sees an
        // already-filtered nominal.
        if !self.scx.values_first_class {
            let mut shape = nullable_nominal_shape(self.ws, ty, &|t| {
                self.ty_node(self.resolve_tyid(t)?)
            })?;
            // A generic bound to `E?` reaches the core as its payload nominal, so
            // the `?` has to be re-applied from the binding. Idempotent for a
            // spelled-out `E?`, which the core already marked.
            shape.nullable |= self.is_nullable_ty(ty);
            return Some(shape);
        }
        None
    }

    /// The leaf globals a place rooted at a `nobox` global denotes, plus the
    /// shape of the projected part (`None` = one primitive leaf). `SPOT` is the
    /// whole group, `SPOT.x` / `SPOT.a.b` a contiguous sub-range of it -- the
    /// leaf order is `repr_to_leaves`', so a projection is a slice, never a
    /// scatter. `None` when the place is not rooted at a `nobox` global.
    pub(crate) fn nobox_global_place(&self, e: Idx<Expr>) -> Option<(Vec<Ustr>, Option<ValueShape>)> {
        match self.body.expr_arena[e].clone() {
            // The DECLARED type, never `e`'s: a refined read of a `global X: NV2?`
            // has expr type `NV2`, whose 2-leaf shape would address the presence
            // leaf as `x`. Storage layout is a property of the global, not of the
            // read site.
            Expr::Global(name) => {
                let shape = nobox_declared_shape(self.ws, *self.ws.globals_by_name.get(&name)?)?;
                let leaves =
                    (0..shape.leaf_count()).map(|i| nobox_global_leaf_name(name, i)).collect();
                Some((leaves, Some(shape)))
            }
            Expr::FieldAccess { base, field } => {
                let (leaves, Some(shape)) = self.nobox_global_place(base)? else {
                    return None;
                };
                let mut off = usize::from(shape.nullable);
                for (fname, fs) in &shape.fields {
                    let (n, sub) = match fs {
                        FieldShape::Prim => (1, None),
                        FieldShape::Nested(s) => (s.leaf_count(), Some(s.clone())),
                    };
                    if *fname == field {
                        return Some((leaves[off..off + n].to_vec(), sub));
                    }
                    off += n;
                }
                None
            }
            _ => None,
        }
    }

    /// Read a `nobox` global place back as an unboxed repr.
    pub(crate) fn nobox_global_load(
        &mut self,
        leaves: &[Ustr],
        shape: Option<ValueShape>,
    ) -> ValueRepr {
        let vals: Vec<Idx<Inst>> =
            leaves.iter().map(|g| self.emit_cur(Inst::Global(*g))).collect();
        match shape {
            Some(shape) => {
                let mut idx = 0;
                self.rebuild_from_leaves(&shape, &vals, &mut idx)
            }
            None => ValueRepr::Scalar(vals[0]),
        }
    }

    /// Write `value` into a `nobox` global place, leaf by leaf.
    pub(crate) fn nobox_global_store(
        &mut self,
        leaves: &[Ustr],
        shape: Option<ValueShape>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let vals = match shape {
            Some(shape) => {
                let r = self.lower_expr_repr(value)?;
                self.repr_to_leaves(&r, &shape)
            }
            None => vec![self.lower_expr(value)?],
        };
        for (g, v) in leaves.iter().zip(vals) {
            self.emit_cur(Inst::StoreGlobal { name: *g, value: v });
        }
        Ok(())
    }

    pub(crate) fn emit_all_equal(&mut self, lhs: &[Idx<Inst>], rhs: &[Idx<Inst>]) -> Idx<Inst> {
        debug_assert_eq!(lhs.len(), rhs.len());
        let result = self.f.new_cell(None);
        let fail = self.f.new_block();
        let join = self.f.new_block();
        for (&a, &b) in lhs.iter().zip(rhs) {
            let equal = self.emit_cur(Inst::Eq(a, b));
            let next = self.f.new_block();
            self.f.set_term(
                self.cur,
                Term::Branch {
                    cond: equal,
                    then_bb: next,
                    else_bb: fail,
                },
            );
            self.cur = next;
        }
        let yes = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.emit_cur(Inst::Store {
            cell: result,
            value: yes,
        });
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = fail;
        let no = self.emit_cur(Inst::Const(Lit::Bool(false)));
        self.emit_cur(Inst::Store {
            cell: result,
            value: no,
        });
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = join;
        self.emit_cur(Inst::Load(result))
    }
}
