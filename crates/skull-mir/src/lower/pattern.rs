use super::*;

impl<'a> Builder<'a> {
    /// `match scrut { pat => body, ... }` as a value. A nested sub-pattern's test
    /// is emitted before any child read, so a parent tag gates every child slot.
    pub(crate) fn lower_match(
        &mut self,
        scrut: Idx<Expr>,
        arms: comfy_la_arena::IdxRange<MatchArm>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let repr = self.lower_match_repr(scrut, arms, RepLayout::Opaque)?;
        self.pack_maybe_absent(repr)
    }

    pub(crate) fn lower_match_repr(
        &mut self,
        scrut: Idx<Expr>,
        arms: comfy_la_arena::IdxRange<MatchArm>,
        shape: RepLayout,
    ) -> Result<ValueRepr, CompilerError> {
        let mut values = self.value_join(shape);
        self.lower_match_arms(scrut, arms, |s, body, _| {
            let bv = s.lower_expr_repr(body)?;
            s.join_arm(&mut values, bv)
        })?;
        self.finish_join(values, self.cur)
    }

    pub(crate) fn lower_match_stmt(
        &mut self,
        scrut: Idx<Expr>,
        arms: comfy_la_arena::IdxRange<MatchArm>,
    ) -> Result<(), CompilerError> {
        self.lower_match_arms(scrut, arms, |s, body, join| {
            s.lower_stmt_expr(body)?;
            s.goto_if_open(join);
            Ok(())
        })
    }

    fn lower_match_arms(
        &mut self,
        scrut: Idx<Expr>,
        arms: comfy_la_arena::IdxRange<MatchArm>,
        mut sink: impl FnMut(&mut Self, Idx<Expr>, Idx<Block>) -> Result<(), CompilerError>,
    ) -> Result<(), CompilerError> {
        let variants = self.scrut_variants(scrut);
        let scrut_enum = self.scrut_enum_name(scrut);
        let scrut_ty = self.concrete_expr_ty(scrut).map(|ty| ty.peel_nullable().0);
        let scrut_s = self.lower_scrut(scrut)?;
        let join = self.f.new_block();

        // Over a nullable scrutinee, a structural arm must miss on nil before reading
        // through it: a tuple index or range compare on nil is a runtime error.
        let scrut_nullable = self.expr_is_nullable(scrut);
        let mut nil_possible = scrut_nullable;
        for aidx in arms {
            let arm = self.body.match_arm_arena[aidx];
            let next = self.f.new_block();
            let matches_null = self.pattern_matches_null(arm.pattern, &variants);
            if scrut_nullable && !matches_null {
                let value = self.scrut_value(&scrut_s);
                // The nil test can be skipped after an unguarded null arm, but the flat-box
                // rebuild is a layout fact and applies on every non-null arm.
                if nil_possible {
                    let cont = self.f.new_block();
                    self.branch_on_nil(value, next, cont);
                    self.cur = cont;
                }
            }
            if matches_null && arm.guard.is_none() {
                nil_possible = false;
            }
            self.match_pattern(
                &scrut_s,
                arm.pattern,
                next,
                &variants,
                true,
                scrut_enum,
                scrut_ty,
            )?;
            if let Some(g) = arm.guard {
                let kept = self.f.new_block();
                self.lower_branch(g, kept, next, false)?;
                self.cur = kept;
            }
            sink(self, arm.body, join)?;
            self.cur = next;
        }
        // No arm matched: bone checks exhaustiveness, so this is unreachable.
        self.f.set_term(self.cur, Term::Unreachable);
        self.cur = join;
        Ok(())
    }

    fn match_pattern(
        &mut self,
        scrut: &Scrut,
        pat: Idx<Pattern>,
        fail: Idx<Block>,
        variants: &HashSet<Ustr>,
        top: bool,
        scrut_enum: Option<Ustr>,
        // A nested position is always storage, so this tells a tuple sub-pattern
        // its box is the flat wire.
        ty: Option<&MonoTy>,
    ) -> Result<(), CompilerError> {
        match self.body.pattern_arena[pat] {
            Pattern::Wildcard => Ok(()),
            // A newtype is transparent: its inner pattern tests the same value.
            Pattern::Newtype { name, sub, .. } => {
                let wrapped = ty.and_then(|ty| self.member_ty(ty, name, Member::Wrapped));
                self.match_pattern(
                    scrut,
                    sub,
                    fail,
                    &HashSet::default(),
                    false,
                    None,
                    wrapped.as_ref(),
                )
            }
            Pattern::Bind(b) => {
                let name = self.body.binding_arena[b].name;
                // Bone lowers an unqualified nullary variant as a bind; at the root, a
                // variant name of the scrutinee enum is a tag test.
                if top && variants.contains(&name) {
                    self.test_tag(scrut, name, scrut_enum, fail);
                    return Ok(());
                }
                // A threaded flat enum binds as its cells; boxing it would allocate a table
                // that the join or first payload read unpacks again.
                if let Scrut::Cells { cells, shape } = scrut {
                    let whole = self.binding_ty(b).and_then(|ty| self.concrete_ty_id(ty));
                    if whole.is_some_and(|ty| ty.nominal() == Some(shape.name)) {
                        self.bind_leaves(b, &cells.slots, shape.value_shape())?;
                        return Ok(());
                    }
                }
                if let Scrut::Leaves { leaves, shape } = scrut {
                    return self.bind_leaves(b, leaves, shape.clone());
                }
                if let Some(shape) = self.whole_tuple_binding(b)? {
                    let whole = self.scrut_repr(scrut);
                    let slots = self.tuple_repr_to_slots(whole, &shape)?;
                    self.bind_tuple_slots(b, &slots, shape);
                    return Ok(());
                }
                if let Scrut::Repr { repr, .. } = scrut
                    && !self.binding_is_ref(b)
                    && !self.binding_ref_borrowed(b)
                {
                    return self.bind_loop_element_repr(&BindingPattern::Leaf(b), repr.clone());
                }
                let mut val = self.scrut_value(scrut);
                // The scrutinee's storage still holds the bound box; a binder a ref borrow
                // can write through gets its own copy.
                if !self.binding_is_ref(b)
                    && self.binding_ref_borrowed(b)
                    && let Some(ty) = self.binding_ty(b).and_then(|ty| self.concrete_ty_id(ty))
                {
                    val = self.copied_value_box(ty, val)?;
                }
                let cell =
                    self.new_binding_cell(b, name, self.binding_ty(b), crate::names::ustr(""));
                self.store_cell(cell, val);
                self.binds.insert(raw(b), cell);
                Ok(())
            }
            Pattern::Literal(le) => {
                if let Scrut::Leaves { leaves, shape } = scrut
                    && self.is_null_lit(le)
                {
                    let absent = if shape.nullable {
                        self.emit_cur(Inst::Not(leaves[0]))
                    } else {
                        self.emit_cur(Inst::Const(Lit::Bool(false)))
                    };
                    self.branch_or_fail(absent, fail);
                    return Ok(());
                }
                let val = self.scrut_value(scrut);
                let lv = self.lower_expr(le)?;
                let cond = self.emit_cur(Inst::Eq(val, lv));
                self.branch_or_fail(cond, fail);
                Ok(())
            }
            Pattern::EnumVariant {
                enum_name,
                variant,
                sub,
                ..
            } => {
                let en = enum_name.or(scrut_enum);
                self.test_tag(scrut, variant, en, fail);
                let plan = en.and_then(|e| EnumPlan::for_name(self.ws, e));
                let cdata = plan.as_ref().is_some_and(|plan| plan.cdata().is_some());
                let flat_shape = plan.as_ref().and_then(|plan| plan.flat());
                for (i, sp) in sub.into_iter().enumerate() {
                    // Position and slot differ once an earlier position is widened: `V2(Vec2)`
                    // at position 0 spans two slots.
                    let slot = flat_shape.map_or(i, |s| s.slot_offset(i));
                    // A nested enum pattern needs its own enum, else an unqualified variant takes
                    // the `.__variant` table path and faults on a cdata inner enum.
                    let sub_enum = en.and_then(|e| self.variant_payload_enum(e, variant, i));
                    let payload_ty =
                        ty.and_then(|ty| self.member_ty(ty, variant, Member::Position(i)));
                    // A position widened to a `nobox` value's leaves binds the leaves; rebuilding
                    // a box here is the allocation the widening exists to avoid.
                    if let Some(vs) =
                        flat_shape.and_then(|shape| shape.payload_shape(self.ws, variant, i))
                    {
                        if let Pattern::Bind(b) = self.body.pattern_arena[sp] {
                            let leaves = self.scrut_payload_leaves(scrut, slot, vs);
                            self.bind_leaves(b, &leaves, vs.clone())?;
                            continue;
                        }
                        if super::shape::is_nobox_nominal(self.ws, vs.name)
                            && !matches!(self.body.pattern_arena[sp], Pattern::Wildcard)
                        {
                            let leaves = self.scrut_payload_leaves(scrut, slot, vs);
                            self.match_pattern(
                                &Scrut::Leaves {
                                    leaves,
                                    shape: vs.clone(),
                                },
                                sp,
                                fail,
                                variants,
                                false,
                                sub_enum,
                                payload_ty.as_ref(),
                            )?;
                            continue;
                        }
                        // A position widened for a nested flat enum holds that enum's cells, not a
                        // value: test its tag slot and read its payload slots.
                        if let Some(inner) = self.nested_flat_enum_scrut(scrut, slot, vs)? {
                            self.match_pattern(
                                &inner,
                                sp,
                                fail,
                                variants,
                                false,
                                sub_enum,
                                payload_ty.as_ref(),
                            )?;
                            continue;
                        }
                    }

                    let payload = self.scrut_payload(scrut, slot, cdata);

                    self.match_pattern(
                        &Scrut::Val(payload),
                        sp,
                        fail,
                        variants,
                        false,
                        sub_enum,
                        payload_ty.as_ref(),
                    )?;
                }
                Ok(())
            }
            Pattern::Tuple { elements } => {
                // A boxed splatting tuple is the flat wire in a table.
                let stored = match (scrut, ty) {
                    (Scrut::Val(v), Some(ty)) if !self.is_tuple_aggregate(*v) => self
                        .concrete_rep_tuple(ty, RepUse::Return, true, false)?
                        .filter(|shape| shape.splats(self.scx.rep_arena()))
                        .map(|shape| self.rebuild_splat_box(shape, *v))
                        .transpose()?,
                    _ => None,
                };
                let scrut = stored.as_ref().unwrap_or(scrut);
                let elem_tys = match ty {
                    Some(MonoTy::Tuple(elems)) => elems.as_slice(),
                    _ => &[],
                };
                if let Scrut::Tuple(parts) = scrut {
                    for (i, sp) in elements.into_iter().enumerate() {
                        let elem = parts.get(i).ok_or_else(|| {
                            gap("tuple pattern exceeds its scrutinee representation")
                        })?;
                        self.match_pattern(elem, sp, fail, variants, false, None, elem_tys.get(i))?;
                    }
                } else {
                    let val = self.scrut_value(scrut);
                    for (i, sp) in elements.into_iter().enumerate() {
                        let elem = self.emit_cur(Inst::TupleIndex {
                            base: val,
                            index: i,
                        });
                        self.match_pattern(
                            &Scrut::Val(elem),
                            sp,
                            fail,
                            variants,
                            false,
                            None,
                            elem_tys.get(i),
                        )?;
                    }
                }
                Ok(())
            }
            Pattern::Record {
                enum_name,
                name,
                fields,
                ..
            } => {
                if let Scrut::Leaves { leaves, shape } = scrut {
                    if shape.nullable {
                        self.branch_or_fail(leaves[0], fail);
                    }
                    for rfidx in fields {
                        let field = self.body.record_pat_field_arena[rfidx];
                        if matches!(self.body.pattern_arena[field.pat], Pattern::Wildcard) {
                            continue;
                        }
                        let (range, child_shape) = self
                            .value_projection_range(shape, &[field.name])
                            .ok_or_else(|| gap("record pattern field has no payload leaves"))?;
                        let field_ty = ty.and_then(|ty| {
                            self.member_ty(ty, shape.name, Member::Field(field.name))
                        });
                        let child = match child_shape {
                            FieldShape::Prim => Scrut::Val(leaves[range.start]),
                            FieldShape::Nested(shape) => {
                                let leaves = &leaves[range];
                                if let Some(flat) = EnumPlan::for_name(self.ws, shape.name)
                                    .and_then(|plan| plan.flat().cloned())
                                {
                                    Scrut::Cells {
                                        cells: FlatCells::from_slots(&flat, leaves),
                                        shape: flat,
                                    }
                                } else {
                                    Scrut::Leaves {
                                        leaves: leaves.to_vec(),
                                        shape,
                                    }
                                }
                            }
                        };
                        self.match_pattern(
                            &child,
                            field.pat,
                            fail,
                            variants,
                            false,
                            None,
                            field_ty.as_ref(),
                        )?;
                    }
                    return Ok(());
                }
                let name = if enum_name.or(scrut_enum).is_some() {
                    name.map(|name| Ustr::from(crate::mangle::declared_name(name.as_str())))
                } else {
                    name
                };
                let en = enum_name.or(scrut_enum);
                if let Some(variant) = name {
                    if enum_name.is_some() || (top && variants.contains(&variant)) {
                        self.test_tag(scrut, variant, en, fail);
                    }
                }
                let cdata = name.is_some()
                    && en.is_some_and(|e| {
                        EnumPlan::for_name(self.ws, e).is_some_and(|plan| plan.cdata().is_some())
                    });
                // Snapshot a bound value before an arm can write back into its source.
                let struct_destructure = name
                    .is_some_and(|n| self.ws.nominals.struct_id(n).is_some())
                    && enum_name.is_none()
                    && !(top && name.is_some_and(|n| variants.contains(&n)));
                if en.is_none() {
                    let owner = name.or_else(|| match ty.map(|ty| ty.peel_nullable().0) {
                        Some(crate::monoty::MonoTy::App {
                            ctor: crate::monoty::MonoCtor::Nominal(name),
                            ..
                        }) => Some(*name),
                        _ => None,
                    });
                    if let Some(owner) = owner {
                        let value = self.scrut_value(scrut);
                        self.mark_record_owner(value, Some(owner));
                    }
                }
                let plan = en.and_then(|e| EnumPlan::for_name(self.ws, e));
                let flat = plan.as_ref().and_then(|plan| plan.flat());
                for rfidx in fields {
                    let rpf = self.body.record_pat_field_arena[rfidx];
                    let field_ty = match (ty, name) {
                        (Some(ty), Some(owner)) => {
                            self.member_ty(ty, owner, Member::Field(rpf.name))
                        }
                        _ => None,
                    };
                    if let Some(payload) = flat.and_then(|flat| flat.named_payload(name?, rpf.name))
                    {
                        let leaves = self.scrut_named_payload_leaves(scrut, &payload.keys)?;
                        self.match_pattern(
                            &Scrut::Leaves {
                                leaves,
                                shape: payload.shape.clone(),
                            },
                            rpf.pat,
                            fail,
                            variants,
                            false,
                            None,
                            field_ty.as_ref(),
                        )?;
                        continue;
                    }
                    // Embedded children are rebuilt from their parent's leaves.
                    let embedded = match (name, scrut) {
                        (Some(sname), Scrut::Val(parent))
                            if self.ws.nominals.struct_id(sname).is_some() =>
                        {
                            let plan = self.struct_field_plan(sname, rpf.name, false)?;
                            plan.embed()
                                .map(|(child, _)| (*parent, child, plan.nullable()))
                        }
                        _ => None,
                    };
                    let fv = match embedded {
                        Some((parent, child, true)) => {
                            self.materialize_embed_nullable(parent, rpf.name, child)?
                        }
                        Some((parent, child, false)) => {
                            self.materialize_embed(parent, rpf.name, child)?
                        }
                        None => self.scrut_named(scrut, rpf.name, cdata)?,
                    };
                    let fv = match self.body.pattern_arena[rpf.pat] {
                        Pattern::Bind(b)
                            if struct_destructure
                                && !self.binding_is_ref(b)
                                && !self.binding_ref_borrowed(b) =>
                        {
                            match self.binding_ty(b).and_then(|ty| self.concrete_ty_id(ty)) {
                                Some(ty) => self.copied_value_box(ty, fv)?,
                                None => fv,
                            }
                        }
                        _ => fv,
                    };
                    self.match_pattern(
                        &Scrut::Val(fv),
                        rpf.pat,
                        fail,
                        variants,
                        false,
                        None,
                        field_ty.as_ref(),
                    )?;
                }
                Ok(())
            }
            Pattern::Range {
                start,
                end,
                inclusive,
            } => {
                // Assumes an int discriminant.
                let val = self.scrut_value(scrut);
                if let Some(s) = start {
                    let sv = self.lower_expr(s)?;
                    let cond = self.emit_cur(Inst::IntLeq(sv, val));
                    self.branch_or_fail(cond, fail);
                }
                if let Some(e) = end {
                    let ev = self.lower_expr(e)?;
                    let cond = if inclusive {
                        self.emit_cur(Inst::IntLeq(val, ev))
                    } else {
                        self.emit_cur(Inst::IntLt(val, ev))
                    };
                    self.branch_or_fail(cond, fail);
                }
                Ok(())
            }
            Pattern::Or { alternatives } => {
                // Alternatives must be binding-free: a binding set on only one branch would
                // be undefined on the others in the shared body.
                for alt in alternatives {
                    if self.pattern_binds(alt, variants, top) {
                        return Err(gap("or-pattern with bindings (later)"));
                    }
                }
                let matched = self.f.new_block();
                for (i, alt) in alternatives.into_iter().enumerate() {
                    let is_last = i + 1 == alternatives.len();
                    let next_alt = if is_last { fail } else { self.f.new_block() };
                    // An or-pattern preserves `top`, so a top-level `A | B` on a payload-free
                    // enum keeps the int-compare path and needs `scrut_enum`.
                    self.match_pattern(scrut, alt, next_alt, variants, top, scrut_enum, ty)?;
                    self.goto_if_open(matched);
                    if !is_last {
                        self.cur = next_alt;
                    }
                }
                self.cur = matched;
                Ok(())
            }
        }
    }

    fn pattern_binds(&self, pat: Idx<Pattern>, variants: &HashSet<Ustr>, top: bool) -> bool {
        match self.body.pattern_arena[pat] {
            Pattern::Wildcard | Pattern::Literal(_) | Pattern::Range { .. } => false,
            Pattern::Bind(b) => {
                let name = self.body.binding_arena[b].name;
                !(top && variants.contains(&name))
            }
            Pattern::EnumVariant { sub, .. } => sub
                .into_iter()
                .any(|p| self.pattern_binds(p, variants, false)),
            Pattern::Tuple { elements } => elements
                .into_iter()
                .any(|p| self.pattern_binds(p, variants, false)),
            Pattern::Record { fields, .. } => fields.into_iter().any(|f| {
                self.pattern_binds(self.body.record_pat_field_arena[f].pat, variants, false)
            }),
            Pattern::Or { alternatives } => alternatives
                .into_iter()
                .any(|p| self.pattern_binds(p, variants, top)),
            Pattern::Newtype { sub, .. } => self.pattern_binds(sub, variants, false),
        }
    }

    pub(crate) fn variant_payload_enum(
        &self,
        enum_name: Ustr,
        variant: Ustr,
        i: usize,
    ) -> Option<Ustr> {
        let ty = shape::enum_position_ty(self.ws, enum_name, variant, i)?;
        match static_ty(self.ws, ty)? {
            TypeExpr::App {
                ctor: TypeCtor::Nominal(n),
                ..
            } if self.ws.nominals.enum_id(n).is_some() => Some(n),
            _ => None,
        }
    }

    /// A flattened enum field stays a `Scrut::Flat` read from the parent's slots,
    /// with no box.
    pub(crate) fn lower_scrut(&mut self, e: Idx<Expr>) -> Result<Scrut, CompilerError> {
        if let Expr::FieldAccess { base, field } = self.body.expr_arena[e] {
            if let Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(sname),
                ..
            }) = self.field_base_ty(base)
            {
                if let Some(shape) = self.struct_field_plan(sname, field, false)?.flat_shape() {
                    let parent = self.lower_expr(base)?;
                    return Ok(Scrut::Flat {
                        parent,
                        prefix: field,
                        shape,
                    });
                }
            }
        }
        if let Expr::Tuple(elements) = self.body.expr_arena[e] {
            let mut parts = Vec::with_capacity(elements.len());
            for element in elements {
                let repr = self.lower_expr_repr(element)?.into_value();
                parts.push(self.scrut_from_repr(repr)?);
            }
            return Ok(Scrut::Tuple(parts));
        }
        let repr = self.lower_expr_repr(e)?.into_value();
        self.scrut_from_repr(repr)
    }

    pub(crate) fn scrut_from_repr(&mut self, repr: ValueRepr) -> Result<Scrut, CompilerError> {
        match repr {
            ValueRepr::Group {
                name,
                parts,
                present,
            } => {
                let plan = EnumPlan::for_name(self.ws, name);
                if let Some(shape) = plan.as_ref().and_then(|plan| plan.flat()) {
                    let cells = self.repr_group_to_cells(shape, parts)?;
                    Ok(Scrut::Cells {
                        cells,
                        shape: shape.clone(),
                    })
                } else {
                    let repr = ValueRepr::Group {
                        name,
                        parts,
                        present,
                    };
                    if self.is_value_struct(name) {
                        let boxed = self.pack_maybe_absent(repr.clone())?;
                        Ok(Scrut::Repr { repr, boxed })
                    } else {
                        Ok(Scrut::Val(self.pack_maybe_absent(repr)?))
                    }
                }
            }
            ValueRepr::Tuple {
                parts,
                present: None,
            } => parts
                .into_iter()
                .map(|p| self.scrut_from_repr(p))
                .collect::<Result<_, _>>()
                .map(Scrut::Tuple),
            other => Ok(Scrut::Val(self.pack_maybe_absent(other)?)),
        }
    }

    fn scrut_repr(&mut self, scrut: &Scrut) -> ValueRepr {
        match scrut {
            Scrut::Val(v) => ValueRepr::Scalar(*v),
            Scrut::Repr { repr, .. } => repr.clone(),
            Scrut::Leaves { leaves, shape } => Self::rebuild_from_leaves(shape, leaves),
            Scrut::Tuple(parts) => ValueRepr::Tuple {
                parts: parts.iter().map(|p| self.scrut_repr(p)).collect(),
                present: None,
            },
            Scrut::Flat { .. } | Scrut::Cells { .. } => ValueRepr::Scalar(self.scrut_value(scrut)),
        }
    }

    /// Materializes a flattened field or threaded cells into a boxed enum; call it
    /// only on a path that needs the whole value.
    pub(crate) fn scrut_value(&mut self, scrut: &Scrut) -> Idx<Inst> {
        match scrut {
            Scrut::Val(v) | Scrut::Repr { boxed: v, .. } => *v,
            Scrut::Leaves { .. } => unreachable!("a leaf record has no scalar scrutinee"),
            Scrut::Tuple(parts) => {
                let values = parts.iter().map(|p| self.scrut_value(p)).collect();
                self.element_tuple(values)
            }
            Scrut::Flat {
                parent,
                prefix,
                shape,
            } => self.materialize_flat(*parent, *prefix, shape),
            Scrut::Cells { cells, shape } => self.materialize_flat_cells(shape, cells.clone()),
        }
    }

    pub(crate) fn scrut_payload_leaves(
        &mut self,
        scrut: &Scrut,
        offset: usize,
        shape: &ValueShape,
    ) -> Vec<Idx<Inst>> {
        (offset..offset + shape.leaf_count())
            .map(|slot| self.scrut_payload(scrut, slot, false))
            .collect()
    }

    pub(crate) fn scrut_named_payload_leaves(
        &mut self,
        scrut: &Scrut,
        keys: &[Ustr],
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        keys.iter()
            .map(|&key| self.scrut_named(scrut, key, false))
            .collect()
    }

    pub(crate) fn nested_flat_enum_scrut(
        &mut self,
        scrut: &Scrut,
        slot: usize,
        vs: &ValueShape,
    ) -> Result<Option<Scrut>, CompilerError> {
        let Some(plan) = EnumPlan::for_name(self.ws, vs.name) else {
            return Ok(None);
        };
        let Some(shape) = plan.flat().cloned() else {
            return Ok(None);
        };
        if vs.leaf_count() < shape.leaf_count() {
            return Err(gap(format!(
                "nested flat enum `{}` reads {} slots but position reserved {}",
                shape.name,
                shape.leaf_count(),
                vs.leaf_count()
            )));
        }
        let leaves: Vec<_> = (0..shape.leaf_count())
            .map(|k| self.scrut_payload(scrut, slot + k, false))
            .collect();
        Ok(Some(Scrut::Cells {
            cells: FlatCells::from_slots(&shape, &leaves),
            shape,
        }))
    }

    /// The `i`th positional payload: threaded cells hold it directly; a flat field
    /// reads the parent's `{prefix}__i` slot; a cdata enum reads its `_<i>` C
    /// field; a table enum reads the positional `[i+1]` slot.
    pub(crate) fn scrut_payload(&mut self, scrut: &Scrut, i: usize, cdata: bool) -> Idx<Inst> {
        match scrut {
            Scrut::Repr { boxed, .. } => self.scrut_payload(&Scrut::Val(*boxed), i, cdata),
            Scrut::Cells { cells, .. } => cells.positional()[i],
            Scrut::Flat { parent, prefix, .. } => {
                self.read_field(*parent, flat_payload_slot(*prefix, i))
            }
            Scrut::Val(v) if cdata => self.read_field(*v, crate::names::indexed("_", "", i)),
            Scrut::Val(v) => self.emit_cur(Inst::TupleIndex { base: *v, index: i }),
            Scrut::Tuple(_) | Scrut::Leaves { .. } => {
                let value = self.scrut_value(scrut);
                self.emit_cur(Inst::TupleIndex {
                    base: value,
                    index: i,
                })
            }
        }
    }

    /// The named payload `field`: threaded cells hold it directly; a flat field
    /// reads the parent's `{prefix}__<field>` slot; a cdata enum reads its
    /// `_<field>` C slot; a table enum reads the field key directly.
    pub(crate) fn scrut_named(
        &mut self,
        scrut: &Scrut,
        field: Ustr,
        cdata: bool,
    ) -> Result<Idx<Inst>, CompilerError> {
        match scrut {
            Scrut::Repr { boxed, .. } => self.scrut_named(&Scrut::Val(*boxed), field, cdata),
            Scrut::Cells { cells, shape } => cells
                .named()
                .find(|(n, _)| *n == field)
                .map(|(_, v)| v)
                .ok_or_else(|| {
                    gap(format!(
                        "record field `{field}` is absent from flat enum `{}`",
                        shape.name
                    ))
                }),
            Scrut::Flat { parent, prefix, .. } => {
                Ok(self.read_field(*parent, flat_named_slot(*prefix, field)))
            }
            Scrut::Val(v) => {
                let f = if cdata {
                    slot_name(field.as_str())
                } else {
                    field
                };
                Ok(self.read_field(*v, f))
            }
            Scrut::Tuple(_) | Scrut::Leaves { .. } => {
                let value = self.scrut_value(scrut);
                Ok(self.read_field(value, field))
            }
        }
    }

    /// Tag test. A payload-free enum is its ordinal int; a cdata enum compares
    /// `.__tag` (int); a table enum compares `.__variant` (string); a flattened
    /// field compares its `{prefix}__tag` slot; threaded cells compare the held tag.
    pub(crate) fn test_tag(
        &mut self,
        scrut: &Scrut,
        variant: Ustr,
        enum_name: Option<Ustr>,
        fail: Idx<Block>,
    ) {
        let flat_tag = match scrut {
            Scrut::Flat { parent, prefix, .. } => {
                Some(self.read_field(*parent, flat_tag_slot(*prefix)))
            }
            Scrut::Cells { cells, .. } => Some(cells.tag()),
            Scrut::Val(_) | Scrut::Tuple(_) | Scrut::Repr { .. } | Scrut::Leaves { .. } => None,
        };
        let (tag, literal) = if let Some(tag) = flat_tag {
            // Inactive flat payload lanes may carry a different literal type.
            if let Inst::Const(Lit::Str(actual)) = self.f.insts[tag] {
                let cont = self.f.new_block();
                self.f.set_term(
                    self.cur,
                    Term::Goto(if actual == variant { cont } else { fail }),
                );
                self.cur = cont;
                return;
            }
            (tag, Lit::Str(variant))
        } else {
            let val = self.scrut_value(scrut);
            if let Some(ord) = enum_name.and_then(|en| payload_free_ordinal(self.ws, en, variant)) {
                (val, Lit::Int(ord))
            } else if let Some(ord) = enum_name
                .and_then(|en| EnumPlan::for_name(self.ws, en))
                .and_then(|plan| plan.cdata()?.tag_of(variant))
            {
                (
                    self.read_field(val, crate::names::ustr("__tag")),
                    Lit::Int(ord),
                )
            } else {
                (
                    self.read_field(val, crate::names::ustr("__variant")),
                    Lit::Str(variant),
                )
            }
        };
        let want = self.emit_cur(Inst::Const(literal));
        let cond = self.emit_cur(Inst::Eq(tag, want));
        self.branch_or_fail(cond, fail);
    }

    pub(crate) fn branch_or_fail(&mut self, cond: Idx<Inst>, fail: Idx<Block>) {
        let cont = self.f.new_block();
        self.branch_to(cond, cont, fail);
        self.cur = cont;
    }

    pub(crate) fn lower_if_let(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
        result_shape: Option<RepLayout>,
    ) -> Result<ValueRepr, CompilerError> {
        let scalar_result = result_shape.is_none().then(|| {
            let ty = self.current_expr_unit_storage_ty_id();
            self.f.new_cell_typed(None, ty)
        });
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.lower_let_test(pattern, value, else_bb)?;

        if let Some(shape) = result_shape {
            let values = self.value_join(shape);
            return self.lower_value_arms(
                values,
                else_bb,
                join,
                |s| s.lower_expr_repr(then_branch),
                else_branch,
            );
        }

        let result = scalar_result.expect("scalar if let has a result cell");
        let tv = self.lower_expr(then_branch)?;
        self.store_and_join(result, tv, join);

        self.cur = else_bb;
        let ev = match else_branch {
            Some(eb) => self.lower_expr(eb)?,
            None => self.emit_cur(Inst::Const(Lit::Nil)),
        };
        self.store_and_join(result, ev, join);

        self.cur = join;
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::Load(result))))
    }

    pub(crate) fn lower_if_let_stmt(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<(), CompilerError> {
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.lower_let_test(pattern, value, else_bb)?;
        self.lower_statement_arms(then_branch, else_branch, else_bb, join)
    }

    /// A nullable `nobox` binding and a names-only pattern over a nullable tuple
    /// branch on their presence leaf, so neither packs a box to compare with nil.
    /// A nullable scrutinee misses on nil before its pattern is matched;
    /// otherwise a plain binding would always match and bind nil.
    fn lower_let_test(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        miss: Idx<Block>,
    ) -> Result<(), CompilerError> {
        if let Pattern::Bind(binding) = self.body.pattern_arena[pattern]
            // A bind named like a scrutinee variant is a tag test, not a binding.
            && !self
                .scrut_variants(value)
                .contains(&self.body.binding_arena[binding].name)
            && self.expr_is_nullable(value)
            && let Some(mut shape) = self
                .nobox_binding_shape(binding)?
                .or(self.nullable_value_shape_of(value)?)
        {
            shape.nullable = false;
            let scrut_shape = ValueShape {
                nullable: true,
                ..shape.clone()
            };
            let bundle = self.lower_expr_repr(value)?;
            let owned = self.snapshot_opaque_value(bundle.value().clone())?;
            let bundle = ValueBundle::new(bundle.representation(), owned);
            let leaves =
                self.repr_to_slot_values_bundle(&bundle, &RepLayout::Leaves(scrut_shape))?;
            let (&present, values) = leaves
                .split_first()
                .expect("a nullable shape carries its presence leaf first");
            self.branch_or_fail(present, miss);
            return self.bind_leaves(binding, values, shape);
        }

        if self.pattern_is_names_only(pattern)
            && let Some(mut shape) = self.nullable_tuple_wire_shape(value)?
        {
            shape.nullable = false;
            let repr = self.lower_expr_repr(value)?.into_value();
            let ValueRepr::Tuple {
                parts,
                present: Some(present),
            } = repr
            else {
                return Err(gap(
                    "a nullable tuple scrutinee arrived without its presence slot",
                ));
            };
            let slots = self.tuple_repr_to_slots(
                ValueRepr::Tuple {
                    parts,
                    present: None,
                },
                &shape,
            )?;
            self.branch_or_fail(present, miss);
            return self.bind_names_slot(pattern, &RepLayout::Tuple(shape), &slots, &mut 0);
        }

        let scrut = self.lower_scrut(value)?;
        let variants = self.scrut_variants(value);
        let scrut_enum = self.scrut_enum_name(value);
        let scrut_ty = self.concrete_expr_ty(value).map(|ty| ty.peel_nullable().0);
        if self.expr_is_nullable(value) {
            let val = self.scrut_value(&scrut);
            let cont = self.f.new_block();
            self.branch_on_nil(val, miss, cont);
            self.cur = cont;
        }
        self.match_pattern(&scrut, pattern, miss, &variants, true, scrut_enum, scrut_ty)
    }

    pub(crate) fn lower_let_match(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        // Bone proved the pattern irrefutable, so `miss` stays Unreachable.
        let miss = self.f.new_block();
        self.lower_let_test(pattern, value, miss)
    }

    pub(crate) fn lower_let_else(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        else_block: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let else_bb = self.f.new_block();
        self.lower_let_test(pattern, value, else_bb)?;
        let cont = self.cur;
        self.cur = else_bb;
        self.lower_stmt_expr(else_block)?;
        // Bone checks that the else block diverges, so its end is unreachable.
        self.cur = cont;
        Ok(())
    }

    fn member_ty(&self, owner_ty: &MonoTy, owner: Ustr, member: Member) -> Option<MonoTy> {
        use bone_core::hir::VariantPayload;
        let MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params,
        } = owner_ty
        else {
            return None;
        };
        let (generics, decl) = if let Some(eid) = self.ws.nominals.enum_id(*name) {
            let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
            let v = ed.variants.iter().find(|v| v.name == owner)?;
            let decl = match (&v.payload, member) {
                (VariantPayload::Tuple(tys), Member::Position(i)) => *tys.get(i)?,
                (VariantPayload::Record(fields), Member::Field(f)) => {
                    fields.iter().find(|rf| rf.name == f)?.ty
                }
                _ => return None,
            };
            (&ed.generics, decl)
        } else if let Some(sid) = self.ws.nominals.struct_id(*name) {
            let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
            let Member::Field(f) = member else {
                return None;
            };
            (&sd.generics, sd.fields.iter().find(|sf| sf.name == f)?.ty)
        } else {
            let nid = self.ws.nominals.newtype_id(*name)?;
            let nd = &self.ws.modules[nid.module.index()].newtypes[nid.def];
            (&nd.generics, nd.wrapped)
        };
        let mut subst = SpecSubst::default();
        for (g, p) in generics.iter().zip(params.iter()) {
            if let Some(TypeExpr::Generic(v, _)) = static_ty(self.ws, *g) {
                subst.insert(v, p.clone());
            }
        }
        crate::monoty::convert_decl(self.ws, decl, &subst).ok()
    }

    fn rebuild_splat_box(
        &mut self,
        shape: TupleLayout,
        val_v: Idx<Inst>,
    ) -> Result<Scrut, CompilerError> {
        let shape = TupleLayout {
            nullable: false,
            ..shape
        };
        let slots: Vec<Idx<Inst>> = (0..shape.slot_count(self.scx.rep_arena()))
            .map(|i| {
                self.emit_cur(Inst::TupleIndex {
                    base: val_v,
                    index: i,
                })
            })
            .collect();
        let mut idx = 0;
        let ValueRepr::Tuple { parts, .. } = self.tuple_from_slots(&shape, &slots, &mut idx) else {
            return Err(gap("a splatting tuple shape rebuilt to a non-tuple repr"));
        };
        let parts = parts
            .into_iter()
            .map(|p| self.scrut_from_repr(p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Scrut::Tuple(parts))
    }

    /// Every top-level pattern other than a hole, a binding, `null` or an
    /// or-pattern of those tests structure, which nil cannot carry.
    fn pattern_matches_null(&self, pat: Idx<Pattern>, variants: &HashSet<Ustr>) -> bool {
        match self.body.pattern_arena[pat] {
            Pattern::Wildcard => true,
            Pattern::Bind(b) => !variants.contains(&self.body.binding_arena[b].name),
            Pattern::Literal(e) => matches!(self.body.expr_arena[e], Expr::Null),
            Pattern::Or { alternatives } => alternatives
                .into_iter()
                .any(|a| self.pattern_matches_null(a, variants)),
            _ => false,
        }
    }

    /// Read off the callee's return type, never `e`'s own: safe-nav gives
    /// `h?.pair()` the same `Nullable(Tuple)` with no presence slot.
    pub(crate) fn nullable_tuple_call_shape(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<TupleLayout>, CompilerError> {
        let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } = self.body.expr_arena[e]
        else {
            return Ok(None);
        };
        if self.ws.nominals.newtype_id(name).is_some() {
            return Ok(None);
        }
        let arg_idxs: Vec<_> = args.into_iter().collect();
        if self.fn_field_of_call(e).is_some() {
            return Ok(None);
        }
        let any_named = arg_idxs
            .iter()
            .any(|a| self.body.arg_names.contains_key(a) || self.body.spread_args.contains(a));
        if !any_named && self.is_fn_global(name) {
            return Ok(None);
        }
        // The callee's instantiation, not this body's `spec_subst`, which is empty in
        // a non-generic caller.
        let Some(shape) = self.call_ret_tuple_shape(e)? else {
            return Ok(None);
        };
        Ok(shape.nullable.then_some(shape))
    }

    /// An actual nullable tuple wire carries presence; a boxed tuple can simply be nil.
    pub(crate) fn nullable_tuple_wire_shape(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<TupleLayout>, CompilerError> {
        if let Expr::Var(b) = self.body.expr_arena[e] {
            let Some((_, shape)) = self.tuple_binds.get(&raw(b)) else {
                return Ok(None);
            };
            return Ok(shape.nullable.then(|| shape.clone()));
        }
        if self.is_safe_nav(e) {
            let Some(inner) = self
                .peeled_tuple_shape(e)?
                .filter(|shape| shape.splats(self.scx.rep_arena()))
            else {
                return Ok(None);
            };
            return Ok(Some(TupleLayout {
                nullable: true,
                ..inner
            }));
        }
        self.nullable_tuple_call_shape(e)
    }

    pub(crate) fn is_safe_nav(&self, e: Idx<Expr>) -> bool {
        matches!(
            self.body.expr_arena[e],
            Expr::SafeNav { .. } | Expr::SafeNavCall { .. } | Expr::SafeNavLet { .. }
        )
    }

    /// Names and holes only: irrefutable, so the presence slot is the only test.
    fn pattern_is_names_only(&self, pat: Idx<Pattern>) -> bool {
        match self.body.pattern_arena[pat] {
            Pattern::Bind(_) | Pattern::Wildcard => true,
            Pattern::Tuple { elements } => {
                elements.into_iter().all(|p| self.pattern_is_names_only(p))
            }
            _ => false,
        }
    }

    fn bind_names_slot(
        &mut self,
        pat: Idx<Pattern>,
        slot: &RepLayout,
        values: &[Idx<Inst>],
        idx: &mut usize,
    ) -> Result<(), CompilerError> {
        match self.body.pattern_arena[pat] {
            Pattern::Wildcard => *idx += slot.slot_count(self.scx.rep_arena()),
            Pattern::Bind(b) => {
                self.bind_pattern_slot(&BindingPattern::Leaf(b), slot, values, idx)?;
            }
            Pattern::Tuple { elements } => {
                let RepLayout::Tuple(inner) = slot else {
                    return Err(gap("a tuple pattern against a non-tuple wire slot"));
                };
                if elements.len() != inner.slots.len() {
                    return Err(gap(
                        "a tuple pattern of a different arity than its wire slot",
                    ));
                }
                for (p, &s) in elements.into_iter().zip(&inner.slots) {
                    self.bind_names_slot(p, self.scx.rep_arena().layout(s), values, idx)?;
                }
            }
            _ => return Err(gap("bind_names_slot on a pattern that needs a test")),
        }
        Ok(())
    }

    /// The latch increments the loop variable, so `continue` still steps.
    pub(crate) fn lower_for_range(
        &mut self,
        var_b: Idx<BindingInfo>,
        low: Idx<Expr>,
        high: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let i_cell = self.new_binding_cell(
            var_b,
            self.body.binding_arena[var_b].name,
            self.binding_ty(var_b),
            crate::names::ustr(""),
        );
        self.binds.insert(raw(var_b), i_cell);
        self.f.mark_loop_binding(i_cell);
        let lo_v = self.lower_expr(low)?;
        self.store_cell(i_cell, lo_v);
        let hi_v = self.lower_expr(high)?;
        let hi_cell = self.f.new_cell(None);
        self.store_cell(hi_cell, hi_v);

        let is_uint = matches!(self.arith_ty(low), Some(ArithTy::Uint))
            || matches!(self.arith_ty(high), Some(ArithTy::Uint));
        self.lower_index_loop(
            i_cell,
            is_uint,
            body,
            |s| s.emit_cur(Inst::Load(hi_cell)),
            |_, _| Ok(()),
        )
    }
}

#[derive(Clone, Copy)]
enum Member {
    Position(usize),
    Field(Ustr),
    Wrapped,
}
