use super::*;

/// Whether a missing serving body is a rejection or a deferral is the caller's
/// call, so this carries no span.
pub struct NoServingBinding;

/// Prefer the exact target's declaration, then a portable body.
pub fn backend_variant(
    ws: &Workspace,
    name: ustr::Ustr,
    rep: FuncRef,
    target: bone_core::hir::FnBackend,
) -> Result<FuncRef, NoServingBinding> {
    use bone_core::hir::FnBackend;
    if target == FnBackend::Any {
        return Ok(rep);
    }
    let rep_backend = ws.module(rep.module).functions[rep.func].backend;
    if rep_backend == target {
        return Ok(rep);
    }
    let has_variants = ws
        .functions_by_name
        .get(&name)
        .is_some_and(|l| l.len() >= 2);
    let mut exact = None;
    let mut portable = None;
    if has_variants
        && visit_variant_group(ws, rep, |backend, candidate| {
            if backend == target {
                exact = Some(candidate);
            } else if backend == FnBackend::Any && rep_backend != FnBackend::Any {
                portable = Some(candidate);
            }
        })
        && let Some(sibling) = exact.or(portable)
    {
        return Ok(sibling);
    }
    serves_target(rep_backend, target)
        .then_some(rep)
        .ok_or(NoServingBinding)
}

/// The arguments remain the graph's identity; only the body sibling changes.
pub fn backend_variant_instance(
    ws: &Workspace,
    inst: &crate::instance::Instance,
    target: bone_core::hir::FnBackend,
) -> Result<crate::instance::Instance, NoServingBinding> {
    let f = &ws.module(inst.func.module).functions[inst.func.func];
    let func = backend_variant(ws, f.name, inst.func, target)?;
    Ok(crate::instance::Instance {
        func,
        args: inst.args.clone(),
    })
}

/// Use bone's backend order when recovering a group's representative.
pub fn canonical_variant(ws: &Workspace, rep: FuncRef) -> FuncRef {
    let mut best = None;
    if visit_variant_group(ws, rep, |backend, candidate| {
        if best.is_none_or(|(least, _)| backend < least) {
            best = Some((backend, candidate));
        }
    }) {
        best.map_or(rep, |(_, candidate)| candidate)
    } else {
        rep
    }
}

pub fn backend_variant_group(ws: &Workspace, member: FuncRef) -> Vec<FuncRef> {
    let mut group = Vec::new();
    if visit_variant_group(ws, member, |_, candidate| group.push(candidate)) {
        group
    } else {
        vec![member]
    }
}

// A repeated backend is an overload ambiguity; the group is discarded.
fn visit_variant_group(
    ws: &Workspace,
    member: FuncRef,
    mut visit: impl FnMut(bone_core::hir::FnBackend, FuncRef),
) -> bool {
    let key = ws.backend_variant_key(member.module, member.func);
    let Some(candidates) = ws.functions_by_name.get(&key.0) else {
        return false;
    };
    let mut seen = 0u8;
    for &(module, func) in candidates {
        if ws.backend_variant_key(module, func) == key {
            let backend = ws.module(module).functions[func].backend;
            let bit = 1u8 << backend as u8;
            if seen & bit != 0 {
                return false;
            }
            seen |= bit;
            visit(backend, FuncRef { module, func });
        }
    }
    true
}

impl<'a> Builder<'a> {
    pub(crate) fn lower_expr_repr(&mut self, e: Idx<Expr>) -> Result<ValueBundle, CompilerError> {
        self.lower_expr_repr_at(e, false)
    }

    fn lower_expr_repr_inner(&mut self, e: Idx<Expr>) -> Result<ValueBundle, CompilerError> {
        self.lower_expr_repr_at(e, true)
    }

    fn lower_expr_repr_at(
        &mut self,
        e: Idx<Expr>,
        check_flat_read: bool,
    ) -> Result<ValueBundle, CompilerError> {
        let outer = self.cur_span.replace(self.body.expr_spans[e]);
        let out = self.with_site_expr(e, |s| {
            let result = (|| {
                let bundle = match s.flat_ref_read(e)? {
                    Some(read) if !check_flat_read => return Ok(read),
                    Some(read) => read,
                    None => match s.body.expr_arena[e] {
                        Expr::Var(binding) if let Some(value) = s.preflight_value(binding) => {
                            s.value_bundle_for_expr(e, ValueRepr::Scalar(value))?
                        }
                        Expr::Call { callee, args, .. } => s.lower_call_repr(e, callee, args)?,
                        Expr::Index { base, index } => s.lower_index_repr(e, base, index)?,
                        _ => {
                            let value = s.lower_expr_repr_unscoped(e)?;
                            s.value_bundle_for_expr(e, value)?
                        }
                    },
                };
                if let ValueRepr::Scalar(value) = bundle.value() {
                    let value = s.check_field_value(e, *value);
                    s.mark_record_base(e, value);
                    return Ok(ValueBundle::new(
                        bundle.representation(),
                        ValueRepr::Scalar(value),
                    ));
                }
                Ok(bundle)
            })();
            result.map_err(|err| s.located(err))
        });
        self.cur_span = outer;
        out
    }

    /// Select an immediate projection before expanding its storage. Reads happen
    /// here, with no deferred place carried to a later consumer.
    fn immediate_value_projection_repr(
        &mut self,
        e: Idx<Expr>,
    ) -> Result<Option<ValueRepr>, CompilerError> {
        if self.scx.values_first_class {
            return Ok(None);
        }
        let mut root = e;
        let mut reference_field = None;
        while let Expr::FieldAccess { base, field } = self.body.expr_arena[root] {
            // A reference-owned box is storage too: select the requested subtree before
            // taking a value snapshot.
            if root != e
                && let Some(name) = self.value_struct_of(root)
                && !self.is_nobox_struct(name)
                && let Some(TypeExpr::App {
                    ctor: TypeCtor::Nominal(owner),
                    ..
                }) = self.field_base_ty(base)
                && !self.is_value_struct(owner)
                && let plan = self.struct_field_plan(owner, field, false)?
                && plan.is_direct()
                && !plan.nullable()
                // Native layouts already use physical leaf aliases below.
                && self
                    .scx
                    .rep_arena()
                    .nominal_value_storage_required(self.ws, self.scx.target, name)
                    .map_err(|err| self.gap_here(err.message))?
                    == ValueStorage::GcTable
            {
                reference_field = Some((base, field, name));
                break;
            }
            root = base;
        }
        let indexed = if let Expr::Index { base, index } = self.body.expr_arena[root] {
            match self.vec_flat_elem_of(base)? {
                Some((elem, FlatStorage::Strided)) => Some((base, index, elem)),
                _ => None,
            }
        } else {
            None
        };
        let boxed_index = if let Expr::Index { base, index } = self.body.expr_arena[root]
            && let Some(name) = self.value_struct_of(root)
            && !self.is_nobox_struct(name)
            && !self.expr_is_flat_vec(base)
            && matches!(
                self.coll_kind(base)?,
                ContainerKind::Vec | ContainerKind::VecNonNull
            )
            && self
                .scx
                .rep_arena()
                .nominal_value_storage_required(self.ws, self.scx.target, name)
                .map_err(|err| self.gap_here(err.message))?
                == ValueStorage::GcTable
        {
            Some((base, index, name))
        } else {
            None
        };
        let (binding, boxed_read, boxed) = match self.body.expr_arena[root] {
            Expr::Var(binding) if self.group_binds.contains_key(&raw(binding)) => {
                (Some(binding), None, None)
            }
            Expr::Var(binding)
                if self.binding_is_ref(binding)
                    && self.binds.contains_key(&raw(binding))
                    && !self.static_cells.contains_key(&raw(binding))
                    && let Some(name) = self.leaf_table_base(root)? =>
            {
                // A borrowed leaf table uses the owned box's selected-range
                // reader; expanding the whole ref would retain unused reads.
                (
                    None,
                    Some(Inst::Load(self.binds[&raw(binding)])),
                    Some(name),
                )
            }
            Expr::Var(binding) => {
                let Some(name) = self.boxed_value_local(binding, root) else {
                    return Ok(None);
                };
                (
                    None,
                    Some(Inst::Load(self.binds[&raw(binding)])),
                    Some(name),
                )
            }
            Expr::Global(global) => {
                let Some(name) = self.value_struct_of(root) else {
                    return Ok(None);
                };
                // Nobox globals use declared-type scalar storage, including a presence leaf
                // when narrowed from nullable.
                if self.is_nobox_struct(name) {
                    return Ok(None);
                }
                (None, Some(Inst::Global(global)), Some(name))
            }
            Expr::FieldAccess { .. } if reference_field.is_some() => {
                (None, None, Some(reference_field.unwrap().2))
            }
            Expr::Index { .. } if boxed_index.is_some() => {
                (None, None, Some(boxed_index.unwrap().2))
            }
            Expr::Index { .. } if indexed.is_some() => (None, None, None),
            _ => return Ok(None),
        };
        let mut path = Vec::new();
        let mut at = e;
        while at != root
            && let Expr::FieldAccess { base, field } = self.body.expr_arena[at]
        {
            if self.swizzle_of(base, field).is_some() {
                return Ok(None);
            }
            path.push(field);
            at = base;
        }
        path.reverse();
        let boxed_shape = boxed
            .map(|name| self.nominal_rep_shape_required(name, false))
            .transpose()?;
        let indexed_shape = indexed
            .map(|(_, _, elem)| self.column_shape(elem))
            .transpose()?;
        let shape = match boxed_shape.as_ref().or(indexed_shape.as_ref()) {
            Some(shape) => shape,
            None => &self.group_binds[&raw(binding.expect("grouped projection"))].1,
        };
        let Some((range, selected)) = self.value_projection_range(shape, &path) else {
            return Ok(None);
        };
        // An empty range performs no demanded access, so cannot preserve a nil
        // element's failure.
        if boxed_index.is_some() && range.is_empty() {
            return Ok(None);
        }
        // An opaque or untyped value slot needs the parent's borrowed-child copy
        // policy; only known primitive leaves and shape-owned groups are sliced.
        if matches!(selected, FieldShape::Prim)
            && self.concrete_expr_ty(e).is_none_or(|ty| {
                ty.peel_nullable()
                    .0
                    .nominal()
                    .is_some_and(|name| self.is_value_struct(name))
            })
        {
            return Ok(None);
        }
        // GC copies share enum children whole, so project through the field-copy
        // protocol instead of spreading the enum into lanes.
        let enum_field = if boxed_shape.is_some()
            && let FieldShape::Nested(selected_shape) = &selected
            && self.shape_has_flat_enum(selected_shape)
        {
            if path.len() != 1 || !self.boxed_copy_uses_gc_fields(shape)? {
                return Ok(None);
            }
            Some(
                shape
                    .fields
                    .iter()
                    .position(|(field, _)| *field == path[0])
                    .expect("projection range selected a declared field"),
            )
        } else {
            None
        };
        let leaves = if let Some((base, index, elem)) = indexed {
            let shape = indexed_shape.as_ref().expect("indexed projection shape");
            let outer = self.cur_span.replace(self.body.expr_spans[root]);
            let outer_expr = self.cur_expr.replace(root);
            let leaves = (|| {
                let (vec, _) = self.lower_index_base(base)?;
                let index = self.lower_expr(index)?;
                Ok::<_, CompilerError>(self.strided_element_leaves(vec, index, elem, shape, range))
            })();
            self.cur_span = outer;
            self.cur_expr = outer_expr;
            leaves?
        } else if let Some(shape) = boxed_shape {
            let value = if let Some(read) = boxed_read {
                self.emit_cur(read)
            } else if let Some((base, index, _)) = boxed_index {
                // Evaluate both operands once, at the original read point, before any later
                // argument can mutate them.
                let outer = self.cur_span.replace(self.body.expr_spans[root]);
                let outer_expr = self.cur_expr.replace(root);
                let slot = (|| {
                    let (base, kind) = self.lower_index_base(base)?;
                    let index = self.lower_expr(index)?;
                    Ok::<_, CompilerError>(self.read_index(base, index, kind))
                })();
                self.cur_span = outer;
                self.cur_expr = outer_expr;
                slot?
            } else {
                let (base, field, _) = reference_field.expect("reference-owned projection");
                let base = self.lower_expr(base)?;
                self.read_field(base, field)
            };
            if let Some(index) = enum_field {
                let inline = self.gc_table_inline_layout(shape.name)?;
                let owner = self.concrete_expr_ty(root);
                return self
                    .boxed_gc_field_copy_repr(
                        value,
                        &shape,
                        owner,
                        index,
                        inline.as_ref().and_then(|layout| layout.get(index)),
                    )
                    .map(Some);
            }
            self.cdata_leaf_range(value, &shape, range)?
        } else {
            let cells =
                self.group_binds[&raw(binding.expect("grouped projection"))].0[range].to_vec();
            cells.into_iter().map(|leaf| self.read_leaf(leaf)).collect()
        };
        let repr = match selected {
            FieldShape::Prim => ValueRepr::Scalar(leaves[0]),
            FieldShape::Nested(shape) => {
                let repr = Self::rebuild_from_leaves(&shape, &leaves);
                let owner = self.concrete_expr_ty(e);
                self.mark_borrowed_value_children(&shape, &repr, owner)?;
                repr
            }
        };
        Ok(Some(repr))
    }

    /// A projected value child must have its own snapshot.
    fn snapshot_projected_value(
        &mut self,
        e: Idx<Expr>,
        repr: ValueRepr,
    ) -> Result<ValueRepr, CompilerError> {
        if let ValueRepr::Scalar(value) = &repr
            && self.borrowed_value_children.contains_key(value)
            && (self.value_struct_of(e).is_some() || self.nullable_place_value_of(e)?.is_some())
            && let Some(ty) = self.concrete_expr_ty(e)
        {
            return self.copied_value_repr(ty, *value);
        }
        Ok(repr)
    }

    fn lower_index_repr(
        &mut self,
        e: Idx<Expr>,
        base: Idx<Expr>,
        index: Idx<Expr>,
    ) -> Result<ValueBundle, CompilerError> {
        let (b, kind) = self.lower_index_base(base)?;
        let i = self.lower_expr(index)?;
        if let Some((elem, storage)) = FlatStorage::of_vec_kind(kind) {
            return self.flat_element_repr(b, i, elem, storage);
        }
        if let Some(elem) = self.map_column_value_of(base)? {
            return self.map_column_index_repr(b, i, elem);
        }
        let name = if self.scx.values_first_class {
            None
        } else {
            self.value_struct_of(e).or(self.nullable_place_value_of(e)?)
        };
        if name.is_some()
            && let ContainerKind::NativeBufferValue(elem) = kind
            && self.is_leaf_addressed(elem)?
        {
            return self.nb_leaf_element_repr(b, i, elem);
        }
        let representation = if matches!(kind, ContainerKind::NativeBufferValue(_)) {
            let family = if self.expr_is_flat_vec(base) {
                ContainerFamily::FlatVec
            } else {
                ContainerFamily::NativeBuffer
            };
            self.container_plan_for(base, family)?
                .filter(|plan| plan.element_shape(self.scx.rep_arena()).is_some())
                .map(|plan| plan.element_rep())
        } else {
            None
        };
        let value = if let Some(name) = name {
            let slot = self.read_index(b, i, kind);
            if self.value_struct_of(e).is_none() {
                ValueRepr::Scalar(self.nullable_boxed_value_copy(
                    slot,
                    name,
                    self.concrete_expr_ty(e),
                )?)
            } else {
                self.boxed_value_copy_repr(slot, name, self.concrete_expr_ty(e))?
            }
        } else {
            let slot = self.read_index(b, i, kind);
            self.stored_read_repr(e, slot)?
        };
        match representation {
            Some(representation) => Ok(ValueBundle::new(representation, value)),
            None => self.value_bundle_for_expr(e, value),
        }
    }

    fn stored_read_repr(
        &mut self,
        e: Idx<Expr>,
        base: Idx<Inst>,
    ) -> Result<ValueRepr, CompilerError> {
        let Some(shape) = self
            .expr_tuple_shape(e)?
            .filter(|shape| shape.splats(self.scx.rep_arena()))
        else {
            return Ok(ValueRepr::Scalar(base));
        };
        let slots = self.flat_box_slots(base, shape.slot_count(self.scx.rep_arena()));
        Ok(self.tuple_from_slots(&shape, &slots, &mut 0))
    }

    fn lower_expr_repr_unscoped(&mut self, e: Idx<Expr>) -> Result<ValueRepr, CompilerError> {
        if let Expr::BinOp {
            op: BinOp::NullCoalesce,
            lhs,
            rhs,
        } = self.body.expr_arena[e]
        {
            if self.expr_is_flat_enum(e) || self.nullable_value_shape_of(lhs)?.is_some() {
                return self.nullcoalesce_repr(e, lhs, rhs);
            }
            if self.nobox_value_name(e).is_none() && self.nullable_tuple_wire_shape(lhs)?.is_some()
            {
                return self.nullcoalesce_tuple_repr(lhs, rhs);
            }
        }
        let expr_tuple_shape = self.expr_tuple_shape(e)?;
        let tuple_box_shape = expr_tuple_shape
            .clone()
            .filter(|shape| shape.splats(self.scx.rep_arena()));
        let nobox_global_place = self.nobox_global_place(e)?;
        let nullable_place_value = self.nullable_place_value_of(e)?;
        match self.body.expr_arena[e] {
            Expr::Block { statements, tail } => self.lower_block_repr(statements, tail),
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => match self.repr_join_slot(e)? {
                Some(shape) => self.lower_if_repr(cond, then_branch, else_branch, shape),
                None => Ok(ValueRepr::Scalar(self.lower_if(
                    cond,
                    then_branch,
                    else_branch,
                )?)),
            },
            Expr::IfLet {
                pattern,
                value,
                then_branch,
                else_branch,
            } => self.lower_if_let(
                pattern,
                value,
                then_branch,
                else_branch,
                self.repr_join_slot(e)?,
            ),
            Expr::Match { scrutinee, arms } => match self.repr_join_slot(e)? {
                Some(shape) => self.lower_match_repr(scrutinee, arms, shape),
                None => Ok(ValueRepr::Scalar(self.lower_match(scrutinee, arms)?)),
            },
            // `?.` is a join; when its result splats it takes the multi-cell path, else a
            // `nobox` result has nowhere to live but the single packed cell.
            Expr::SafeNav { base, field } => match self.repr_join_slot(e)? {
                Some(shape) => self.safe_nav_field_repr(e, base, field, shape),
                None => Ok(ValueRepr::Scalar(self.lower_expr(e)?)),
            },
            Expr::SafeNavCall {
                receiver,
                method,
                args,
                ..
            } => {
                let shape = self.repr_join_slot(e)?.unwrap_or(RepLayout::Opaque);
                self.safe_nav_repr(receiver, shape, move |s, recv| {
                    s.safe_nav_call_value_repr(e, method, args, recv)
                })
            }
            Expr::SafeNavLet {
                receiver,
                binding,
                body,
            } => {
                let shape = self.repr_join_slot(e)?.unwrap_or(RepLayout::Opaque);
                self.safe_nav_let_repr(receiver, binding, body, shape)
            }
            // A splatting tuple literal stays a `Tuple` repr, so a `nobox` element reaches
            // the return boundary as leaves instead of being packed first.
            Expr::Tuple(elems)
                if expr_tuple_shape
                    .as_ref()
                    .is_some_and(|shape| shape.splats(self.scx.rep_arena())) =>
            {
                let parts = elems
                    .into_iter()
                    .map(|el| self.lower_expr_repr_inner(el).map(ValueBundle::into_value))
                    .collect::<Result<_, CompilerError>>()?;
                Ok(ValueRepr::Tuple {
                    parts,
                    present: None,
                })
            }
            // Boxed value slots snapshot like fields and array elements. Newtype `.0` is
            // identity.
            Expr::TupleIndex { tuple, index } if !self.expr_is_newtype(tuple) => {
                let r = self.lower_expr_repr_inner(tuple)?.into_value();
                let projected = if let ValueRepr::Tuple { parts, .. } = r {
                    parts
                        .into_iter()
                        .nth(index)
                        .ok_or_else(|| gap("tuple index past the end of a splatting tuple"))?
                } else {
                    let base = self.pack(r)?;
                    ValueRepr::Scalar(self.emit_cur(Inst::TupleIndex { base, index }))
                };
                if !self.scx.values_first_class
                    && let ValueRepr::Scalar(slot) = &projected
                {
                    // Retaining a tuple slot's value snapshots the leaves, while container leaves
                    // keep their shared backing.
                    if let Some(name) = self.value_struct_of(e)
                        && (self.is_mutable_place(tuple)? || self.is_nobox_struct(name))
                    {
                        return self.boxed_value_copy_repr(*slot, name, self.concrete_expr_ty(e));
                    }
                    if let Some(name) = nullable_place_value {
                        return Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(
                            *slot,
                            name,
                            self.concrete_expr_ty(e),
                        )?));
                    }
                }
                Ok(projected)
            }
            // Rebuild the unboxed Tuple repr from slot cells: indexing a packed table
            // would re-materialize a `nobox` element.
            Expr::Var(b) if self.tuple_binds.contains_key(&raw(b)) => {
                let (cells, shape) = self.tuple_binds.get(&raw(b)).cloned().unwrap();
                let slots = self.load_cells(&cells);
                let mut idx = 0;
                Ok(self.tuple_from_slots(&shape, &slots, &mut idx))
            }
            // A scalarized value param's children are still the caller's or the storage's.
            Expr::Var(b) if self.group_binds.contains_key(&raw(b)) => {
                let (cells, shape) = self.group_binds.get(&raw(b)).cloned().unwrap();
                let leaves: Vec<Idx<Inst>> = cells.iter().map(|&c| self.read_leaf(c)).collect();
                let repr = Self::rebuild_from_leaves(&shape, &leaves);
                if !self.scx.values_first_class {
                    let owner = self.concrete_expr_ty(e);
                    self.mark_borrowed_value_children(&shape, &repr, owner)?;
                    if self.body.param_bindings.into_iter().any(|pb| pb == b)
                        && !self.binding_ref_borrowed(b)
                    {
                        self.share_borrowed_children(&repr);
                    }
                }
                Ok(repr)
            }
            // Keep borrowed projections in leaves so unused reads can disappear.
            Expr::Var(b) if let Some(name) = self.boxed_value_local(b, e) => {
                let boxed = self.emit_cur(Inst::Load(self.binds[&raw(b)]));
                if self.lending_pack && self.binding_value_is_shareable(b) {
                    if let Some(ty) = self.concrete_expr_ty(e) {
                        self.borrowed_value_children
                            .insert(boxed, std::borrow::Cow::Borrowed(ty));
                        self.shareable_value_boxes.insert(boxed);
                    }
                    return Ok(ValueRepr::Scalar(boxed));
                }
                self.boxed_value_copy_repr(boxed, name, self.concrete_expr_ty(e))
            }
            Expr::FieldAccess { base, field } => {
                if let Some(repr) = self.immediate_value_projection_repr(e)? {
                    return Ok(repr);
                }
                // A swizzle selects components into a fresh VecN Group (len 1: the bare
                // scalar), so a swizzle never boxes by itself.
                if let Some(idxs) = self.swizzle_of(base, field) {
                    let br = self.lower_expr_repr_inner(base)?.into_value();
                    if let ValueRepr::Group { parts, .. } = &br {
                        let sel: Vec<ValueRepr> = idxs.iter().map(|&i| parts[i].clone()).collect();
                        return Ok(swizzle_group(sel));
                    }
                    let bv = self.pack(br)?;
                    return Ok(self.swizzle_from_packed(bv, &idxs));
                }
                // A flattened enum field read as a whole value re-materializes the box from
                // the parent's slots (the single escape boundary).
                if let Some(TypeExpr::App {
                    ctor: TypeCtor::Nominal(sname),
                    ..
                }) = self.field_base_ty(base)
                    && let Some(shape) = self.struct_field_plan(sname, field, false)?.flat_shape()
                {
                    let parent = self.lower_expr(base)?;
                    return Ok(ValueRepr::Scalar(
                        self.materialize_flat(parent, field, &shape),
                    ));
                }
                if self.scx.values_first_class {
                    let parent = self.lower_expr(base)?;
                    return Ok(ValueRepr::Scalar(self.read_field(parent, field)));
                }
                if let Some(TypeExpr::App {
                    ctor: TypeCtor::Nominal(sname),
                    ..
                }) = self.field_base_ty(base)
                    && let Some((cname, _)) = self.struct_field_plan(sname, field, false)?.embed()
                {
                    let br = self.lower_expr_repr_inner(base)?.into_value();
                    if let ValueRepr::Group { name, parts, .. } = &br {
                        if let Some(i) = self.field_index(*name, field) {
                            return self.snapshot_projected_value(e, parts[i].clone());
                        }
                    }
                    let boxed = self.pack(br)?;
                    let nullable = self.embed_read_is_nullable(base, field)?;
                    return self.load_embed_repr(boxed, field, cname, nullable);
                }
                // An embed leaf read composes to one dotted key on the parent that holds it.
                let (pbase, mut key, projected_shape, native_leaf) =
                    self.physical_field_layout(base, field)?;
                if native_leaf {
                    let parent = self.lower_place_base(pbase)?;
                    return Ok(ValueRepr::Scalar(self.read_field(parent, key)));
                }
                if key != field {
                    // An unboxed value parent projects the path off its Group rather than pack a
                    // box just to read a dotted key back out.
                    let mut repr = self.lower_expr_repr_inner(pbase)?.into_value();
                    // A boxed GC-table parent keys an inline child's enum and struct fields by
                    // their leaves, so a whole-field read is split back into the Group.
                    if let ValueRepr::Scalar(v) = repr {
                        repr = match self.value_struct_name_of(pbase) {
                            Some(sname) if self.gc_table_inline_layout(sname)?.is_some() => {
                                self.boxed_value_copy_repr(v, sname, self.concrete_expr_ty(pbase))?
                            }
                            _ => ValueRepr::Scalar(v),
                        };
                    }
                    let segs: Vec<Ustr> = key.as_str().split('.').map(Ustr::from).collect();
                    let mut at = 0;
                    while at < segs.len() {
                        let ValueRepr::Group { name, parts, .. } = &repr else {
                            break;
                        };
                        let Some(i) = self.field_index(*name, segs[at]) else {
                            break;
                        };
                        repr = parts[i].clone();
                        at += 1;
                    }
                    if at == segs.len() {
                        return self.snapshot_projected_value(e, repr);
                    }
                    if at > 0 {
                        let rest: Vec<&str> = segs[at..].iter().map(|s| s.as_str()).collect();
                        key = crate::names::ustr(&rest.join("."));
                    }
                    let parent = self.pack(repr)?;
                    if let Some(shape) = projected_shape {
                        let leaves = super::shape::leaf_names(&shape, key.as_str(), ".")
                            .into_iter()
                            .map(|key| self.read_field(parent, crate::names::ustr(&key)))
                            .collect::<Vec<_>>();
                        return Ok(Self::rebuild_from_leaves(&shape, &leaves));
                    }
                    if let Some(name) = self.value_struct_of(e)
                        && self.is_nobox_struct(name)
                    {
                        let nullable = self.embed_read_is_nullable(base, field)?;
                        return self.load_embed_repr(parent, key, name, nullable);
                    }
                    return Ok(ValueRepr::Scalar(self.read_field(parent, key)));
                }
                let mut br = self.lower_expr_repr_inner(base)?.into_value();
                // A nested child uses dotted leaf keys and its name is its presence flag.
                if let ValueRepr::Scalar(v) = br {
                    br = match self.leaf_table_base(base)? {
                        Some(sname) => {
                            self.boxed_value_copy_repr(v, sname, self.concrete_expr_ty(base))?
                        }
                        None => ValueRepr::Scalar(v),
                    };
                }
                if let ValueRepr::Group { name, parts, .. } = &br {
                    if let Some(i) = self.field_index(*name, field) {
                        return self.snapshot_projected_value(e, parts[i].clone());
                    }
                }
                let basev = self.pack(br)?;
                let fv = self.read_field(basev, field);
                if let Some(name) = self.value_struct_of(e) {
                    if self.is_mutable_place(base)? || self.is_nobox_struct(name) {
                        return self.boxed_value_copy_repr(fv, name, self.concrete_expr_ty(e));
                    }
                }
                match nullable_place_value {
                    Some(name) => Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(
                        fv,
                        name,
                        self.concrete_expr_ty(e),
                    )?)),
                    None => self.stored_read_repr(e, fv),
                }
            }
            Expr::Global(g)
                if self.scx.values_first_class && !self.ws.functions_by_name.contains_key(&g) =>
            {
                Ok(ValueRepr::Scalar(self.emit_cur(Inst::Global(g))))
            }
            // A `nobox` global lives in K scalar globals, so the read is the leaf snapshot.
            Expr::Global(_) if nobox_global_place.is_some() => {
                let (leaves, shape) = nobox_global_place.expect("guard matched");
                Ok(self.nobox_global_load(e, &leaves, shape))
            }
            // A global holding a value struct is a mutable place: a whole-value read
            // snapshots the leaves at the read point.
            Expr::Global(g) if self.value_struct_of(e).is_some() => {
                let name = self.value_struct_of(e).unwrap();
                let gv = self.emit_cur(Inst::Global(g));
                self.boxed_value_copy_repr(gv, name, self.concrete_expr_ty(e))
            }
            // A static is a private global and copies the same way.
            Expr::Var(b)
                if !self.scx.values_first_class
                    && self.static_cells.contains_key(&raw(b))
                    && (self.value_struct_of(e).is_some() || nullable_place_value.is_some()) =>
            {
                let gv = self.emit_cur(Inst::Global(self.static_cells[&raw(b)]));
                match self.value_struct_of(e) {
                    Some(name) => self.boxed_value_copy_repr(gv, name, self.concrete_expr_ty(e)),
                    None => {
                        let name = nullable_place_value.expect("guard matched");
                        Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(
                            gv,
                            name,
                            self.concrete_expr_ty(e),
                        )?))
                    }
                }
            }
            // A `V?` global copies the same way, gated on presence.
            Expr::Global(g) if nullable_place_value.is_some() => {
                let name = nullable_place_value.expect("guard matched");
                let gv = self.emit_cur(Inst::Global(g));
                Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(
                    gv,
                    name,
                    self.concrete_expr_ty(e),
                )?))
            }
            // A value-struct literal stays an unboxed Group; `pack` builds the cdata only
            // if a boxed context needs it.
            Expr::StructLiteral {
                name,
                fields,
                bases,
            } if self.is_value_struct(name) => self.value_literal_repr(name, fields, bases),
            // A cdata enum construction stays an unboxed Group `[tag, _0]`; `pack` builds
            // `<Enum>_ct` only if a boxed context needs it.
            Expr::EnumVariant {
                enum_name,
                variant,
                args,
                ..
            } if EnumPlan::for_name(self.ws, enum_name)
                .is_some_and(|plan| plan.cdata().is_some()) =>
            {
                let shape = EnumPlan::for_name(self.ws, enum_name)
                    .and_then(|plan| plan.cdata().cloned())
                    .expect("cdata enum plan disappeared");
                self.enum_cdata_repr(&shape, variant, args)
            }
            // A flat enum construction stays an unboxed Group so it threads as scalars;
            // `pack` builds the `{__variant}` box only if a boxed context needs it.
            Expr::EnumVariant { enum_name, .. }
                if EnumPlan::for_name(self.ws, enum_name)
                    .is_some_and(|plan| plan.flat().is_some()) =>
            {
                let shape = EnumPlan::for_name(self.ws, enum_name)
                    .and_then(|plan| plan.flat().cloned())
                    .expect("flat enum plan disappeared");
                let cells = self.lower_flat_value(&shape, e)?;
                Ok(self.flat_cells_to_repr(&shape, cells))
            }
            Expr::BinOp {
                op: BinOp::NullCoalesce,
                lhs,
                rhs,
            } if self.nobox_value_name(e).is_some() => self.nullcoalesce_repr(e, lhs, rhs),
            _ if tuple_box_shape.is_some() => {
                let shape = tuple_box_shape.expect("guard matched");
                let base = self.lower_expr(e)?;
                let slots = self.flat_box_slots(base, shape.slot_count(self.scx.rep_arena()));
                let mut idx = 0;
                Ok(self.tuple_from_slots(&shape, &slots, &mut idx))
            }
            _ => Ok(ValueRepr::Scalar(self.lower_expr(e)?)),
        }
    }

    /// Read the lhs before branching; its presence bool or enum tag determines
    /// which arm supplies the slots.
    pub(crate) fn nullcoalesce_repr(
        &mut self,
        e: Idx<Expr>,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<ValueRepr, CompilerError> {
        let flat_shape = self.flat_enum_value_shape_of(e)?;
        let is_flat_enum = flat_shape.is_some();
        let shape = match flat_shape {
            Some(shape) => shape,
            None => self
                .nobox_value_shape_of(e)?
                .or(self.nullable_value_shape_of(lhs)?.map(|mut shape| {
                    shape.nullable = self.expr_is_nullable(e);
                    shape
                }))
                .ok_or_else(|| gap("coalesce result has no scalar shape"))?,
        };
        let mut lhs_shape = shape.clone();
        // Flat enums use a nil tag for absence, not a separate presence leaf.
        lhs_shape.nullable = !is_flat_enum;
        let lhs_bundle = self.lower_expr_repr(lhs)?;
        let owned = self.snapshot_opaque_value(lhs_bundle.value().clone())?;
        let lhs_bundle = ValueBundle::new(lhs_bundle.representation(), owned);
        let lhs_leaves =
            self.repr_to_slot_values_bundle(&lhs_bundle, &RepLayout::Leaves(lhs_shape))?;
        let (first, rest) = lhs_leaves
            .split_first()
            .ok_or_else(|| gap("coalesce lhs has no presence or tag leaf"))?;
        let (present, lhs_values) = if is_flat_enum {
            let nil = self.emit_cur(Inst::Const(Lit::Nil));
            (self.emit_cur(Inst::NotEq(*first, nil)), lhs_leaves)
        } else {
            (*first, rest.to_vec())
        };

        let mut values = self.value_join(RepLayout::Leaves(shape.clone()));
        let lhs_bb = self.f.new_block();
        let rhs_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_to(present, lhs_bb, rhs_bb);

        self.cur = rhs_bb;
        let rhs_bundle = self.lower_expr_repr(rhs)?;
        self.join_arm(&mut values, rhs_bundle)?;

        self.cur = lhs_bb;
        let mut lhs_slots = Vec::with_capacity(shape.leaf_count());
        if shape.nullable {
            lhs_slots.push(self.emit_cur(Inst::Const(Lit::Bool(true))));
        }
        lhs_slots.extend(lhs_values);
        self.join_slots_arm(&mut values, lhs_slots);
        self.finish_join(values, join)
    }

    /// Branch on the tuple's presence slot. Packing first would hide absence.
    pub(crate) fn nullcoalesce_tuple_repr(
        &mut self,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<ValueRepr, CompilerError> {
        let lhs_shape = self
            .nullable_tuple_wire_shape(lhs)?
            .expect("caller checked");
        // `??` always yields a value, so the result wire is the lhs wire minus its
        // presence, which is also what makes the rhs arm fit it.
        let shape = TupleLayout {
            nullable: false,
            ..lhs_shape.clone()
        };

        let lhs_repr = self.lower_expr_repr_inner(lhs)?.into_value();
        let lhs_slots = self.tuple_repr_to_slots(lhs_repr, &lhs_shape)?;
        let (present, lhs_values) = lhs_slots
            .split_first()
            .expect("a nullable wire carries its presence slot first");
        let (present, lhs_values) = (*present, lhs_values.to_vec());

        let mut values = self.value_join(RepLayout::Tuple(shape.clone()));
        let lhs_bb = self.f.new_block();
        let rhs_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_to(present, lhs_bb, rhs_bb);

        self.cur = rhs_bb;
        let rhs_bundle = self.lower_expr_repr(rhs)?;
        self.join_arm(&mut values, rhs_bundle)?;

        // The lhs slots were read before the branch, so their block dominates this one.
        self.cur = lhs_bb;
        self.join_slots_arm(&mut values, lhs_values);
        self.finish_join(values, join)
    }

    /// Grouped fields share one evaluation.
    pub(crate) fn value_literal_repr(
        &mut self,
        name: Ustr,
        fields: comfy_la_arena::IdxRange<bone_core::hir::StructLitField>,
        bases: comfy_la_arena::IdxRange<bone_core::hir::Expr>,
    ) -> Result<ValueRepr, CompilerError> {
        // Evaluate the base before fields, including when it needs unboxing.
        let base = bases.into_iter().next();
        let base_repr: Option<ValueRepr> = match base {
            Some(b) => Some(self.lower_expr_repr_inner(b)?.into_value()),
            None => None,
        };
        let base_boxed: Option<Idx<Inst>> = match &base_repr {
            Some(ValueRepr::Group { .. }) | None => None,
            Some(other) => Some(self.pack(other.clone())?),
        };
        let mut litmap: Vec<(Ustr, ValueRepr)> = Vec::new();
        let mut by_value: HashMap<u32, ValueRepr> = HashMap::default();
        for fidx in fields {
            let lf = self.body.struct_lit_field_arena[fidx];
            let key: u32 = lf.value.into_raw().into();
            let r = match by_value.get(&key) {
                // A later field of a group: one evaluation, but each value field owns its
                // own box.
                Some(ValueRepr::Scalar(v)) => {
                    let ty = self.concrete_expr_ty(lf.value).ok_or_else(|| {
                        self.gap_here(format!(
                            "grouped field `{name}.{}` has no concrete type",
                            lf.name
                        ))
                    })?;
                    self.copied_value_repr(ty, *v)?
                }
                Some(r) => r.clone(),
                None => {
                    let r = self.lower_expr_repr_inner(lf.value)?.into_value();
                    by_value.insert(key, r.clone());
                    r
                }
            };
            let r = self.coerce_field_repr(r, lf.value, name, lf.name)?;
            litmap.push((lf.name, r));
        }
        let parts = self.collect_literal_fields(name, litmap, |s, fname| {
            s.spread_base_field_repr(&base_repr, base_boxed, name, fname, base)
        })?;
        Ok(ValueRepr::Group {
            name,
            parts,
            present: None,
        })
    }

    pub(crate) fn overload_target(&self, e: Idx<Expr>) -> Option<FuncRef> {
        self.inf
            .and_then(|i| i.resolved_overload_calls.get(&e).copied())
            .map(|(module, func)| FuncRef { module, func })
    }

    pub(crate) fn fn_value_symbol(
        &self,
        e: Idx<Expr>,
        name: ustr::Ustr,
    ) -> Result<String, CompilerError> {
        if let Some(fref) = self.overload_target(e) {
            let f = &self.ws.module(fref.module).functions[fref.func];
            return Ok(crate::mangle::mangle_fn(self.ws, f));
        }
        let fref = self.unresolved_function_variant(name)?;
        Ok(crate::mangle::mangle_fn(
            self.ws,
            &self.ws.module(fref.module).functions[fref.func],
        ))
    }

    fn unresolved_function_variant(&self, name: ustr::Ustr) -> Result<FuncRef, CompilerError> {
        let cands = self
            .ws
            .functions_by_name
            .get(&name)
            .map(|l| l.as_slice())
            .unwrap_or(&[]);
        let Some(&(module, func)) = cands.first() else {
            return Err(gap(format!("unresolved function `{name}`")));
        };
        let first = crate::mangle::mangle_fn(self.ws, &self.ws.module(module).functions[func]);
        if cands.iter().all(|&(m, f)| {
            crate::mangle::mangle_fn(self.ws, &self.ws.module(m).functions[f]) == first
        }) {
            return Ok(FuncRef { module, func });
        }
        Err(gap(format!(
            "overloaded function `{name}` with no recorded resolution"
        )))
    }

    /// Check the resolved declaration before using a name-keyed intrinsic.
    pub(crate) fn resolves_to_inline_builtin(&self, e: Idx<Expr>, name: ustr::Ustr) -> bool {
        let Ok(fref) = self.resolve_funcref(e, name) else {
            return false;
        };
        self.ws.module(fref.module).functions[fref.func]
            .inline_body
            .is_some()
    }

    pub(crate) fn resolve_funcref(
        &self,
        e: Idx<Expr>,
        name: ustr::Ustr,
    ) -> Result<FuncRef, CompilerError> {
        let rep = if let Some(&(module, func)) =
            self.inf.and_then(|i| i.resolved_overload_calls.get(&e))
        {
            FuncRef { module, func }
        } else {
            self.unresolved_function_variant(name)?
        };
        self.select_backend_variant(e, name, rep)
    }

    pub(crate) fn select_backend_variant(
        &self,
        e: Idx<Expr>,
        name: ustr::Ustr,
        rep: FuncRef,
    ) -> Result<FuncRef, CompilerError> {
        match backend_variant(self.ws, name, rep, self.scx.target) {
            Ok(f) => Ok(f),
            Err(NoServingBinding) => self.reject_or_defer(e, name, rep, self.scx.target),
        }
    }

    /// JS can supply a host mapping for a Lua-only declaration.
    pub(crate) fn reject_or_defer(
        &self,
        e: Idx<Expr>,
        name: ustr::Ustr,
        rep: FuncRef,
        target: bone_core::hir::FnBackend,
    ) -> Result<FuncRef, CompilerError> {
        use bone_core::hir::FnBackend;
        match target {
            FnBackend::Lua => Err(CompilerError::new_with_label(
                self.body.expr_spans[e],
                format!(
                    "call to `{name}` has no luajit binding: its only declaration targets the `js` backend"
                ),
            )),
            _ => Ok(rep),
        }
    }

    pub(crate) fn lower_yield_delegate(
        &mut self,
        sub_expr: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let rac = self
            .resume_arg
            .ok_or_else(|| gap("await outside a coroutine"))?;
        let sub = self.lower_expr(sub_expr)?;
        // Registered as the active inner so kill() chains into it.
        self.emit_cur(Inst::CoroSetInner(sub));

        // Keep the yielded enum in frame slots throughout the pump.
        if let Some(shape) = self.coroutine_yield_flat_shape(sub_expr) {
            return self.lower_yield_delegate_flat(sub, rac, &shape);
        }

        let slot = self.f.new_cell(None);
        let resume0 = self.read_field(sub, crate::names::ustr("__resume"));
        let primed = self.emit_cur(Inst::CallValue {
            callee: resume0,
            args: vec![sub],
        });
        self.store_cell(slot, primed);

        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let v = self.emit_cur(Inst::Load(slot));
        self.branch_on_nil(v, exit, body);

        self.cur = body;
        let vv = self.emit_cur(Inst::Load(slot));
        let resume_bb = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Yield {
                values: vec![vv],
                resume: resume_bb,
            },
        );
        self.cur = resume_bb;
        let arg = self.emit_cur(Inst::Load(rac));
        let resume1 = self.read_field(sub, crate::names::ustr("__resume"));
        let v2 = self.emit_cur(Inst::CallValue {
            callee: resume1,
            args: vec![sub, arg],
        });
        self.store_cell(slot, v2);
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        self.yield_delegate_exit(sub)
    }

    /// Clear the active inner and read its return after successful delegation.
    pub(crate) fn yield_delegate_exit(
        &mut self,
        sub: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let clear = self.emit_cur(Inst::Const(Lit::Nil));
        self.emit_cur(Inst::CoroSetInner(clear));
        Ok(self.read_field(sub, crate::names::ustr("__return")))
    }

    pub(crate) fn lower_yield_delegate_flat(
        &mut self,
        sub: Idx<Inst>,
        rac: Idx<Cell>,
        shape: &EnumFlatShape,
    ) -> Result<Idx<Inst>, CompilerError> {
        let n = shape.leaf_count();
        let frame = sub;
        let adv0 = self.read_field(sub, crate::names::ustr("__advance"));
        self.emit_cur(Inst::CallValue {
            callee: adv0,
            args: vec![sub],
        });

        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let state = self.read_field(frame, crate::names::ustr("__state"));
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        let done = self.emit_cur(Inst::IntLt(state, zero));
        self.branch_to(done, exit, body);

        self.cur = body;
        let values: Vec<Idx<Inst>> = (0..n)
            .map(|i| self.read_field(frame, yield_slot(i)))
            .collect();
        let resume_bb = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Yield {
                values,
                resume: resume_bb,
            },
        );
        self.cur = resume_bb;
        let arg = self.emit_cur(Inst::Load(rac));
        let adv1 = self.read_field(sub, crate::names::ustr("__advance"));
        self.emit_cur(Inst::CallValue {
            callee: adv1,
            args: vec![sub, arg],
        });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        self.yield_delegate_exit(sub)
    }

    /// Requires a single workspace-wide definition.
    pub(crate) fn resolve_global_fn(&self, name: &str) -> Result<FuncRef, CompilerError> {
        let (module, func) = unique_fn(self.ws, crate::names::ustr(name))
            .ok_or_else(|| gap(format!("interp helper `{name}` not uniquely resolvable")))?;
        Ok(FuncRef { module, func })
    }
}
