//! Overload and symbol resolution -- named-arg reordering, overload targets,
//! fn-value symbols, funcref and backend-variant selection -- plus the
//! `yield*` delegation forms, which resolve their channel the same way.
use super::*;

/// Nothing declared under `name` has a body serving the target. Whether that is
/// a rejection or a deferral is the caller's call, so this carries no span.
pub struct NoServingBinding;

/// Pick the backend-variant of a collapsed call `rep` that serves `target`.
/// bone typechecks one backend-agnostic HIR and records ONE representative per
/// overload; codegen selects here:
///
/// - `Any` target (GLSL / generic): no filtering, `rep` as-is.
/// - `rep` already serves the target: `rep` as-is (the common Lua path).
/// - a same-signature sibling serves the target: use it (the JS side of a
///   same-name pair -- the representative is the luajit variant, the sibling
///   the `inline(js)`/`extern(js)` one).
/// - otherwise `Err`: `Lua` rejects, `Js` defers to its own emitter's
///   host-mapping. [`Builder::select_backend_variant`] applies that policy.
///
/// Free rather than a `Builder` method because the shadow instance graph
/// (`skull::instance_graph`) selects variants per target too, and a second
/// implementation would let the two views of "which body serves this target"
/// drift.
pub fn backend_variant(
    ws: &Workspace,
    name: ustr::Ustr,
    rep: FuncRef,
    target: bone_semantic::hir::FnBackend,
) -> Result<FuncRef, NoServingBinding> {
    use bone_semantic::hir::FnBackend;
    if target == FnBackend::Any {
        return Ok(rep);
    }
    let rep_backend = ws.module(rep.module).functions[rep.func].backend;
    if rep_backend == target {
        return Ok(rep);
    }
    // A backend sibling can exist ONLY when the name has >=2 candidates
    // (bone collapsed a backend-variant group to `rep`). The overwhelmingly
    // common single-candidate call skips the sibling scan entirely, keeping
    // the `Any`-serves-everything fast path off the hot overload-select path.
    let has_variants = ws.functions_by_name.get(&name).is_some_and(|l| l.len() >= 2);
    if !has_variants {
        return if serves_target(rep_backend, target) { Ok(rep) } else { Err(NoServingBinding) };
    }
    // An exact-target sibling outranks an `Any` representative: backend
    // mapping beats the portable fallback (the intrinsics precedence
    // doctrine). Without this an `inline(js)` variant of a portable-bodied
    // fn is unreachable, since `Any` serves every target.
    if let Some(sib) = sibling_variant(ws, rep, target, true) {
        return Ok(sib);
    }
    if rep_backend == FnBackend::Any {
        return Ok(rep);
    }
    // rep is the other concrete backend (e.g. lua while target is js): an
    // `Any` sibling can still serve as the portable fallback.
    if let Some(sib) = sibling_variant(ws, rep, target, false) {
        return Ok(sib);
    }
    Err(NoServingBinding)
}

/// The target-selected declaration for one target-neutral concrete instance.
/// The arguments remain the graph's identity; only the body sibling changes.
pub fn backend_variant_instance(
    ws: &Workspace,
    inst: &crate::instance::Instance,
    target: bone_semantic::hir::FnBackend,
) -> Result<crate::instance::Instance, NoServingBinding> {
    let f = &ws.module(inst.func.module).functions[inst.func.func];
    let func = backend_variant(ws, f.name, inst.func, target)?;
    Ok(crate::instance::Instance { func, args: inst.args.clone() })
}

/// The backend-variant group representative of `rep` -- the declaration bone
/// would have collapsed the group to, whichever member a caller happens to hold.
///
/// The inverse direction of [`backend_variant`]: that goes representative ->
/// serving sibling for a target, this goes any member -> representative. A
/// target-neutral consumer (`skull::instance_graph`) needs it for roots, which
/// production hands it as raw declarations rather than as bone-resolved callees.
///
/// The preference order IS bone's `backend_rank` (`Any` < `Lua` < `Js`,
/// `bone-inference/src/calls.rs`) and [`backend_variant_group`] owns the complete
/// grouping rule, including bone's repeated-backend refusal.
pub fn canonical_variant(ws: &Workspace, rep: FuncRef) -> FuncRef {
    backend_variant_group(ws, rep)
        .into_iter()
        .min_by_key(|r| ws.module(r.module).functions[r.func].backend)
        .expect("a variant group contains its requested member")
}

/// The backend-variant group containing `member`.
///
/// This is bone's complete collapse rule: same name and rendered parameter
/// signature, with every backend distinct. A repeated backend is a genuine
/// overload ambiguity, so no group exists and the member stays by itself.
pub fn backend_variant_group(ws: &Workspace, member: FuncRef) -> Vec<FuncRef> {
    let mf = &ws.module(member.module).functions[member.func];
    let member_sig: Vec<String> =
        mf.param_decls.iter().map(|p| ws.render(None, p.ty)).collect();
    let Some(cands) = ws.functions_by_name.get(&mf.name) else { return vec![member] };
    let mut group = Vec::new();
    for &(mid, fid) in cands.iter() {
        let f = &ws.module(mid).functions[fid];
        if f.param_decls.len() != member_sig.len() {
            continue;
        }
        if f.param_decls.iter().zip(&member_sig).all(|(p, r)| ws.render(None, p.ty) == *r) {
            group.push(FuncRef { module: mid, func: fid });
        }
    }
    let mut backends = std::collections::BTreeSet::new();
    if group.iter().all(|r| backends.insert(ws.module(r.module).functions[r.func].backend)) {
        group
    } else {
        vec![member]
    }
}

/// A same-name, same-signature sibling of `rep` whose backend serves `target`.
pub fn sibling_variant(
    ws: &Workspace,
    rep: FuncRef,
    target: bone_semantic::hir::FnBackend,
    exact: bool,
) -> Option<FuncRef> {
    use bone_semantic::hir::FnBackend;
    for candidate in backend_variant_group(ws, rep) {
        if candidate == rep {
            continue;
        }
        let f = &ws.module(candidate.module).functions[candidate.func];
        let ok = if exact {
            f.backend == target
        } else {
            f.backend == FnBackend::Any || f.backend == target
        };
        if !ok {
            continue;
        }
        return Some(candidate);
    }
    None
}

impl Builder<'_> {
    /// Lower an expression to a `ValueRepr`: a value param read / value field
    /// projection / value literal / scalar-ABI value call stays UNBOXED (a
    /// `Group`); everything else is a `Scalar`. The ABI boundaries
    /// (params/returns/call args) consume this to splat without a cdata round-trip.
    pub(crate) fn lower_expr_repr(&mut self, e: Idx<Expr>) -> Result<ValueRepr, CompilerError> {
        // The unboxed route is a SECOND entry into expression lowering, so it
        // narrows the source cursor exactly as [`Self::lower_expr`] does. Without
        // this a scalar-ABI argument's own calls are stamped with the ENCLOSING
        // expression -- `mote_value_noise(v(..))` filed the `v` call under the
        // `mote_value_noise` site, which then selected the wrong callee.
        let outer = self.cur_span.replace(self.body.expr_spans[e]);
        let outer_expr = self.cur_expr.replace(e);
        let out = self.lower_expr_repr_inner(e);
        self.cur_span = outer;
        self.cur_expr = outer_expr;
        out
    }

    fn lower_expr_repr_inner(&mut self, e: Idx<Expr>) -> Result<ValueRepr, CompilerError> {
        // Probed ahead of the match rather than in a guard: the wire-shape oracle
        // is fallible, and a guard cannot propagate. Ordered after the `nobox`
        // coalesce below, which is the narrower case.
        if let Expr::BinOp { op: BinOp::NullCoalesce, lhs, rhs } = self.body.expr_arena[e] {
            if self.nobox_value_name(e).is_none() && self.nullable_tuple_wire_shape(lhs)?.is_some() {
                return self.nullcoalesce_tuple_repr(lhs, rhs);
            }
        }
        let expr_tuple_shape = self.expr_tuple_shape(e)?;
        let tuple_box_shape = expr_tuple_shape.clone().filter(TupleRetShape::splats);
        match self.body.expr_arena[e].clone() {
            Expr::Block { statements, tail } => self.lower_block_repr(statements, tail),
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => match self.repr_join_slot(e)? {
                Some(shape) => self.lower_if_repr(cond, then_branch, else_branch, shape),
                None => Ok(ValueRepr::Scalar(
                    self.lower_if(cond, then_branch, else_branch)?,
                )),
            },
            Expr::Match { scrutinee, arms } => match self.repr_join_slot(e)? {
                Some(shape) => self.lower_match_repr(scrutinee, arms, shape),
                None => Ok(ValueRepr::Scalar(self.lower_match(scrutinee, arms)?)),
            },
            // `?.` is a join like `if`/`match`, and takes the same multi-cell
            // path when its result splats -- otherwise a `nobox` result has
            // nowhere to live but the single packed cell.
            Expr::SafeNav { base, field } => match self.repr_join_slot(e)? {
                Some(shape) => self.safe_nav_field_repr(e, base, field, shape),
                None => Ok(ValueRepr::Scalar(self.lower_expr(e)?)),
            },
            Expr::SafeNavCall { receiver, method, args } => match self.repr_join_slot(e)? {
                Some(shape) => self.safe_nav_call_repr(e, receiver, method, args, shape),
                None => Ok(ValueRepr::Scalar(self.lower_expr(e)?)),
            },
            Expr::SafeNavLet { receiver, binding, body } => match self.repr_join_slot(e)? {
                Some(shape) => self.safe_nav_let_repr(receiver, binding, body, shape),
                None => Ok(ValueRepr::Scalar(self.lower_expr(e)?)),
            },
            // A tuple literal whose type splats stays a `Tuple` repr, so a
            // `nobox` element reaches the return boundary as leaves instead of
            // being packed into the table first.
            Expr::Tuple(elems) if expr_tuple_shape.as_ref().is_some_and(TupleRetShape::splats) => {
                let parts = elems
                    .into_iter()
                    .map(|el| self.lower_expr_repr(el))
                    .collect::<Result<_, CompilerError>>()?;
                Ok(ValueRepr::Tuple { parts, present: None })
            }
            // `t.0` off a splatting tuple projects the part REPR directly. Without
            // this the base packs to a table and the index reads a boxed element
            // back out, which re-materializes a `nobox` one. A newtype's `.0` is
            // identity and a non-splatting base keeps the packed `TupleIndex`, so
            // only the unboxed case changes.
            Expr::TupleIndex { tuple, index } if !self.expr_is_newtype(tuple) => {
                let r = self.lower_expr_repr(tuple)?;
                if let ValueRepr::Tuple { parts, .. } = r {
                    return parts
                        .into_iter()
                        .nth(index)
                        .ok_or_else(|| gap("tuple index past the end of a splatting tuple"));
                }
                let base = self.pack(r);
                Ok(ValueRepr::Scalar(self.emit_cur(Inst::TupleIndex { base, index })))
            }
            // A name bound to a splatting tuple: rebuild the unboxed Tuple repr
            // from its slot cells, so `t.0` projects a part instead of indexing a
            // packed table (which would re-materialize a `nobox` element).
            Expr::Var(b) if self.tuple_binds.contains_key(&raw(b)) => {
                let (cells, shape) = self.tuple_binds.get(&raw(b)).cloned().unwrap();
                let slots: Vec<Idx<Inst>> =
                    cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let mut idx = 0;
                Ok(self.tuple_from_slots(&shape, &slots, &mut idx))
            }
            // A scalarized value param: rebuild the unboxed Group from leaf cells.
            Expr::Var(b) if self.group_binds.contains_key(&raw(b)) => {
                let (cells, shape) = self.group_binds.get(&raw(b)).cloned().unwrap();
                let leaves: Vec<Idx<Inst>> =
                    cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let mut idx = 0;
                Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx))
            }
            // `base.field`: project a Group part directly (no cdata); else a
            // boxed field read off the packed base.
            Expr::FieldAccess { base, field } => {
                // A swizzle read (docs/vec-swizzles-plan.md): select the
                // components into a fresh VecN Group (len 1: the bare scalar) --
                // scalarized end to end, so a swizzle never boxes by itself.
                if let Some(idxs) = self.swizzle_of(base, field) {
                    let br = self.lower_expr_repr(base)?;
                    if let ValueRepr::Group { parts, .. } = &br {
                        let sel: Vec<ValueRepr> = idxs.iter().map(|&i| parts[i].clone()).collect();
                        return Ok(swizzle_group(sel));
                    }
                    let bv = self.pack(br);
                    return Ok(self.swizzle_from_packed(bv, &idxs));
                }
                // A flattened enum field read as a WHOLE value re-materializes the
                // box from the parent's slots (the single escape boundary).
                if let Some(shape) = self.flat_field_of(base, field) {
                    let parent = self.lower_expr(base)?;
                    return Ok(ValueRepr::Scalar(self.materialize_flat(parent, field, &shape)));
                }
                // An embedded field read as a WHOLE value re-materializes from
                // its leaves. Projection-only keeps every such read transient
                // (never bound, returned, captured or stored), so this is the
                // consuming boundary, not a reintroduced box.
                if let Some(cname) = self.embed_field_of(base, field) {
                    // A leaf-table VALUE parent can still be UNBOXED here (a
                    // local, a param, a call result): project the part off the
                    // Group rather than packing a table just to read dotted keys
                    // back out of it. A reference parent is never a Group, so
                    // this changes nothing for the `embed` case proper.
                    let br = self.lower_expr_repr(base)?;
                    if let ValueRepr::Group { name, parts, .. } = &br {
                        if let Some(i) = self.field_index(*name, field) {
                            return Ok(parts[i].clone());
                        }
                    }
                    let boxed = self.pack(br);
                    // A `nobox` child stays in LEAF form instead. The transient
                    // box is legal for an unmarked value -- it sinks -- but for
                    // a marked one it is the guarantee's violation itself, so
                    // the sink decides the representation. This is what lets a
                    // field load widen to `T?`: a Group carries no presence, so
                    // `repr_to_leaves` supplies it, exactly as it already does
                    // for a literal ctor or a call result at the same param.
                    if let Some(shape) = self.nobox_embed_group_shape(cname) {
                        let parent = boxed;
                        let mut g = self.embed_group(parent, field, &shape);
                        // A NULLABLE child carries presence in the field's own
                        // (undotted) key, so the Group takes it as its presence
                        // rather than gating the read: an absent child's leaves
                        // are nil, and every consumer branches on presence
                        // before touching them.
                        if self.embed_read_is_nullable(base, field) {
                            let present = self.emit_cur(Inst::Field { base: parent, field });
                            if let ValueRepr::Group { present: p, .. } = &mut g {
                                *p = Some(present);
                            }
                        }
                        return Ok(g);
                    }
                    let v = if self.embed_read_is_nullable(base, field) {
                        self.materialize_embed_nullable(boxed, field, cname)
                    } else {
                        self.materialize_embed(boxed, field, cname)
                    };
                    return Ok(ValueRepr::Scalar(v));
                }
                // An embed LEAF read composes to one dotted key on the parent
                // that holds it; no intermediate projection exists.
                let (pbase, key) = self.physical_field(base, field);
                if key != field {
                    let parent = self.lower_expr(pbase)?;
                    return Ok(ValueRepr::Scalar(
                        self.emit_cur(Inst::Field { base: parent, field: key }),
                    ));
                }
                let mut br = self.lower_expr_repr(base)?;
                // A leaf-table BOX has no declared-field keys -- a nested child
                // lives at dotted leaves and its own name holds only presence,
                // so `Field { base, "pos" }` would hand the consumer a bool.
                // Split it back into leaves and project the rebuilt Group; the
                // leaves nothing reads die in DCE.
                if let ValueRepr::Scalar(v) = br {
                    match self
                        .value_struct_name_of(base)
                        .filter(|n| {
                            matches!(
                                value_storage_for_nominal(self.ws, self.scx.target, *n),
                                Some(ValueStorage::LeafTable)
                            )
                        })
                    {
                        Some(sname) => br = self.boxed_value_copy_repr(v, sname),
                        None => br = ValueRepr::Scalar(v),
                    }
                }
                if let ValueRepr::Group { name, parts, .. } = &br {
                    if let Some(i) = self.field_index(*name, field) {
                        return Ok(parts[i].clone());
                    }
                }
                // A boxed base: read the field directly. A payload-free enum field
                // is a plain `int` (the enum value IS the int), so no conversion --
                // same as any primitive field. A VALUE-struct field is a whole-value
                // read out of a mutable place, so it copies.
                let basev = self.pack(br);
                let fv = self.emit_cur(Inst::Field { base: basev, field });
                // A `nobox` field splits to leaves whatever the base is: the box
                // is the guarantee's violation, not just a copy that would sink.
                // It also carries presence STATICALLY -- a Group widening to `T?`
                // supplies `true`, where a Scalar would have to ask `~= nil` at
                // runtime, and that test pins the cell feeding it against SROA.
                if let Some(name) = self
                    .value_struct_of(e)
                    .filter(|n| self.is_mutable_place(base) || self.is_nobox_struct(*n))
                {
                    return Ok(self.boxed_value_copy_repr(fv, name));
                }
                match self.nullable_place_value_of(e) {
                    Some(name) => Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(fv, name))),
                    None => Ok(ValueRepr::Scalar(fv)),
                }
            }
            // A `nobox` global lives in K scalar globals, so the read IS the leaf
            // snapshot -- no box to copy out of.
            Expr::Global(_) if self.nobox_global_place(e).is_some() => {
                let (leaves, shape) = self.nobox_global_place(e).expect("guard matched");
                Ok(self.nobox_global_load(&leaves, shape))
            }
            // A global holding a value struct is a mutable place: a whole-value read
            // copies (the leaves are snapshotted at the read point).
            Expr::Global(g) if self.value_struct_of(e).is_some() => {
                let name = self.value_struct_of(e).unwrap();
                let gv = self.emit_cur(Inst::Global(g));
                Ok(self.boxed_value_copy_repr(gv, name))
            }
            // A `V?` global copies the same way, gated on presence.
            Expr::Global(g) if self.nullable_place_value_of(e).is_some() => {
                let name = self.nullable_place_value_of(e).unwrap();
                let gv = self.emit_cur(Inst::Global(g));
                Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(gv, name)))
            }
            // A value-struct literal stays an unboxed Group (its fields lowered as
            // reprs); `pack` builds the cdata only if a boxed context needs it.
            Expr::StructLiteral { name, fields, bases } if self.is_value_struct(name) => {
                self.value_literal_repr(name, fields, bases)
            }
            // A cdata-eligible enum construction stays an unboxed Group `[tag, _0]`
            // (so a callsite/field can splat it without a cdata round-trip); `pack`
            // builds `<Enum>_ct` only if a boxed context needs it.
            Expr::EnumVariant { enum_name, variant, args, .. }
                if EnumPlan::for_name(self.ws, enum_name).is_some_and(|plan| plan.cdata().is_some()) =>
            {
                let shape = EnumPlan::for_name(self.ws, enum_name)
                    .and_then(|plan| plan.cdata().cloned())
                    .expect("cdata enum plan disappeared");
                self.enum_cdata_repr(&shape, variant, args)
            }
            // A flat (boxed) enum construction stays an unboxed Group `[tag, _0..,
            // <rec>..]` so a call/return/match can thread it as scalars; `pack`
            // builds the `{__variant}` box only if a boxed context needs it.
            Expr::EnumVariant { enum_name, .. }
                if EnumPlan::for_name(self.ws, enum_name).is_some_and(|plan| plan.flat().is_some()) =>
            {
                let shape = EnumPlan::for_name(self.ws, enum_name)
                    .and_then(|plan| plan.flat().cloned())
                    .expect("flat enum plan disappeared");
                let cells = self.lower_flat_value(&shape, e)?;
                Ok(self.flat_cells_to_repr(&shape, cells))
            }
            Expr::Call { callee, args, .. } => self.lower_call_repr(e, callee, args),
            // A COLUMN element read routes on the container, ahead of the
            // element-type guard below: column storage has no element slot at
            // all, and inside a body specialized at `T = E?` the element type is
            // an unsubstituted generic that no type predicate can classify.
            Expr::Index { base, index } if self.vec_column_elem_of(base)?.is_some() => {
                let elem = self.vec_column_elem_of(base)?.expect("guard matched");
                let (b, _) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                self.column_element_repr(b, i, elem)
            }
            // A value-struct ELEMENT read (`buf[i]`/`fv[i]`/`v[i]`) as an unboxed
            // Group of leaf reads off the checked slot: a leaf projection reads the
            // part live, while `pack` (a whole-value use -- let-bind, arg, return,
            // store) rebuilds a fresh value instead of aliasing the array slot.
            // A `V?` element takes the same shape, gated on presence at the copy.
            Expr::Index { base, index }
                if self.value_struct_of(e).is_some()
                    || self.nullable_place_value_of(e).is_some() =>
            {
                let nullable = self.value_struct_of(e).is_none();
                let name = self
                    .value_struct_of(e)
                    .or_else(|| self.nullable_place_value_of(e))
                    .expect("guard matched");
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                // Column storage: there is no element slot to read leaves off --
                // each leaf lives in its own array, so the Group is built from K
                // direct column reads. Bounds are checked ONCE for the element.
                if let ContainerKind::VecValueColumns(elem) = kind {
                    return self.column_element_repr(b, i, elem);
                }
                // A column-stored Map reaches its leaves the same way, one
                // indirection out: the spine resolves the key to a slot, and the
                // slot indexes `data`'s columns.
                if let Some(elem) = self.map_column_value_of(base)? {
                    return self.map_column_index_repr(b, i, elem);
                }
                // Leaf addressing: no slot reference, no nested ctor -- each
                // leaf comes straight from its typed view.
                if let ContainerKind::NativeBufferValue(elem) = kind {
                    if self.is_leaf_addressed(elem)? {
                        return self.nb_leaf_element_repr(b, i, elem);
                    }
                }
                let slot = self.emit_cur(Inst::Index { base: b, index: i, kind });
                if nullable {
                    return Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(slot, name)));
                }
                Ok(self.boxed_value_copy_repr(slot, name))
            }
            Expr::BinOp { op: BinOp::NullCoalesce, lhs, rhs }
                if self.nobox_value_name(e).is_some() =>
            {
                self.nullcoalesce_repr(e, lhs, rhs)
            }
            // A BOXED tuple whose layout splats -- read from a container, a field,
            // an opaque call. Rebuild the unboxed `Tuple` repr from the box's
            // slots, the exact inverse of the producer in `Expr::Tuple`, so a
            // `nobox` part comes back as leaves instead of being indexed out as a
            // single value that no longer exists there.
            //
            // Rebuilding HERE rather than at each reader is what keeps the readers
            // honest: slot projection, `let (a, b) =`, and the `for` destructure
            // all already consume a `ValueRepr::Tuple`, so none of them needs to
            // know the box widened. Slots the consumer ignores are DCE'd.
            _ if tuple_box_shape.is_some() => {
                let shape = tuple_box_shape.expect("guard matched");
                let base = self.lower_expr(e)?;
                let slots: Vec<Idx<Inst>> = (0..shape.slot_count())
                    .map(|i| self.emit_cur(Inst::TupleIndex { base, index: i }))
                    .collect();
                let mut idx = 0;
                Ok(self.tuple_from_slots(&shape, &slots, &mut idx))
            }
            _ => Ok(ValueRepr::Scalar(self.lower_expr(e)?)),
        }
    }

    /// `a ?? b` whose RESULT is a `nobox` value: both arms thread leaves and join
    /// through cells, so the coalesce never packs a cdata just to choose between
    /// them. The lhs leaves are read before the branch (their block dominates
    /// both arms) and its presence leaf IS the condition -- no nil compare.
    pub(crate) fn nullcoalesce_repr(
        &mut self,
        e: Idx<Expr>,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<ValueRepr, CompilerError> {
        let name = self.nobox_value_name(e).expect("caller checked");
        let shape = self
            .nobox_value_shape_of(e)
            .ok_or_else(|| gap(format!("nobox value `{name}` has no scalar shape")))?;
        let mut lhs_shape = shape.clone();
        lhs_shape.nullable = true;

        let lhs_repr = self.lower_expr_repr(lhs)?;
        let lhs_leaves = self.repr_to_leaves(&lhs_repr, &lhs_shape);
        let (present, lhs_values) = lhs_leaves
            .split_first()
            .expect("a nullable shape carries its presence leaf first");
        let (present, lhs_values) = (*present, lhs_values.to_vec());

        let cells: Vec<Idx<Cell>> = lhs_values.iter().map(|_| self.f.new_cell(None)).collect();
        let lhs_bb = self.f.new_block();
        let rhs_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch { cond: present, then_bb: lhs_bb, else_bb: rhs_bb },
        );

        self.cur = rhs_bb;
        let rhs_repr = self.lower_expr_repr(rhs)?;
        self.store_repr_and_join(&cells, &RetSlot::Leaves(shape.clone()), rhs_repr, join)?;

        self.cur = lhs_bb;
        self.store_slots_and_join(&cells, &lhs_values, join)?;

        self.cur = join;
        let loads: Vec<Idx<Inst>> = cells.iter().map(|&c| self.emit_cur(Inst::Load(c))).collect();
        let mut idx = 0;
        Ok(self.rebuild_from_leaves(&shape, &loads, &mut idx))
    }

    /// [`Self::nullcoalesce_repr`] for a nullable TUPLE lhs, and the same rule one
    /// level up: the lhs presence SLOT is the condition, both arms join slot-wise.
    ///
    /// Without this the lhs went through `lower_expr`, which packs a `MakeTuple` --
    /// structurally never nil, so `Eq(lv, nil)` const-folded, the rhs arm was
    /// deleted outright, and `maybe(false) ?? fallback` evaluated to the ABSENT
    /// wire's nil leaves. That is the same "a box is never nil" trap the `!= null`
    /// tuple case already carries.
    pub(crate) fn nullcoalesce_tuple_repr(
        &mut self,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<ValueRepr, CompilerError> {
        let lhs_shape = self.nullable_tuple_wire_shape(lhs)?.expect("caller checked");
        // `??` always yields a value, so the result wire is the lhs wire minus its
        // presence -- which is also what makes the rhs (non-nullable) arm fit it.
        let shape = TupleRetShape { nullable: false, ..lhs_shape.clone() };

        let lhs_repr = self.lower_expr_repr(lhs)?;
        let lhs_slots = self.tuple_repr_to_slots(lhs_repr, &lhs_shape);
        let (present, lhs_values) = lhs_slots
            .split_first()
            .expect("a nullable wire carries its presence slot first");
        let (present, lhs_values) = (*present, lhs_values.to_vec());

        let cells: Vec<Idx<Cell>> = lhs_values.iter().map(|_| self.f.new_cell(None)).collect();
        let lhs_bb = self.f.new_block();
        let rhs_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch { cond: present, then_bb: lhs_bb, else_bb: rhs_bb },
        );

        self.cur = rhs_bb;
        let rhs_repr = self.lower_expr_repr(rhs)?;
        self.store_repr_and_join(&cells, &RetSlot::Nested(shape.clone()), rhs_repr, join)?;

        // The lhs slots were read before the branch, so their block dominates this
        // one -- the same reason the value twin can reuse its leaves here.
        self.cur = lhs_bb;
        self.store_slots_and_join(&cells, &lhs_values, join)?;

        self.cur = join;
        let loads: Vec<Idx<Inst>> = cells.iter().map(|&c| self.emit_cur(Inst::Load(c))).collect();
        let mut idx = 0;
        Ok(self.tuple_from_slots(&shape, &loads, &mut idx))
    }

    /// A value-struct literal as an unboxed `Group` (declared field order, defaults
    /// filled; grouped fields share one evaluation). Mirrors `lower_struct_literal`
    /// but threads `ValueRepr`s so nested value fields stay unboxed.
    pub(crate) fn value_literal_repr(
        &mut self,
        name: Ustr,
        fields: comfy_la_arena::IdxRange<bone_semantic::hir::StructLitField>,
        bases: comfy_la_arena::IdxRange<bone_semantic::hir::Expr>,
    ) -> Result<ValueRepr, CompilerError> {
        // Functional-update base: evaluate once (before fields). A Group base
        // projects its parts directly (no cdata); a boxed base reads fields off
        // the packed cdata (the normal value-field read). Inference guarantees
        // at most one base of this struct's type.
        let base_repr: Option<ValueRepr> = match bases.into_iter().next() {
            Some(b) => Some(self.lower_expr_repr(b)?),
            None => None,
        };
        let base_boxed: Option<Idx<Inst>> = match &base_repr {
            Some(ValueRepr::Group { .. }) | None => None,
            Some(other) => Some(self.pack(other.clone())),
        };
        let mut litmap: Vec<(Ustr, ValueRepr)> = Vec::new();
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
            let r = self.coerce_field_repr(r, lf.value, name, lf.name)?;
            litmap.push((lf.name, r));
        }
        let parts: Vec<ValueRepr> = match self.struct_field_names(name) {
            Some(decl) => {
                let mut fs = Vec::new();
                for fname in decl {
                    match litmap.iter().find(|(n, _)| *n == fname) {
                        Some((_, r)) => fs.push(r.clone()),
                        None => match (&base_repr, base_boxed) {
                            (Some(ValueRepr::Group { name: bn, parts, .. }), _) => {
                                let i = self
                                    .field_index(*bn, fname)
                                    .expect("functional-update base covers every field");
                                fs.push(parts[i].clone());
                            }
                            (Some(_), Some(bi)) => {
                                fs.push(ValueRepr::Scalar(
                                    self.emit_cur(Inst::Field { base: bi, field: fname }),
                                ));
                            }
                            _ => {
                                let v = self.lower_struct_default(name, fname)?;
                                fs.push(ValueRepr::Scalar(v));
                            }
                        },
                    }
                }
                fs
            }
            None => litmap.into_iter().map(|(_, r)| r).collect(),
        };
        Ok(ValueRepr::Group {
            name,
            parts,
            present: None,
        })
    }

    /// Reorder a call's arguments (some written `name = value`) into the callee's
    /// declared parameter order. Args evaluate in SOURCE order (left-to-right);
    /// each lands in its param slot (named -> by name, positional -> next free
    /// front slot). A slot with no arg becomes an explicit `nil` so the callee's
    /// default-fill prologue triggers. Trailing empty slots are dropped (Lua nils
    /// them).
    pub(crate) fn reorder_named_args(
        &mut self,
        e: Idx<Expr>,
        arg_idxs: &[Idx<Expr>],
        func: FuncRef,
        callee_inst: Option<&crate::instance::Instance>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let param_names: Vec<Ustr> = self
            .ws
            .module(func.module)
            .functions[func.func]
            .param_decls
            .iter()
            .map(|pd| pd.name)
            .collect();
        let mut slots: Vec<Option<Idx<Inst>>> = vec![None; param_names.len()];
        let mut next_pos = 0usize;
        for &a in arg_idxs {
            // Struct spread `..op`: decompose the operand into per-field values
            // (a struct-literal operand is elided) and place each into the param
            // slot of the same name. bone verified every field matches a param,
            // and an unmatched one rejects here rather than being dropped -- a
            // silent skip would pass `nil` for that parameter instead;
            // a named arg after the spread overrides via last-write-wins.
            if self.body.spread_args.contains(&a) {
                for (fname, fv) in self.spread_field_insts(a)? {
                    let pidx = param_names.iter().position(|p| *p == fname).ok_or_else(|| {
                        gap(format!("spread field `{fname}` matches no parameter"))
                    })?;
                    slots[pidx] = Some(fv);
                }
                continue;
            }
            let v = self.lower_expr(a)?;
            let pidx = match self.body.arg_names.get(&a) {
                Some(nm) => param_names
                    .iter()
                    .position(|p| p == nm)
                    .ok_or_else(|| gap(format!("named argument `{nm}` matches no parameter")))?,
                None => {
                    let i = next_pos;
                    next_pos += 1;
                    i
                }
            };
            if pidx >= slots.len() {
                return Err(gap(format!(
                    "argument lands in slot {pidx} of {} parameters",
                    slots.len()
                )));
            }
            let v = match callee_inst
                .map(|inst| self.concrete_callee_param_ty(e, inst, pidx))
                .transpose()?
                .flatten()
            {
                Some(t) => self.coerced_mono(v, a, &t),
                None => {
                    return Err(self.gap_here("concrete call has no callee parameter type"));
                }
            };
            slots[pidx] = Some(v);
        }
        let hi = slots.iter().rposition(|s| s.is_some()).map_or(0, |i| i + 1);
        let mut out = Vec::with_capacity(hi);
        for s in slots.into_iter().take(hi) {
            out.push(match s {
                Some(v) => v,
                None => self.emit_cur(Inst::Const(Lit::Nil)),
            });
        }
        Ok(out)
    }

    /// The overload target bone resolved on expr `e` (a unary `-x`/`!x`/`~x`
    /// over a struct records its `__op_*` callee here), or `None` for a native
    /// scalar op.
    pub(crate) fn overload_target(&self, e: Idx<Expr>) -> Option<FuncRef> {
        self.inf
            .and_then(|i| i.resolved_overload_calls.get(&e).copied())
            .map(|(module, func)| FuncRef { module, func })
    }

    /// Resolve a named call to a [`FuncRef`]. bone records overload-resolved
    /// targets in `resolved_overload_calls`; a non-overloaded name resolves
    /// uniquely through `functions_by_name`.
    /// The emitted symbol for a FUNCTION taken by value (`let f = adder`). Prefer
    /// bone's recorded overload resolution (an overload disambiguated by the
    /// value's expected type); otherwise every candidate is a backend VARIANT of
    /// one `(name, signature)` and so mangles identically -- dedup and take it.
    /// A genuine unresolved overload rejects (reject, don't fall back).
    pub(crate) fn fn_value_symbol(&self, e: Idx<Expr>, name: ustr::Ustr) -> Result<String, CompilerError> {
        if let Some(fref) = self.overload_target(e) {
            let f = &self.ws.module(fref.module).functions[fref.func];
            return Ok(crate::mangle::mangle_fn(self.ws, f));
        }
        let cands = self.ws.functions_by_name.get(&name).map(|l| l.as_slice()).unwrap_or(&[]);
        let mut names: Vec<String> = cands
            .iter()
            .map(|&(m, fi)| crate::mangle::mangle_fn(self.ws, &self.ws.module(m).functions[fi]))
            .collect();
        names.sort();
        names.dedup();
        match names.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(gap(format!("unresolved function value `{name}`"))),
            _ => Err(gap(format!(
                "function value `{name}` is an unresolved overload (distinct signatures)"
            ))),
        }
    }

    /// Whether the call at `e` resolves to an `inline(lua)`-bodied definition of
    /// `name`. Guards the name-keyed intrinsic fast paths: a user fn may legally
    /// share a stdlib name, and rewriting ITS call as the intrinsic miscompiles.
    /// A rock-bodied user fn has no inline body, which is the discriminant --
    /// module identity is not, because the test harnesses build their own
    /// stdlib. Fails CLOSED: an unresolved callee is not the intrinsic.
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
        let rep = if let Some(&(module, func)) = self
            .inf
            .and_then(|i| i.resolved_overload_calls.get(&e))
        {
            FuncRef { module, func }
        } else {
            match self.ws.functions_by_name.get(&name).map(|l| l.as_slice()) {
                Some([(module, func)]) => FuncRef {
                    module: *module,
                    func: *func,
                },
                Some([]) | None => return Err(gap(format!("unresolved call `{name}`"))),
                Some(_) => {
                    return Err(gap(format!(
                        "overloaded call `{name}` with no recorded resolution"
                    )))
                }
            }
        };
        self.select_backend_variant(e, name, rep)
    }

    /// Pick the backend-variant of a collapsed call `rep` that serves the current
    /// codegen [`target`](ScalarCtx::target). bone typechecks one backend-agnostic
    /// HIR and records ONE representative per overload; codegen selects here:
    ///
    /// - `Any` target (GLSL / generic): no filtering, `rep` as-is.
    /// - `rep` already serves the target: `rep` as-is (the common Lua path).
    /// - a same-signature sibling serves the target: use it (the JS side of a
    ///   same-name pair -- the representative is the luajit variant, the sibling
    ///   the `inline(js)`/`extern(js)` one).
    /// - `Lua` target, no lua/any binding: REJECT (a JS-only decl called from
    ///   lua-reachable code).
    /// - `Js` target, no js/any binding and no sibling: leave `rep` for the JS
    ///   emitter's own host-mapping / reject (a `inline(lua)` stdlib primitive with
    ///   a hand-written JS template must still resolve there).
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

    /// No servable binding for `target` was found: a `Lua` target REJECTS (a
    /// js-only decl reached from lua-reachable code); a `Js` target defers `rep`
    /// to the JS emitter (which may host-map a lua-bodied stdlib primitive, or
    /// reject with its own message).
    pub(crate) fn reject_or_defer(
        &self,
        e: Idx<Expr>,
        name: ustr::Ustr,
        rep: FuncRef,
        target: bone_semantic::hir::FnBackend,
    ) -> Result<FuncRef, CompilerError> {
        use bone_semantic::hir::FnBackend;
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

    /// `await sub` — drive the inner coroutine `sub` through its `.resume()`,
    /// re-yielding each value; the expression's value is `sub`'s return value.
    /// Built as a normal pump loop with a `yield` inside, so the coroutine
    /// transform splits it like any other yield. Mirrors rock-codegen / old skull.
    pub(crate) fn lower_yield_delegate(&mut self, sub_expr: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        let rac = self.resume_arg.ok_or_else(|| gap("await outside a coroutine"))?;
        let sub = self.lower_expr(sub_expr)?;
        // Register `sub` as the active inner (kill() chains into it).
        self.emit_cur(Inst::CoroSetInner(sub));

        // A flat-enum delegation threads the inner's yield leaves through the
        // frame-slot channel (advance + read `sub.__frame.__y*`), re-yielding them
        // as leaves -- no per-yield box on either coroutine. Otherwise the boxed
        // path below drives the inner's generic `resume`.
        if let Some(shape) = self.coroutine_yield_flat_shape(sub_expr) {
            return self.lower_yield_delegate_flat(sub, rac, &shape);
        }

        let slot = self.f.new_cell(None);
        let resume0 = self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__resume") });
        let primed = self.emit_cur(Inst::CallValue { callee: resume0, args: vec![sub] });
        self.emit_cur(Inst::Store { cell: slot, value: primed });

        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        // header: loop while the inner's last yield wasn't nil (still producing).
        self.cur = header;
        let v = self.emit_cur(Inst::Load(slot));
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(v, nil));
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: exit, else_bb: body });

        // body: yield the inner's value, resume it with our resume arg.
        self.cur = body;
        let vv = self.emit_cur(Inst::Load(slot));
        let resume_bb = self.f.new_block();
        self.f.set_term(self.cur, Term::Yield { values: vec![vv], resume: resume_bb });
        self.cur = resume_bb;
        let arg = self.emit_cur(Inst::Load(rac));
        let resume1 = self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__resume") });
        let v2 = self.emit_cur(Inst::CallValue { callee: resume1, args: vec![sub, arg] });
        self.emit_cur(Inst::Store { cell: slot, value: v2 });
        self.f.set_term(self.cur, Term::Goto(header));

        // exit: clear the active inner, re-raise the inner's error if any, then
        // the `await` value is the inner's return.
        self.cur = exit;
        self.yield_delegate_exit(sub)
    }

    /// The `await` epilogue (shared by the boxed + flat channels): clear the
    /// active inner, re-raise the inner's error if any, evaluate to its return.
    /// The inner object IS its frame, so `__error`/`__return` are direct field
    /// reads (what the `__last_error`/`__result` methods read). Emitted into the
    /// current block.
    pub(crate) fn yield_delegate_exit(&mut self, sub: Idx<Inst>) -> Result<Idx<Inst>, CompilerError> {
        let clear = self.emit_cur(Inst::Const(Lit::Nil));
        self.emit_cur(Inst::CoroSetInner(clear));
        let inner_err = self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__error") });
        let nil2 = self.emit_cur(Inst::Const(Lit::Nil));
        let has_err = self.emit_cur(Inst::NotEq(inner_err, nil2));
        let raise = self.f.new_block();
        let cont = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: has_err, then_bb: raise, else_bb: cont });
        self.cur = raise;
        // `error` is the Lua builtin (a global), NOT a rock fn — read it as a
        // global and call indirectly, so re-raise works with no stdlib present.
        let err_fn = self.emit_cur(Inst::Global(Ustr::from("error")));
        self.emit_cur(Inst::CallValue { callee: err_fn, args: vec![inner_err] });
        self.f.set_term(self.cur, Term::Goto(cont));
        self.cur = cont;
        Ok(self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__return") }))
    }

    /// `await sub` where `sub` yields a flat enum: drive the inner via its
    /// frame-slot channel (`__advance` + the `__y*` slots on the object, which IS
    /// its frame) and re-yield the leaves through THIS coroutine's frame slots.
    /// Zero boxes on either side -- the outer transform copies `sub.__y*` into
    /// `s.__y*` at the yield point.
    pub(crate) fn lower_yield_delegate_flat(
        &mut self,
        sub: Idx<Inst>,
        rac: Idx<Cell>,
        shape: &EnumFlatShape,
    ) -> Result<Idx<Inst>, CompilerError> {
        let n = shape.leaf_count();
        let frame = sub;
        // Prime the inner (advance once).
        let adv0 = self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__advance") });
        self.emit_cur(Inst::CallValue { callee: adv0, args: vec![sub] });

        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        // header: loop while the inner is still producing (`__state >= 0`).
        self.cur = header;
        let state = self.emit_cur(Inst::Field { base: frame, field: Ustr::from("__state") });
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        let done = self.emit_cur(Inst::IntLt(state, zero));
        self.f.set_term(self.cur, Term::Branch { cond: done, then_bb: exit, else_bb: body });

        // body: re-yield the inner's current leaves, then advance it with our arg.
        self.cur = body;
        let values: Vec<Idx<Inst>> = (0..n)
            .map(|i| self.emit_cur(Inst::Field { base: frame, field: yield_slot(i) }))
            .collect();
        let resume_bb = self.f.new_block();
        self.f.set_term(self.cur, Term::Yield { values, resume: resume_bb });
        self.cur = resume_bb;
        let arg = self.emit_cur(Inst::Load(rac));
        let adv1 = self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__advance") });
        self.emit_cur(Inst::CallValue { callee: adv1, args: vec![sub, arg] });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        self.yield_delegate_exit(sub)
    }

    /// Resolve a global function by surface name to a [`FuncRef`], requiring a
    /// single workspace-wide definition (the `@nomangle` stdlib interp helpers —
    /// `__rock_format_float`, `inspect` — are unique, so they resolve here).
    pub(crate) fn resolve_global_fn(&self, name: &str) -> Result<FuncRef, CompilerError> {
        match self
            .ws
            .functions_by_name
            .get(&Ustr::from(name))
            .map(|l| l.as_slice())
        {
            Some([(module, func)]) => Ok(FuncRef { module: *module, func: *func }),
            _ => Err(gap(format!("interp helper `{name}` not uniquely resolvable"))),
        }
    }
}
