use super::*;

/// How a pending `defer` is reached when its scope dies instead of exiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingDefer {
    /// Spliced at scope exit only: a global initializer or decorator, or a backend
    /// without unwind support.
    Inline,
    /// A temporary generator owned by a `for` scope.
    LoopCoroutine {
        iterator: Idx<Cell>,
        flag: Option<Idx<Cell>>,
    },
    /// A frame flag guards the splice and the terminal cleanup copy
    /// (`MirFunc::coro_defers`).
    CoFlag(Idx<Cell>),
    /// The cleanup closure is on the runtime unwind stack; the splice pops it.
    Unwind,
}

/// How a body's `defer` statements reach a death of their scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeferMode {
    /// Scope exit only: a global initializer or decorator, or a backend without
    /// unwind support.
    Inline,
    Unwind,
    /// A `co fn` body, not a lambda inside it.
    CoFlag,
}

impl DeferMode {
    pub(crate) fn for_body(is_coroutine: bool, target: bone_core::hir::FnBackend) -> Self {
        if is_coroutine {
            DeferMode::CoFlag
        } else if matches!(
            target,
            bone_core::hir::FnBackend::Lua | bone_core::hir::FnBackend::Js
        ) {
            DeferMode::Unwind
        } else {
            DeferMode::Inline
        }
    }
}

impl<'a> Builder<'a> {
    /// Its selected fields are read now, before later mutations.
    fn lower_projected_value_let(
        &mut self,
        name: Idx<BindingInfo>,
        value: Idx<Expr>,
    ) -> Result<bool, CompilerError> {
        if self.scx.values_first_class
            || self.f.is_coroutine
            || self.binding_is_ref(name)
            || self.binding_ref_escapes(name)
            || self.body_facts().may_capture
        {
            return Ok(false);
        }
        let Some(Some(fields)) = self.body_facts().projected.get(&raw(name)) else {
            return Ok(false);
        };
        let fields = fields.clone();
        let Some(nominal) = self.value_struct_of(value) else {
            return Ok(false);
        };
        let shape = self.nominal_rep_shape_required(nominal, false)?;
        if self.is_nobox_struct(nominal)
            || self.shape_has_flat_enum(&shape)
            || self
                .scx
                .rep_arena()
                .nominal_value_storage_required(self.ws, self.scx.target, nominal)
                .map_err(|e| self.gap_here(e.message))?
                != ValueStorage::GcTable
        {
            return Ok(false);
        }
        // Constructors and control flow keep their normal eager evaluation. A source
        // binding already held in leaves has no box to read.
        match self.body.expr_arena[value] {
            Expr::Var(b) if !self.group_binds.contains_key(&raw(b)) => {}
            Expr::FieldAccess { base, field } => {
                if self.physical_field_layout(base, field)?.2.is_some() {
                    return Ok(false);
                }
                let mut root = value;
                while let Expr::FieldAccess { base, .. } = self.body.expr_arena[root] {
                    root = base;
                }
                match self.body.expr_arena[root] {
                    Expr::Var(b) if self.group_binds.contains_key(&raw(b)) => return Ok(false),
                    Expr::Var(b) if self.flat_ref_binds.contains_key(&raw(b)) => return Ok(false),
                    Expr::Index { base, .. } if self.flat_stored_container(base)? => {
                        return Ok(false);
                    }
                    Expr::Call { .. } | Expr::SafeNavCall { .. }
                        if self.expr_is_value_struct(root) =>
                    {
                        return Ok(false);
                    }
                    _ => {}
                }
            }
            _ => return Ok(false),
        }
        let expanded = super::abi::expanded_nullable_parameter_shape(self.ws, &shape);
        for (field, kind) in &expanded.fields {
            if fields.contains(field)
                && matches!(kind, FieldShape::Prim)
                && self
                    .opaque_value_child(nominal, self.concrete_expr_ty(value), *field)?
                    .is_some()
            {
                return Ok(false);
            }
        }
        let mut selected = expanded.clone();
        selected.fields.retain(|(field, _)| fields.contains(field));
        let demand: Vec<bool> = expanded
            .fields
            .iter()
            .flat_map(|(field, shape)| {
                std::iter::repeat_n(fields.contains(field), shape.leaf_count())
            })
            .collect();
        let boxed = self.lower_place_base(value)?;
        let leaves = self.cdata_leaf_demand(boxed, &expanded, &demand)?;
        let repr = Self::rebuild_from_leaves(&selected, &leaves);
        self.mark_borrowed_value_children(&selected, &repr, self.concrete_expr_ty(value))?;
        let repr = self.own_opaque_parameter_children(repr, &selected)?;
        let leaves = self.repr_to_leaves_shape(&repr, &selected)?;
        if self.binding_stays_unwritten(name) {
            self.bind_value_leaves(name, &leaves, selected);
        } else {
            self.f.value_lets += leaves.len();
            self.f.value_leaves += leaves.len();
            let cells = self
                .store_binding_slots(name, &leaves)
                .into_iter()
                .map(GroupLeaf::Cell)
                .collect();
            self.group_binds.insert(raw(name), (cells, selected));
        }
        Ok(true)
    }

    /// A static initializer runs once, even across reloads.
    pub(crate) fn lower_static(
        &mut self,
        name: Idx<BindingInfo>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let sname = self.body.binding_arena[name].name;
        let ctr = {
            let c = self.static_counters.entry(sname).or_insert(0);
            let v = *c;
            *c += 1;
            v
        };
        let cell = crate::names::ustr(
            format!("__static_{}_{}_{}", self.static_owner_symbol(), sname, ctr).as_str(),
        );
        let done_flag = crate::names::ustr(format!("{cell}__done").as_str());
        self.static_cells.insert(raw(name), cell);

        let ty = self
            .inf
            .and_then(|i| i.binding_types.get(name).copied())
            .or(self.body.binding_arena[name].declared_ty);
        let type_str = ty
            .map(|t| self.ws.render(self.inf, t))
            .unwrap_or_else(|| "any".to_string());
        let init_hash = {
            use std::hash::Hasher;
            let mut hsh = std::collections::hash_map::DefaultHasher::new();
            hash_init_expr(self.body, value, &mut hsh);
            hsh.finish()
        };
        self.f.statics.push(StaticCell {
            cell,
            done_flag,
            ty,
            logical_name: sname,
            ordinal: ctr,
            type_str,
            init_hash,
            owner: self.f.code_identity.clone(),
        });

        let done_v = self.emit_cur(Inst::Global(done_flag));
        let init_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_to(done_v, join, init_bb);
        self.cur = init_bb;
        let expected = self.binding_ty(name).and_then(|ty| self.concrete_ty_id(ty));
        let v = self.lower_expr_with_expected(value, expected)?;
        self.emit_cur(Inst::StoreGlobal {
            name: cell,
            value: v,
        });
        let t = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.emit_cur(Inst::StoreGlobal {
            name: done_flag,
            value: t,
        });
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        Ok(())
    }

    /// Read by both the definition and its call sites: a fn value narrows to `nobox`
    /// shapes; a direct-only fn may use every scalarizable value shape.
    pub(crate) fn nested_fn_abi(
        &self,
        name: Idx<BindingInfo>,
        params: IdxRange<BindingInfo>,
        hoisted: Ustr,
    ) -> Result<NestedFn, CompilerError> {
        let param_names: Vec<Ustr> = params
            .into_iter()
            .map(|pb| self.body.binding_arena[pb].name)
            .collect();
        let fn_ty = self
            .binding_ty(name)
            .and_then(|ty| self.concrete_ty_id(ty))
            .ok_or_else(|| gap("concrete nested function binding has no type plan"))?;
        let boxed = NestedFn {
            hoisted,
            param_names: param_names.clone(),
            representation: self
                .scx
                .rep_arena()
                .boxed_function(self.scx.target, param_names.len()),
        };
        let taken_by_value = self
            .body
            .expr_arena
            .iter()
            .any(|(_, e)| matches!(e, Expr::Var(b) if *b == name));
        if self.scx.values_first_class {
            return Ok(boxed);
        }
        let representation =
            BackendFnAbiPlan::for_function_value(self.ws, self.scx, fn_ty, taken_by_value)
                .map_err(|error| self.rep_error(error))?;
        let abi = NestedFn {
            hoisted,
            param_names,
            representation,
        };
        Ok(if abi.uses_scalar_abi() { abi } else { boxed })
    }

    fn static_owner_symbol(&self) -> Ustr {
        crate::mangle::symbol_of_key(self.static_owner)
    }

    /// The name is registered before the body lowers so self-recursion resolves;
    /// the child sees the parent's in-scope statics and earlier sibling fns.
    pub(crate) fn lower_nested_fn(
        &mut self,
        name: Idx<BindingInfo>,
        params: IdxRange<BindingInfo>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let sname = self.body.binding_arena[name].name;
        let ctr = {
            let c = self.nested_fn_counters.entry(sname).or_insert(0);
            let v = *c;
            *c += 1;
            v
        };
        let hoisted = crate::names::ustr(
            format!("__local_{}_{}_{}", self.static_owner_symbol(), sname, ctr).as_str(),
        );
        let abi = self.nested_fn_abi(name, params, hoisted)?;
        self.nested_fn_globals.insert(raw(name), abi.clone());

        let mut mf = MirFunc::new(hoisted);
        mf.rep_arena = Some(self.scx.rep_arena().clone());
        let nested_identity = self.f.code_identity.as_ref().map(|parent| {
            use crate::code_identity::{CodeIdentity, LambdaSiteId};
            let path = match parent {
                CodeIdentity::Nested { site, .. } => format!("{}/{sname}:{ctr}", site.path),
                _ => format!("{sname}:{ctr}"),
            };
            CodeIdentity::Nested {
                enclosing: parent.owner(),
                site: LambdaSiteId {
                    file: self.body.binding_arena[name].span.file,
                    path: Ustr::from(path.as_str()),
                },
                name: sname,
            }
        });
        let mut child = Builder::new(
            self.ws,
            self.scx,
            self.concrete_plan,
            self.concrete_plans,
            self.body,
            self.inf,
            mf,
            self.flat_variants.clone(),
        );
        child.generic_bounds = self.generic_bounds;
        child.defer_mode = DeferMode::for_body(false, self.scx.target);
        child.param_reps = abi.representation.params().to_vec();
        child.ret_rep = abi.representation.result();
        child.ret_ty =
            Some(self.fn_result_ty(self.binding_ty(name).and_then(|ty| self.concrete_ty_id(ty)))?);
        child.site_owner = self.site_owner.clone();
        child.static_cells = self.static_cells.clone();
        child.nested_fn_globals = self.nested_fn_globals.clone();

        child.f.abi_representation = Some(abi.representation.clone());
        child.f.code_identity = nested_identity;
        child.f.code_abi_fingerprint =
            Some(abi.representation.logical_fingerprint(self.scx.rep_arena()));
        if child.f.code_identity.is_some() {
            child.f.code_compatibility = Some(crate::code_identity::CodeCompatibility {
                call_abi: child
                    .f
                    .code_abi_fingerprint
                    .clone()
                    .expect("nested ABI fingerprint"),
                capture: crate::code_identity::CaptureSchema::empty(),
                frame: None,
                lambda_topology: Default::default(),
            });
        }
        if let Some(identity) = child.f.code_identity.clone() {
            self.f.nested_fn_identities.insert(hoisted, identity);
        }
        child.f.nested_fn_identities = self.f.nested_fn_identities.clone();

        for (i, pb) in params.into_iter().enumerate() {
            let slot = child.param_slot(i)?;
            child.bind_param_slot(pb, slot)?;
        }
        child.copy_ref_borrowed_params(params)?;

        child.defers.push(Vec::new());
        if let Expr::Block { statements, tail } = child.body.expr_arena[body] {
            let terminated = child.lower_stmts(statements)?;
            if !terminated {
                child.lower_return(tail)?;
            }
        } else {
            child.lower_return(Some(body))?;
        }
        child.defers.pop();

        self.f.nested_fns.push(child.f);
        Ok(())
    }

    pub(crate) fn lower_stmts(
        &mut self,
        stmts: IdxRange<Statement>,
    ) -> Result<bool, CompilerError> {
        for sidx in stmts {
            self.cur_span = self.body.statement_spans.get(sidx).copied();
            match self.body.statement_arena[sidx].clone() {
                Statement::Let {
                    pattern,
                    value,
                    else_block,
                } => {
                    if let Some(else_e) = else_block {
                        self.lower_let_else(pattern, value, else_e)?;
                        continue;
                    }
                    // A transparent newtype's runtime value is the inner, so `x` binds to it.
                    if let Pattern::Newtype { sub, .. } = self.body.pattern_arena[pattern]
                        && let Pattern::Bind(var) = self.body.pattern_arena[sub]
                    {
                        let v = self.lower_expr(value)?;
                        self.bind_for_pattern(&BindingPattern::Leaf(var), v)?;
                        continue;
                    }
                    let name = match binding_view(self.body, pattern) {
                        Some(BindingPattern::Leaf(name)) => name,
                        Some(vars @ BindingPattern::Tuple(_)) => {
                            match self
                                .expr_tuple_shape(value)?
                                .filter(|shape| shape.splats(self.scx.rep_arena()))
                            {
                                Some(shape) => {
                                    let r = self.lower_expr_repr(value)?.into_value();
                                    self.bind_tuple_repr(&vars, &shape, r)?;
                                }
                                None => {
                                    let v = self.lower_expr(value)?;
                                    self.bind_for_pattern(&vars, v)?;
                                }
                            }
                            continue;
                        }
                        None => {
                            // Bone proved this pattern irrefutable.
                            self.lower_let_match(pattern, value)?;
                            continue;
                        }
                    };
                    if self.lower_projected_value_let(name, value)? {
                        continue;
                    }
                    if self.lower_flat_let(name, value)? {
                        continue;
                    }
                    if self.binding_is_ref(name) {
                        self.check_ref_holder(value)?;
                    }
                    // Scalar values live in leaf cells, matching the call ABI instead of relying
                    // on later SROA.
                    let threaded_shape =
                        if let Some(shape) = self.flat_enum_value_shape_of(value)? {
                            Some(shape)
                        } else if let Some(shape) = self.nobox_binding_shape(name)? {
                            Some(shape)
                        } else {
                            self.nobox_value_shape_of(value)?
                                .or(self.nullable_value_shape_of(value)?)
                        };
                    let mut threaded = false;
                    let mut opaque_repr = None;
                    if let Some(shape) = threaded_shape {
                        let r = self.with_value_lending(false, |s| s.lower_expr_repr(value))?;
                        let owned = self.snapshot_opaque_value(r.value().clone())?;
                        let r = ValueBundle::new(r.representation(), owned);
                        let threaded_leaves = match self
                            .scx
                            .rep_arena()
                            .value_shape(r.representation())
                        {
                            Some(actual) => {
                                if actual.name != shape.name || actual.fields != shape.fields {
                                    return Err(self.gap_here(format!(
                                        "representation {:?} shape {:?} does not match binding shape {:?}",
                                        r.representation(), actual, shape
                                    )));
                                }
                                // A nullable binding keeps its presence lane even when the source
                                // plan is non-null with presence on the Group (a map column `get`).
                                let shape = if shape.nullable && !actual.nullable {
                                    shape
                                } else {
                                    actual.clone()
                                };
                                Some((
                                    shape.clone(),
                                    self.repr_to_leaves_bundle_for_shape(&r, &shape)?,
                                ))
                            }
                            // A container-read copy is boxed, but the snapshot already split it
                            // into the binding's lanes: thread those rather than repacking them.
                            None if matches!(r.value(), ValueRepr::Group { parts, .. } if parts.len() == shape.fields.len()) => {
                                Some((
                                    shape.clone(),
                                    self.repr_to_leaves_shape(&r.value().clone(), &shape)?,
                                ))
                            }
                            // A boxed first value of a `nobox` binding still threads leaves: one
                            // boxed cell would force every later assignment to pack a box to match.
                            None if matches!(r.value(), ValueRepr::Scalar(_))
                                && !shape.nullable
                                && self.is_nobox_struct(shape.name) =>
                            {
                                Some((
                                    shape.clone(),
                                    self.repr_to_leaves_shape(&r.value().clone(), &shape)?,
                                ))
                            }
                            None => None,
                        };
                        if let Some((shape, leaves)) = threaded_leaves {
                            self.bind_leaf_slots(name, &leaves, shape, "call-binding")?;
                            threaded = true;
                        } else {
                            opaque_repr = Some(r);
                        }
                    }
                    if !threaded {
                        let shape = self.nullable_tuple_wire_shape(value)?.or(self
                            .expr_tuple_shape(value)?
                            .filter(|shape| shape.splats(self.scx.rep_arena())));
                        if let Some(shape) = shape {
                            let r = self.lower_expr_repr(value)?.into_value();
                            // A splatting tuple binds its wire, one cell per slot led by presence.
                            // Packing it would box the element and make a nullable one never
                            // compare equal to null.
                            let slots = self.tuple_repr_to_slots(r, &shape)?;
                            self.bind_tuple_slots(name, &slots, shape);
                        } else {
                            let expected =
                                self.binding_ty(name).and_then(|ty| self.concrete_ty_id(ty));
                            let mut shareable = true;
                            let mut snapshot_leaves = None;
                            let v = match opaque_repr {
                                Some(r) => self.pack_bundle(&r)?,
                                None if self.binding_is_ref(name) => {
                                    // The binding's own box is not an escape: every whole use
                                    // copies again. A `ref` borrow writes the box in place, so the
                                    // binding owns its value children.
                                    self.lower_ref_borrow(value)?
                                }
                                None if self
                                    .value_struct_of(value)
                                    .is_some_and(|n| !self.is_nobox_struct(n)) =>
                                {
                                    let lend = !self.binding_ref_borrowed(name);
                                    self.with_value_lending(lend, |s| {
                                        let source_lend = lend
                                            && (matches!(s.body.expr_arena[value], Expr::Var(_))
                                                || s.conditional_value_is_shareable(value));
                                        let r = s
                                            .with_value_lending(source_lend, |s| {
                                                s.lower_expr_repr(value)
                                            })?
                                            .into_value();
                                        let needs_snapshot = !s.value_repr_is_shareable(&r);
                                        let r = s.snapshot_opaque_value(r)?;
                                        if needs_snapshot
                                            && s.binding_stays_unwritten(name)
                                            && let Some(nominal) = s.value_struct_of(value)
                                            && expected
                                                .is_some_and(|ty| ty.nominal() == Some(nominal))
                                            && let Some(shape) =
                                                super::shape::value_shape_of_nominal(s.ws, nominal)
                                        {
                                            let expanded =
                                                super::abi::expanded_nullable_parameter_shape(
                                                    s.ws, &shape,
                                                );
                                            if expanded != shape
                                                && expanded.leaf_count()
                                                    <= WIDE_VALUE_PAYLOAD_LEAF_CUT
                                            {
                                                // Packing and rereading a nullable snapshot would
                                                // retain descriptor reads.
                                                let leaves =
                                                    s.repr_to_leaves_shape(&r, &expanded)?;
                                                snapshot_leaves = Some((expanded, leaves));
                                            }
                                        }
                                        shareable = s.value_repr_is_shareable(&r);
                                        if lend { s.pack_lent(r) } else { s.pack(r) }
                                    })?
                                }
                                None => self.with_value_lending(false, |s| {
                                    s.lower_expr_with_expected(value, expected)
                                })?,
                            };
                            if let Some((shape, leaves)) = snapshot_leaves {
                                self.bind_value_leaves(name, &leaves, shape);
                                continue;
                            }
                            let v = self.coerced_to_binding(v, value, name);
                            if self.bind_let_value(name, v) {
                                continue;
                            }
                            let cty = self.binding_ty(name);
                            let cell = self.new_binding_cell(
                                name,
                                self.body.binding_arena[name].name,
                                cty,
                                crate::names::ustr(""),
                            );
                            if self.binding_is_nonnull_value_struct(name) {
                                self.f.mark_nonnull_value_struct(cell);
                            }
                            if !shareable {
                                self.unshareable_value_cells.insert(cell);
                            }
                            if matches!(self.f.insts[v], Inst::MakeValueStruct { .. }) {
                                self.owned_value_binds.insert(raw(name));
                            }
                            self.store_cell(cell, v);
                            self.binds.insert(raw(name), cell);
                        }
                    }
                }
                Statement::Expr(e) => {
                    self.lower_stmt_expr(e)?;
                }
                Statement::Return(opt) => {
                    self.lower_return(opt)?;
                    return Ok(true);
                }
                Statement::Defer(body) => {
                    // Spliced at every scope exit. Dying also exits every open scope, so
                    // registration also builds a detached cleanup copy.
                    let pending = match self.defer_mode {
                        DeferMode::CoFlag => {
                            let flag = self.f.new_cell(Some(crate::names::ustr("__defer")));
                            let on = self.emit_cur(Inst::Const(Lit::Bool(true)));
                            self.store_cell(flag, on);
                            let entry = self.lower_detached_defer(body, Some(flag))?;
                            self.f.coro_defers.push((flag, entry));
                            PendingDefer::CoFlag(flag)
                        }
                        DeferMode::Unwind => {
                            self.register_unwind_defer(body)?;
                            PendingDefer::Unwind
                        }
                        DeferMode::Inline => PendingDefer::Inline,
                    };
                    if let Some(frame) = self.defers.last_mut() {
                        frame.push((body, pending));
                    }
                }
                Statement::Static { name, value } => {
                    self.lower_static(name, value)?;
                }
                Statement::Fn {
                    name, params, body, ..
                } => {
                    self.lower_nested_fn(name, params, body)?;
                }
                Statement::Comment(_) => {}
                Statement::While { cond, body } => {
                    if self.lower_while(cond, body)? {
                        return Ok(true);
                    }
                }
                Statement::For {
                    pattern,
                    iter,
                    body,
                } => {
                    let vars = binding_view(self.body, pattern).ok_or_else(|| {
                        gap("a `for` pattern beyond names (bone-lower desugars these)")
                    })?;
                    self.lower_for(vars, iter, body)?;
                }
                Statement::Break => {
                    let (_, brk, depth) = self.loop_target("break")?;
                    self.used_break_targets.insert(brk);
                    self.run_defers_above(depth)?;
                    self.f.set_term(self.cur, Term::Goto(brk));
                    return Ok(true);
                }
                Statement::Continue => {
                    let (cont, _, depth) = self.loop_target("continue")?;
                    self.run_defers_above(depth)?;
                    self.f.set_term(self.cur, Term::Goto(cont));
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Splices the innermost scope's pending defers in LIFO order.
    pub(crate) fn run_top_frame(&mut self) -> Result<(), CompilerError> {
        if let Some(frame) = self.defers.last() {
            for (body, pending) in frame.clone().into_iter().rev() {
                self.lower_defer_splice(body, pending)?;
            }
        }
        Ok(())
    }

    pub(crate) fn run_all_defers(&mut self) -> Result<(), CompilerError> {
        self.run_defers_above(0)
    }

    /// Splices scopes at `depth` or deeper, innermost first, each LIFO: a `return`
    /// leaves depth 0, a `break`/`continue` the loop's entry depth.
    pub(crate) fn run_defers_above(&mut self, depth: usize) -> Result<(), CompilerError> {
        let frames: Vec<Vec<(Idx<Expr>, PendingDefer)>> = self
            .defers
            .get(depth..)
            .unwrap_or_default()
            .iter()
            .rev()
            .cloned()
            .collect();
        for frame in frames {
            for (body, pending) in frame.into_iter().rev() {
                self.lower_defer_splice(body, pending)?;
            }
        }
        Ok(())
    }

    /// The frame flag is cleared before a coroutine's cleanup runs.
    pub(crate) fn lower_defer_splice(
        &mut self,
        body: Idx<Expr>,
        pending: PendingDefer,
    ) -> Result<(), CompilerError> {
        match pending {
            PendingDefer::LoopCoroutine { iterator, flag } => {
                self.kill_loop_coroutine(iterator, flag);
            }
            PendingDefer::CoFlag(f) => {
                let b_body = self.f.new_block();
                let b_done = self.f.new_block();
                let fv = self.emit_cur(Inst::Load(f));
                self.branch_to(fv, b_body, b_done);
                self.cur = b_body;
                let off = self.emit_cur(Inst::Const(Lit::Bool(false)));
                self.store_cell(f, off);
                self.lower_defer_body(body)?;
                self.f.set_term(self.cur, Term::Goto(b_done));
                self.cur = b_done;
            }
            PendingDefer::Unwind | PendingDefer::Inline => {
                if pending == PendingDefer::Unwind {
                    let pop =
                        self.emit_cur(Inst::Global(crate::names::ustr(crate::mir::DEFER_POP)));
                    self.emit_cur(Inst::CallValue {
                        callee: pop,
                        args: Vec::new(),
                    });
                }
                self.lower_defer_body(body)?;
            }
        }
        Ok(())
    }

    /// A co fn's defer body cannot yield, so a `defer` nested in it needs only its
    /// own scope-exit splice.
    fn lower_defer_body(&mut self, body: Idx<Expr>) -> Result<(), CompilerError> {
        let was = std::mem::replace(&mut self.in_defer_body, true);
        let mode = self.defer_mode;
        if mode == DeferMode::CoFlag {
            self.defer_mode = DeferMode::Inline;
        }
        let res = self.lower_stmt_expr(body);
        self.in_defer_body = was;
        self.defer_mode = mode;
        res
    }

    /// A binding reassigned after this point is shared with the cleanup closure, so
    /// it reads the current value exactly as the inline splice does.
    fn register_unwind_defer(&mut self, body: Idx<Expr>) -> Result<(), CompilerError> {
        let span = self.cur_span;
        let plan = self
            .fn_value_rep_plan_for_mono(&crate::monoty::MonoTy::Function {
                params: Vec::new(),
                ret: Box::new(crate::monoty::MonoTy::Unit),
            })?
            .ok_or_else(|| self.gap_here("defer cleanup has no function representation"))?;
        let entry = self.lower_detached_defer(body, None)?;
        // The closure's site is the defer body: the statement span would alias the lambda.
        self.cur_span = self.body.expr_spans.get(body).copied();
        let lambda = self.emit_cur(Inst::Lambda {
            params: Vec::new(),
            entry,
        });
        self.cur_span = span;
        self.f
            .lambda_abi_fingerprints
            .insert(lambda, plan.logical_fingerprint(self.scx.rep_arena()));
        self.f.lambda_abi_representations.insert(lambda, plan);
        let push = self.emit_cur(Inst::Global(crate::names::ustr(crate::mir::DEFER_PUSH)));
        self.emit_cur(Inst::CallValue {
            callee: push,
            args: vec![lambda],
        });
        Ok(())
    }

    /// Coroutine termination can enter this cleanup from outside the main CFG.
    pub(crate) fn lower_detached_defer(
        &mut self,
        body: Idx<Expr>,
        flag: Option<Idx<Cell>>,
    ) -> Result<Idx<Block>, CompilerError> {
        let saved_cur = self.cur;
        let saved_binds = self.binds.clone();
        let saved_group = self.group_binds.clone();
        let saved_loops = std::mem::take(&mut self.loop_targets);
        let saved_defers = std::mem::take(&mut self.defers);

        let entry = self.f.new_block();
        let b_body = flag.map(|_| self.f.new_block());
        let b_done = self.f.new_block();
        self.cur = entry;
        if let (Some(flag), Some(b_body)) = (flag, b_body) {
            let fv = self.emit_cur(Inst::Load(flag));
            self.branch_to(fv, b_body, b_done);
            self.cur = b_body;
            let off = self.emit_cur(Inst::Const(Lit::Bool(false)));
            self.store_cell(flag, off);
        }
        self.lower_defer_body(body)?;
        self.f.set_term(self.cur, Term::Goto(b_done));
        self.f.set_term(b_done, Term::Return(Vec::new()));

        self.cur = saved_cur;
        self.binds = saved_binds;
        self.group_binds = saved_group;
        self.loop_targets = saved_loops;
        self.defers = saved_defers;
        Ok(entry)
    }

    pub(crate) fn lower_block_repr(
        &mut self,
        statements: comfy_la_arena::IdxRange<Statement>,
        tail: Option<Idx<Expr>>,
    ) -> Result<ValueRepr, CompilerError> {
        self.defers.push(Vec::new());
        let terminated = self.lower_stmts(statements)?;
        if terminated {
            self.defers.pop();
            return Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))));
        }
        let repr = match tail {
            Some(tail) => self.lower_expr_repr(tail)?.into_value(),
            None => ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))),
        };
        self.run_top_frame()?;
        self.defers.pop();
        Ok(repr)
    }

    pub(crate) fn lower_stmt_expr(&mut self, e: Idx<Expr>) -> Result<(), CompilerError> {
        match self.body.expr_arena[e] {
            Expr::Block { statements, tail } => {
                self.defers.push(Vec::new());
                let terminated = self.lower_stmts(statements)?;
                if !terminated {
                    if let Some(t) = tail {
                        self.lower_stmt_expr(t)?;
                    }
                    self.run_top_frame()?;
                }
                self.defers.pop();
                Ok(())
            }
            // Statement-position control flow has no result cell: discarded result cells
            // and their store/load are trace-hostile dead code.
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => self.lower_if_stmt(cond, then_branch, else_branch),
            Expr::IfLet {
                pattern,
                value,
                then_branch,
                else_branch,
            } => self.lower_if_let_stmt(pattern, value, then_branch, else_branch),
            Expr::Match { scrutinee, arms } => self.lower_match_stmt(scrutinee, arms),
            Expr::Assign { target, value } => self.lower_assign_effect(target, value),
            Expr::CompoundAssign {
                target,
                op_fn,
                value,
                ..
            } => self.lower_compound_assign_effect(e, target, op_fn, value),
            _ => {
                self.lower_expr_repr(e)?;
                Ok(())
            }
        }
    }
}
