use super::*;

impl<'a> Builder<'a> {
    pub(crate) fn authorize_gpu_boundary_boxes(&mut self, func: &FuncRef, args: &[Idx<Inst>]) {
        let callee = &self.ws.module(func.module).functions[func.func];
        if !crate::intrinsics::is_gpu_only(callee) {
            return;
        }
        for &a in args {
            self.authorize_nobox_box(a);
        }
    }

    pub(crate) fn authorize_nobox_box(&mut self, inst: Idx<Inst>) {
        let Inst::MakeValueStruct {
            name,
            representation,
            form,
            fields,
        } = &self.f.insts[inst]
        else {
            return;
        };
        if !is_nobox_nominal(self.ws, *name) {
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

    pub(crate) fn reject_nobox_boxed_call_args(
        &mut self,
        func: FuncRef,
        args: &[Idx<Expr>],
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        // GPU-only calls use native values in GLSL and never execute on Lua.
        if crate::intrinsics::is_gpu_only(callee) {
            return Ok(());
        }
        // Only an opaque body reads a tuple box by its user layout; the stdlib's
        // opaque primitives hold a splatting tuple's flat box without reading it.
        let foreign_body = (callee.inline_body.is_some() || callee.is_extern)
            && !bone_core::modules::is_stdlib_module_file(
                self.ws.module(func.module).file.as_str(),
            );
        for &arg in args {
            if self.is_explicit_materialize_call(arg) {
                if foreign_body {
                    self.boundary_materializes.insert(arg);
                }
                continue;
            }
            // A flat element `ref` passes its container and slot, never a box.
            if self.is_flat_ref_call_arg(arg) {
                continue;
            }
            if foreign_body && self.tuple_box_shape(arg)?.is_some() {
                return Err(CompilerError::new_with_label(
                    self.body.expr_spans[arg],
                    format!(
                        "a tuple carrying a nobox value cannot cross boxed call boundary `{}` \
                         (`materialize` it to pass one boxed value per element)",
                        self.ws.module(func.module).functions[func.func].name
                    ),
                ));
            }
            if let Some(ty) = self.concrete_expr_ty(arg) {
                let Some(name) = ty.peel_nullable().0.nominal() else {
                    continue;
                };
                if is_nobox_nominal(self.ws, name) {
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

    /// Opaque bodies may assume row storage, including inside tuple parameters.
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
        let callee = plan.callees.get(&site).ok_or_else(|| {
            self.gap_here("concrete callee identity disagrees with its site plan")
        })?;
        if &callee.instance != concrete_callee {
            return Err(self.gap_here("concrete callee identity disagrees with its site plan"));
        }
        self.reject_concrete_column_container_opaque_slots(
            e,
            concrete_callee.func,
            &callee.signature,
        )
    }

    /// Nullable and tuple slots can still carry the same underlying container.
    fn param_slot_hazard_mono(
        &self,
        mono: &crate::monoty::MonoTy,
        leaf_views_rebuilt: bool,
    ) -> Result<Option<(String, &'static str)>, CompilerError> {
        match mono {
            crate::monoty::MonoTy::Nullable(inner) => {
                self.param_slot_hazard_mono(inner, leaf_views_rebuilt)
            }
            crate::monoty::MonoTy::Tuple(elems) => {
                for elem in elems {
                    if let Some(hazard) = self.param_slot_hazard_mono(elem, leaf_views_rebuilt)? {
                        return Ok(Some(hazard));
                    }
                }
                Ok(None)
            }
            _ => self.slot_hazard_mono(mono, leaf_views_rebuilt),
        }
    }

    fn slot_hazard_mono(
        &self,
        mono: &crate::monoty::MonoTy,
        leaf_views_rebuilt: bool,
    ) -> Result<Option<(String, &'static str)>, CompilerError> {
        let family = match mono {
            crate::monoty::MonoTy::App {
                ctor: crate::monoty::MonoCtor::Array,
                ..
            } => Some(ContainerFamily::Vec),
            crate::monoty::MonoTy::App {
                ctor: crate::monoty::MonoCtor::Map,
                ..
            } => Some(ContainerFamily::Map),
            crate::monoty::MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name),
                ..
            } => {
                if bone_core::modules::is_intrinsic_key(*name, "Arena") {
                    Some(ContainerFamily::Arena)
                } else if bone_core::modules::is_intrinsic_key(*name, "NativeBuffer") {
                    Some(ContainerFamily::NativeBuffer)
                } else if bone_core::modules::is_intrinsic_key(*name, "FlatVec") {
                    Some(ContainerFamily::FlatVec)
                } else {
                    None
                }
            }
            _ => None,
        };
        let Some(family) = family else {
            return Ok(None);
        };
        let plan = self.container_plan_for_type(mono, family)?;
        let arena = self.scx.rep_arena();
        match plan.storage() {
            ContainerStorage::Columns { .. } => {
                let elem = plan
                    .column_elem(arena)
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
                if self.scx.target == bone_core::hir::FnBackend::Js && !leaf_views_rebuilt =>
            {
                let elem = plan
                    .ffi_element_name(arena)
                    .unwrap_or_else(|| crate::names::ustr("<element>"));
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
        // The JS `deepcopy` runtime rebuilds every TypedArray over one copy of its
        // buffer, so a leaf-addressed container comes back with views over its own storage.
        let leaf_views_rebuilt =
            callee.name == "deepcopy" && crate::intrinsics::is_intrinsic(callee);
        for (i, id) in signature.params.iter().enumerate() {
            if self.param_is_repr_opaque(func, i) {
                continue;
            }
            let mono = self.concrete_mono(*id);
            if let Some((rendered, physical)) =
                self.param_slot_hazard_mono(mono, leaf_views_rebuilt)?
            {
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
        let Some(id) = signature.result else {
            return Ok(());
        };
        let mono = self.concrete_mono(id);
        let Some((rendered, physical)) = self.slot_hazard_mono(mono, leaf_views_rebuilt)? else {
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

    fn param_is_repr_opaque(&self, func: FuncRef, i: usize) -> bool {
        self.ws.module(func.module).functions[func.func]
            .param_decls
            .get(i)
            .is_some_and(|pd| pd.annotations.iter().any(|a| a.is_builtin("repr")))
    }

    /// A named function value must match the ABI derived from its type.
    pub(crate) fn reject_fn_value_abi_mismatch(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Result<(), CompilerError> {
        let callee = &self.ws.module(func.module).functions[func.func];
        let plan = BackendFnAbiPlan::for_boundary_declaration(self.ws, self.scx, func);
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

    pub(crate) fn reject_nobox_opaque_tuple_return(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        shape: &TupleLayout,
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class || !shape.splats(self.scx.rep_arena()) {
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

    pub(crate) fn reject_nobox_opaque_value_return(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        inst: &crate::instance::Instance,
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        // A `@gpu_only` intrinsic's boxed return is no real boundary: on the GPU it is
        // a native vector, and the Lua body only errors.
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
        if !is_nobox_nominal(self.ws, name) {
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

    pub(crate) fn struct_field_ty(&self, name: Ustr, field: Ustr) -> Option<TyId> {
        let sid = *self.ws.struct_by_name.get(&name)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        sd.fields.iter().find(|f| f.name == field).map(|f| f.ty)
    }

    pub(crate) fn field_is_f32(&self, name: Ustr, field: Ustr) -> bool {
        self.ws.struct_by_name.get(&name).is_some_and(|sid| {
            self.ws.modules[sid.module.index()].structs[sid.def].fields.iter().any(|f| {
                f.name == field
                    && matches!(
                        f.storage_ty,
                        TyId::Static(m, i)
                            if matches!(self.ws.module(m).ty_arena[i], TypeExpr::Builtin(BuiltinTy::F32))
                    )
            })
        })
    }

    pub(crate) fn round_f32_field(
        &mut self,
        name: Ustr,
        field: Ustr,
        value: Idx<Inst>,
    ) -> Idx<Inst> {
        if self.field_is_f32(name, field) {
            self.emit_cur(Inst::FloatToF32(value))
        } else {
            value
        }
    }

    /// Coerces at construction, before any scalar-ABI splat; only a `Scalar` leaf
    /// can be an int.
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
            Some(self.concrete_mono(id))
        } else {
            None
        };
        match r {
            ValueRepr::Scalar(s) => {
                let s = match expected {
                    Some(ty) => self.coerced_mono(s, value_expr, ty),
                    None => match self.struct_field_ty(struct_name, field) {
                        Some(fty) => self.coerced(s, value_expr, fty),
                        None => s,
                    },
                };
                let s = if self.is_value_struct(struct_name) {
                    self.check_integer_field(s, expected.and_then(Self::integer_field_unsigned))
                } else {
                    s
                };
                Ok(ValueRepr::Scalar(self.round_f32_field(
                    struct_name,
                    field,
                    s,
                )))
            }
            other => Ok(other),
        }
    }

    /// Sees through a transparent newtype: `newtype Score(int)` is the raw int at runtime.
    pub(crate) fn arith_ty_of(&self, te: TypeExpr) -> Option<ArithTy> {
        match te {
            TypeExpr::Builtin(BuiltinTy::Int) => Some(ArithTy::Int),
            TypeExpr::Builtin(BuiltinTy::Uint) => Some(ArithTy::Uint),
            TypeExpr::Builtin(BuiltinTy::Float) => Some(ArithTy::Float),
            TypeExpr::Builtin(BuiltinTy::String) => Some(ArithTy::Str),
            TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            } if self.ws.newtypes_by_name.contains_key(&name) => {
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

    /// First-class values need no box, so the target already meets the guarantee.
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
        let Some(id) = self.expr_rep_id(e, RepUse::Value, true)? else {
            return Ok(None);
        };
        Ok(self.scx.rep_arena().value_shape(id).cloned())
    }

    pub(crate) fn nullable_value_shape_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<ValueShape>, CompilerError> {
        if self.scx.values_first_class {
            return Ok(None);
        }
        let Some(id) = self.expr_rep_id(e, RepUse::Value, false)? else {
            return Ok(None);
        };
        Ok(self
            .scx
            .rep_arena()
            .value_shape(id)
            .filter(|shape| shape.nullable)
            .cloned())
    }

    pub(crate) fn nobox_binding_shape(
        &self,
        binding: Idx<BindingInfo>,
    ) -> Result<Option<ValueShape>, CompilerError> {
        let Some(ty) = self.binding_ty(binding) else {
            return Ok(None);
        };
        self.nobox_shape_of_ty(ty)
    }

    pub(crate) fn fn_value_rep_plan(
        &self,
        ty: TyId,
    ) -> Result<Option<FunctionRepPlan>, CompilerError> {
        if let Some(concrete) = self.concrete_ty_id(ty) {
            return self.fn_value_rep_plan_for_mono(concrete);
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
        BackendFnAbiPlan::for_function_value(self.ws, self.scx, ty)
            .map(Some)
            .map_err(|error| self.rep_error(error))
    }

    pub(crate) fn nobox_shape_of_ty(&self, ty: TyId) -> Result<Option<ValueShape>, CompilerError> {
        if self.scx.values_first_class {
            return Ok(None);
        }
        self.rep_shape_for_ty(ty, RepUse::Value, true)
    }

    pub(crate) fn nobox_global_place(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<(Vec<Ustr>, Option<ValueShape>)>, CompilerError> {
        if self.scx.values_first_class {
            return Ok(None);
        }
        match self.body.expr_arena[e] {
            // The global's declared nullability preserves its leaf positions.
            Expr::Global(name) => {
                let Some(&ty) = self.ws.globals_by_name.get(&name) else {
                    return Ok(None);
                };
                let plan = GlobalPlan::for_declared_type(self.ws, self.scx, ty)
                    .map_err(|e| self.gap_here(e.message))?;
                let Some(shape) = plan.value_shape().cloned() else {
                    return Ok(None);
                };
                // Minted from the emitted symbol, matching `global_storage::physical_leaf_names`:
                // these leaves are emitted Lua globals, and a DefKey is not an identifier.
                let symbol = crate::mangle::symbol_of_key(name);
                let leaves = (0..shape.leaf_count())
                    .map(|i| nobox_global_leaf_name(symbol, i))
                    .collect();
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

    fn global_flag_for_place(
        &self,
        mut expr: Idx<Expr>,
    ) -> Option<crate::global_storage::GlobalReadCheck> {
        loop {
            match self.body.expr_arena[expr] {
                Expr::FieldAccess { base, .. } => expr = base,
                Expr::Global(name) => {
                    return self
                        .scx
                        .global_read_checks(self.ws)
                        .get(&crate::mangle::symbol_of_key(name))
                        .copied()
                        .filter(|check| check.flag_key.is_some());
                }
                _ => return None,
            }
        }
    }

    pub(crate) fn nobox_global_load(
        &mut self,
        expr: Idx<Expr>,
        leaves: &[Ustr],
        shape: Option<ValueShape>,
    ) -> ValueRepr {
        if leaves.is_empty()
            && crate::verify::checks_enabled()
            && let Some(check) = self.global_flag_for_place(expr)
        {
            self.emit_cur(Inst::Global(check.probe));
        }
        let vals: Vec<Idx<Inst>> = leaves
            .iter()
            .map(|g| self.emit_cur(Inst::Global(*g)))
            .collect();
        match shape {
            Some(shape) => Self::rebuild_from_leaves(&shape, &vals),
            None => ValueRepr::Scalar(vals[0]),
        }
    }

    pub(crate) fn nobox_global_store(
        &mut self,
        target: Idx<Expr>,
        leaves: &[Ustr],
        shape: Option<ValueShape>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let whole = shape.is_some();
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
        if whole
            && crate::verify::checks_enabled()
            && let Some(check) = self.global_flag_for_place(target)
            && leaves.len() == check.leaf_count
            && (check.leaf_count > 0 || matches!(self.body.expr_arena[target], Expr::Global(_)))
        {
            let initialized = self.emit_cur(Inst::Const(Lit::Bool(true)));
            self.emit_cur(Inst::StoreGlobal {
                name: check.probe,
                value: initialized,
            });
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
            self.branch_to(equal, next, fail);
            self.cur = next;
        }
        let yes = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.store_cell(result, yes);
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = fail;
        let no = self.emit_cur(Inst::Const(Lit::Bool(false)));
        self.store_cell(result, no);
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = join;
        self.emit_cur(Inst::Load(result))
    }
}
