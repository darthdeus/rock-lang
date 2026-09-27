//! `nobox` policy: the authorized-box mints (`materialize`, the `@gpu_only`
//! call boundary), the rejections that keep an implicit box from being emitted,
//! and the fn-value ABI compatibility check.
//!
//! This is the ENFORCEMENT side of the mark. The representation it enforces is
//! described in `shape`.
use super::*;

impl Builder<'_> {
    /// The caller reaches this only when the body keeps the boxed ABI
    /// (`BackendFnAbiPlan::scalar` is `None`).
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
        let Inst::MakeValueStruct { name, representation, form, fields } = &self.f.insts[inst]
        else {
            return;
        };
        let Some(sid) = self.ws.struct_by_name.get(name) else { return };
        if !self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
            return;
        }
        let (name, representation, form, fields) = (*name, *representation, *form, fields.clone());
        self.f.insts[inst] = Inst::MaterializeValueStruct {
            name,
            representation,
            form,
            fields,
        };
    }

    /// ARGUMENTS ONLY -- there is no return-position sibling, and that is an
    /// open hole, not a decision.
    ///
    /// A generic `-> V` instantiated at a `nobox` value crosses an opaque
    /// boundary unrejected and reads a key that does not exist: a `nobox` field
    /// flattens to per-leaf keys, so `field_get(row, .pos)` for a `Vec2` field
    /// emits `o["pos"]` and dies on nil, while `field_set` of that same field
    /// IS rejected here. The opaque-CONTAINER check below reads declared slots
    /// INCLUDING the return through the concrete callee signature; this one
    /// stops at the caller's argument expressions.
    ///
    /// Pinned with its controls and the measured exempt population in
    /// `crates/skull/tests/nobox_generic_return_boundary_sentinel.rs`. Owner:
    /// the `mono` branch (`docs/parked/mono.md` -> `docs/parked/inline-lua.md`).
    /// It blocks `docs/scene-system-plan.md` P4's FieldRef pull model, whose
    /// position descriptor is exactly `ReadFieldRef<T, Vec2>`.
    pub(crate) fn reject_nobox_boxed_call_args(
        &self,
        func: FuncRef,
        args: &[Idx<Expr>],
    ) -> Result<(), CompilerError> {
        if self.call_scx.values_first_class {
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
            if let Some(ty) = self.concrete_expr_ty(arg) {
                let Some(name) = ty.peel_nullable().0.nominal() else { continue };
                let Some(sid) = self.ws.struct_by_name.get(&name) else { continue };
                if self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
                    return Err(CompilerError::new_with_label(
                        self.body.expr_spans[arg],
                        format!(
                            "nobox value `{name}` cannot cross boxed call boundary `{}`",
                            callee.name
                        ),
                    ));
                }
                continue;
            }
        }
        Ok(())
    }

    /// An OPAQUE body cannot be handed a COLUMN container.
    ///
    /// A column-stored `[P]` is per-leaf arrays with no `_data` at all. Codegen
    /// routes its OWN container operations around the stdlib primitives -- a
    /// column `push` emits stores and no call -- but that routing covers only the
    /// hand-maintained primitive set and reaches nothing a user writes. An
    /// `inline(lua)`/`inline(js)` body is a string to the compiler and an
    /// `extern` has no body at all, so neither can be adapted to the
    /// representation: `v._data` is simply `nil`, and the call silently answers
    /// wrong or dies indexing it.
    ///
    /// Not a `nobox` rule, unlike its sibling: an UNMARKED value struct is
    /// column-eligible on the same terms and fails identically. The element type
    /// is the discriminant -- a boxed `[int]` keeps `_data` and is fine.
    ///
    /// Keyed on the DECLARED SEMANTIC param type under the concrete
    /// substitution, not the argument's (`docs/boundary-contracts.md` 2.4). A
    /// param spelled `[T]` or `Map<K, V>` states that the body expects that
    /// container's boxed representation, and a column-stored one is not it. A
    /// param spelled `any` states the opposite -- it claims no representation at
    /// all -- and the column-introspection probes that read `v["@vec"]`
    /// deliberately take `any`, so keying on the argument would reject the
    /// bodies that handle columns CORRECTLY.
    ///
    /// A method's IMPLICIT receiver is such a declared type: it is `param_decls`
    /// slot 0 and carries the semantic `Map<K, V>` though the source writes no
    /// type for it. Reading the written syntax instead would miss every method.
    ///
    /// The intercepted stdlib names are exempt BY CONSTRUCTION, not by an
    /// exception here: `lower_call` replaces them and returns before this check
    /// is reached.
    pub(crate) fn reject_column_container_opaque_slots(
        &self,
        e: Idx<Expr>,
        concrete_callee: &crate::instance::Instance,
    ) -> Result<(), CompilerError> {
        let plan = self.concrete_plan;
        let site = crate::site::Site {
            domain: self.cur_domain.clone(),
            expr: e,
            op: self.cur_op.clone(),
        };
        if plan.callees.get(&site) != Some(concrete_callee) {
            return Err(self.gap_here("concrete callee identity disagrees with its site plan"));
        }
        let signature = plan.callee_signatures.get(&site).ok_or_else(|| {
            self.gap_here("concrete callee signature is missing for its site")
        })?;
        self.reject_concrete_column_container_opaque_slots(e, concrete_callee.func, signature)
    }

    fn slot_hazard_mono(
        &self,
        mono: &crate::monoty::MonoTy,
    ) -> Result<Option<(String, &'static str)>, CompilerError> {
        let family = match mono {
            crate::monoty::MonoTy::App { ctor: crate::monoty::MonoCtor::Array, .. } => {
                Some(ContainerFamily::Vec)
            }
            crate::monoty::MonoTy::App { ctor: crate::monoty::MonoCtor::Map, .. } => {
                Some(ContainerFamily::Map)
            }
            crate::monoty::MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name), ..
            } => {
                if bone_semantic::prelude::is_intrinsic_key(*name, "Arena") {
                    Some(ContainerFamily::Arena)
                } else if bone_semantic::prelude::is_intrinsic_key(*name, "NativeBuffer") {
                    Some(ContainerFamily::NativeBuffer)
                } else if bone_semantic::prelude::is_intrinsic_key(*name, "FlatVec") {
                    Some(ContainerFamily::FlatVec)
                } else {
                    None
                }
            }
            _ => None,
        };
        let Some(family) = family else { return Ok(None) };
        let plan = self.container_plan_for_type(mono, family)?;
        let arena = self.scx.rep_arena();
        match plan.storage() {
            ContainerStorage::Columns { .. } => {
                let elem = plan
                    .column_elem(&arena)
                    .ok_or_else(|| gap("column container plan has no element shape"))?;
                let rendered = match plan.family() {
                    ContainerFamily::Vec => format!("[{}]", elem.name),
                    ContainerFamily::Map => format!("Map<_, {}>", elem.name),
                    ContainerFamily::Arena => format!("Arena<{}>", elem.name),
                    _ => unreachable!("only Vec, Map, and Arena use column storage"),
                };
                let physical = match plan.family() {
                    ContainerFamily::Vec => "`_data`",
                    ContainerFamily::Map => "a bare key table",
                    ContainerFamily::Arena => "column-backed data",
                    _ => unreachable!("only Vec, Map, and Arena use column storage"),
                };
                Ok(Some((rendered, physical)))
            }
            ContainerStorage::LeafAddressed { .. }
                if self.scx.target == bone_semantic::hir::FnBackend::Js => {
                let elem = plan
                    .ffi_element_name(&arena)
                    .unwrap_or_else(|| Ustr::from("<element>"));
                let family = match plan.family() {
                    ContainerFamily::NativeBuffer => "NativeBuffer",
                    ContainerFamily::FlatVec => "FlatVec",
                    _ => unreachable!("only native containers are leaf-addressed"),
                };
                Ok(Some((
                    format!("{family}<{elem}>"),
                    "a per-element view over `ptr`",
                )))
            }
            _ => Ok(None),
        }
    }

    fn reject_concrete_column_container_opaque_slots(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        signature: &crate::concrete::ConcreteSignature,
    ) -> Result<(), CompilerError> {
        let callee = &self.ws.module(func.module).functions[func.func];
        if callee.inline_body.is_none() && !callee.is_extern {
            return Ok(());
        }
        if callee.param_decls.len() != signature.params.len() {
            return Err(self.gap_here("concrete callee signature has the wrong parameter count"));
        }
        for (i, id) in signature.params.iter().enumerate() {
            if self.param_is_repr_opaque(func, i) {
                continue;
            }
            let mono = self.concrete_mono_view(*id, ConcreteMonoConsumer::NoboxBoundary);
            if let Some((rendered, physical)) = self.slot_hazard_mono(&mono)? {
                return Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    format!(
                        "column-stored `{rendered}` cannot cross opaque body boundary \
                         `{}` (an inline/extern body sees per-leaf columns, not \
                         {physical}; take `any` if the body reads the column layout \
                         itself, else use a boxed element type)",
                        callee.name
                    ),
                ));
            }
        }
        let Some(id) = signature.result else { return Ok(()) };
        let mono = self.concrete_mono_view(id, ConcreteMonoConsumer::NoboxBoundary);
        let Some((rendered, physical)) = self.slot_hazard_mono(&mono)? else {
            return Ok(());
        };
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "opaque body boundary `{}` cannot BUILD column-stored \
                 `{rendered}` (an inline/extern body hand-builds {physical}, which \
                 a column-planned result does not have; return `any` if the body \
                 builds the column layout itself, else use a boxed element type)",
                callee.name
            ),
        ))
    }

    /// Whether parameter `i` of `func` carries the `@repr(opaque)` contract.
    ///
    /// Read off `ParamDecl::annotations`, the declaration -- never off the
    /// call. The registry (`bone_core::annotations`) has already checked the
    /// kind and that the enclosing fn is an opaque boundary, so reaching here
    /// with the name present means the contract is valid.
    fn param_is_repr_opaque(&self, func: FuncRef, i: usize) -> bool {
        self.ws.module(func.module).functions[func.func]
            .param_decls
            .get(i)
            .is_some_and(|pd| pd.annotations.iter().any(|a| a.is_builtin("repr")))
    }


    /// A fn taken BY VALUE must have the ABI an indirect caller can derive.
    ///
    /// A definition reads its [`BackendFnAbiPlan`]; an indirect call site reads
    /// the same [`FunctionRepPlan`] projection from the function type. When the
    /// two disagree the program cannot be emitted correctly, so the mismatch is
    /// rejected rather than repaired with a second shape calculation.
    pub(crate) fn reject_fn_value_abi_mismatch(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Result<(), CompilerError> {
        let callee = &self.ws.module(func.module).functions[func.func];
        let plan = BackendFnAbiPlan::for_boundary_declaration(self.ws, self.call_scx, func);
        let ty = self
            .inf
            .and_then(|inf| inf.expr_types.get(e))
            .copied()
            .ok_or_else(|| gap("a function value has no inferred function type"))?;
        let value_plan = self
            .fn_value_rep_plan(ty)?
            .ok_or_else(|| gap("a function value does not have a function representation"))?;
        if plan.representation() == &value_plan {
            return Ok(());
        }
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "`{}` cannot be taken as a fn value because its fixed ABI does not match the \
                 type-derived `nobox` fn-value ABI",
                callee.name,
            ),
        ))
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
        if self.call_scx.values_first_class || !shape.splats() {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        let opaque = crate::lower::boundary_requires_boxed_representation(callee);
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
        inst: &crate::instance::Instance,
    ) -> Result<(), CompilerError> {
        if self.call_scx.values_first_class {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        // Same authorization as the arg direction: a `@gpu_only` intrinsic's
        // boxed return is not a real boundary (on the GPU it is a native vector,
        // and the Lua body only `error(...)`s).
        if crate::intrinsics::is_gpu_only(callee) {
            return Ok(());
        }
        let opaque = crate::lower::boundary_requires_boxed_representation(callee);
        if !opaque {
            return Ok(());
        }
        let Some(ret) = self.concrete_callee_return_ty(e, inst)? else {
            return Ok(());
        };
        let crate::monoty::MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            ..
        } = ret
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
        matches!(
            self.resolve_funcref(expr, name),
            Ok(func) if matches!(self.fn_expand_kind(func), Some(ExpandKind::Materialize))
        )
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
    pub(crate) fn coerce_field_repr(
        &mut self,
        r: ValueRepr,
        value_expr: Idx<Expr>,
        struct_name: Ustr,
        field: Ustr,
    ) -> Result<ValueRepr, CompilerError> {
        let expected = if self.struct_field_ty(struct_name, field).is_some() {
            let literal = self
                .cur_expr
                .ok_or_else(|| self.gap_here("concrete struct field has no literal expression"))?;
            let id = self
                .dispatch()
                .concrete_types
                .and_then(|tables| tables.struct_field_type(literal, field))
                .ok_or_else(|| {
                    self.gap_here(format!(
                        "concrete struct field plan is missing `{struct_name}.{field}`"
                    ))
                })?;
            Some(self.concrete_mono_view(id, ConcreteMonoConsumer::NoboxBoundary))
        } else {
            None
        };
        match r {
            ValueRepr::Scalar(s) => {
                let s = match expected {
                    Some(ref ty) => self.coerced_mono(s, value_expr, ty),
                    None => match self.struct_field_ty(struct_name, field) {
                        Some(fty) => self.coerced(s, value_expr, fty),
                        None => s,
                    },
                };
                Ok(ValueRepr::Scalar(s))
            }
            other => Ok(other),
        }
    }


    /// The scalar class of a resolved type, seeing through a TRANSPARENT newtype
    /// to its wrapped type (a `newtype Score(int)` is the raw int at runtime, so
    /// `<`/`+`/... on it are native int ops — matching its identity lowering).
    pub(crate) fn arith_ty_of(&self, te: TypeExpr) -> Option<ArithTy> {
        match te {
            TypeExpr::Builtin(BuiltinTy::Int) => Some(ArithTy::Int),
            TypeExpr::Builtin(BuiltinTy::Uint) => Some(ArithTy::Uint),
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

    pub(crate) fn nobox_value_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        self.dispatch().nobox_value_name(e)
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
        self.dispatch().is_nobox_struct(name)
    }

    pub(crate) fn nobox_value_shape_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<ValueShape>, CompilerError> {
        if self.scx.values_first_class {
            return Ok(None);
        }
        let Some(id) = self.expr_rep_id(e, RepUse::Value, true)? else { return Ok(None) };
        Ok(self.scx.rep_arena().value_shape(id))
    }

    pub(crate) fn nobox_binding_shape(
        &self,
        binding: Idx<BindingInfo>,
    ) -> Result<Option<ValueShape>, CompilerError> {
        let Some(ty) = self.binding_ty(binding) else { return Ok(None) };
        self.nobox_shape_of_ty(ty)
    }

    pub(crate) fn fn_value_rep_plan(
        &self,
        ty: TyId,
    ) -> Result<Option<FunctionRepPlan>, CompilerError> {
        if let Some(concrete) = self.concrete_ty_id(ty) {
            return self.fn_value_rep_plan_for_mono(&concrete);
        }
        Ok(None)
    }

    pub(crate) fn fn_value_rep_plan_for_mono(
        &self,
        ty: &MonoTy,
    ) -> Result<Option<FunctionRepPlan>, CompilerError> {
        if !matches!(ty.peel_nullable().0, MonoTy::Function { .. }) {
            return Ok(None);
        }
        BackendFnAbiPlan::for_function_value(self.ws, self.call_scx, ty)
            .map(Some)
            .map_err(|error| match error.span.or(self.cur_span) {
                Some(span) => CompilerError::new_with_label(span, error.message),
                None => CompilerError::new_no_location(error.message),
            })
    }

    pub(crate) fn nobox_shape_of_ty(
        &self,
        ty: TyId,
    ) -> Result<Option<ValueShape>, CompilerError> {
        if self.scx.values_first_class {
            return Ok(None);
        }
        self.rep_shape_for_ty(ty, RepUse::Value, true)
    }

    /// The leaf globals a place rooted at a `nobox` global denotes, plus the
    /// shape of the projected part (`None` = one primitive leaf). `SPOT` is the
    /// whole group, `SPOT.x` / `SPOT.a.b` a contiguous sub-range of it -- the
    /// leaf order is `repr_to_leaves`', so a projection is a slice, never a
    /// scatter. `Ok(None)` when the place is not rooted at a `nobox` global.
    /// A planning failure is an error rather than an ordinary-slot fallback.
    pub(crate) fn nobox_global_place(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<(Vec<Ustr>, Option<ValueShape>)>, CompilerError> {
        if self.scx.values_first_class {
            return Ok(None);
        }
        match self.body.expr_arena[e].clone() {
            // The DECLARED type, never `e`'s: a refined read of a `global X: NV2?`
            // has expr type `NV2`, whose 2-leaf shape would address the presence
            // leaf as `x`. Storage layout is a property of the global, not of the
            // read site.
            Expr::Global(name) => {
                let Some(&ty) = self.ws.globals_by_name.get(&name) else {
                    return Ok(None);
                };
                let plan = GlobalPlan::for_declared_type(self.ws, self.scx, ty)
                    .map_err(|e| self.gap_here(e.message))?;
                let Some(shape) = plan.value_shape() else {
                    return Ok(None);
                };
                // Minted from the global's emitted SYMBOL, matching
                // `state_schema::physical_leaf_names`: a v1 DefKey is not an
                // identifier, and these leaves ARE emitted Lua globals.
                let symbol = crate::mangle::symbol_of_key(name);
                let leaves =
                    (0..shape.leaf_count()).map(|i| nobox_global_leaf_name(symbol, i)).collect();
                Ok(Some((leaves, Some(shape))))
            }
            Expr::FieldAccess { base, field } => {
                let Some((leaves, Some(shape))) = self.nobox_global_place(base)? else {
                    return Ok(None);
                };
                let mut off = usize::from(shape.nullable);
                for (fname, fs) in &shape.fields {
                    let (n, sub) = match fs {
                        FieldShape::Prim => (1, None),
                        FieldShape::Nested(s) => (s.leaf_count(), Some(s.clone())),
                    };
                    if *fname == field {
                        return Ok(Some((leaves[off..off + n].to_vec(), sub)));
                    }
                    off += n;
                }
                Ok(None)
            }
            _ => Ok(None),
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
                self.repr_to_storage_leaves(&r, &shape, "nobox global store")?
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
