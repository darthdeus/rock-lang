//! Pattern matching: `match` as expression and statement, arm tests, scrutinee
//! payload reads (positional and named), tag testing, and the `if let` /
//! `while let` / `for` range forms built on the same machinery.
use super::*;

enum LetScrutinee {
    Generic,
    Nobox {
        binding: Idx<BindingInfo>,
        shape: ValueShape,
    },
    Tuple(TupleLayout),
}

impl<'a> Builder<'a> {
    /// `match scrut { pat => body, ... }` as a value. Each arm is a test block
    /// (branch to its body on match, else to the next arm) and a body block that
    /// binds, checks the guard, then stores its value into a result **cell** and
    /// jumps to the join. A sub-pattern that needs its own test (a nested enum)
    /// emits it before any child read, so a parent tag gates every child slot.
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
        let mut joined_at = None;
        self.lower_match_arms(scrut, arms, |s, body, join| {
            joined_at = Some(join);
            let bv = s.lower_expr_repr(body)?;
            s.join_arm(&mut values, bv)
        })?;
        let join = match joined_at {
            Some(join) => join,
            None => self.cur,
        };
        self.finish_join(values, join)
    }

    /// `match` in STATEMENT position (value discarded): same arm walk, each
    /// body lowered as a statement with NO result cell / store / load.
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

    /// The arm walk shared by every `match` position: per arm, a test block
    /// (failures jump to the next arm) and a guard check, then `sink` consumes
    /// the arm body and routes to `join`. Leaves `self.cur` at `join`.
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

        // Over a NULLABLE scrutinee, an arm that tests structure cannot match
        // nil and must not read through it (a tuple index or range compare on
        // nil is a runtime error): miss to the next arm first, and rebuild a
        // splatting tuple's flat box on the non-nil side. Once an UNGUARDED
        // null-matching arm has run, nil cannot reach a later arm.
        let scrut_nullable = self.expr_is_nullable(scrut);
        let mut nil_possible = scrut_nullable;
        for aidx in arms {
            let arm = self.body.match_arm_arena[aidx];
            let next = self.f.new_block();
            let matches_null = self.pattern_matches_null(arm.pattern, &variants);
            let arm_scrut = scrut_s.clone();
            if scrut_nullable && !matches_null {
                let val_v = self.scrut_value(&arm_scrut);
                // Skip the nil test once an unguarded null arm has run, but
                // the flat-box rebuild is a LAYOUT fact, not a nil fact: it
                // applies on every non-null arm.
                if nil_possible {
                    let cont = self.f.new_block();
                    self.branch_on_nil(val_v, next, cont);
                    self.cur = cont;
                }
            }
            if matches_null && arm.guard.is_none() {
                nil_possible = false;
            }
            // Emit this arm's pattern tests (failures jump to `next`); on success
            // `self.cur` is the block where the bindings are in scope.
            self.match_pattern(
                &arm_scrut,
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

    /// Recursively test `pat` against the value `val`, branching to `fail` on any
    /// mismatch and binding pattern variables on the success path. Advances
    /// `self.cur` to the block where matching continues (so a parent tag test
    /// always gates a child read — the access-path ordering). Emits no test for
    /// irrefutable forms (wildcard / bind / tuple-of-binds).
    pub(crate) fn match_pattern(
        &mut self,
        scrut: &Scrut,
        pat: Idx<Pattern>,
        fail: Idx<Block>,
        // The scrutinee enum's variant names (empty if not an enum). `top` is true
        // only at the pattern root — an unqualified `Red` there is a nullary
        // variant tag test, not a binding. `scrut_enum` is the scrutinee's enum
        // name (used at `top` to int-compare a payload-free tag); nested patterns
        // pass `None` and rely on their own `enum_name`.
        variants: &HashSet<Ustr>,
        top: bool,
        scrut_enum: Option<Ustr>,
        // The scrutinee's concrete type where it is known. A nested position
        // (payload, field, wrapped value, tuple element) is always storage, so
        // this is what tells a tuple sub-pattern its box is the flat wire.
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
                // `match c { Red => .. }` — bone lowers an unqualified nullary
                // variant as a catch-all bind; at the root, if the name is a
                // variant of the scrutinee enum it's a tag test (old skull parity).
                if top && variants.contains(&name) {
                    self.test_tag(scrut, name, scrut_enum, fail);
                    return Ok(());
                }
                // A threaded flat enum binds as its cells; boxing it here cost a
                // table that the arm's join or first payload read unpacked again.
                // Native backends bind their own tagged enum value.
                if let Scrut::Cells {
                    tag,
                    positional,
                    named,
                    shape,
                } = scrut
                {
                    let whole = self.binding_ty(b).and_then(|ty| self.concrete_ty_id(ty));
                    if !self.scx.native_flat_enums
                        && whole.is_some_and(|ty| ty.nominal() == Some(shape.name))
                    {
                        let mut leaves = Vec::with_capacity(shape.leaf_count());
                        leaves.push(*tag);
                        leaves.extend(positional.iter().copied());
                        leaves.extend(named.iter().map(|(_, value)| *value));
                        self.bind_leaves(b, &leaves, shape.value_shape())?;
                        return Ok(());
                    }
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
                // The scrutinee's storage still holds the bound box; a binder a
                // ref borrow can write through gets its own copy.
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
                // The variant's enum: the qualified name on the pattern, else the
                // scrutinee's enum (an unqualified `Variant` form). `test_tag`
                // gates on `payload_free_ordinal(en, variant)`: a payload-free
                // unit variant int-compares its ordinal; a payload-carrying
                // variant resolves to None there and takes the `.__variant` path.
                let en = enum_name.or(scrut_enum);
                self.test_tag(scrut, variant, en, fail);
                // A cdata enum's payload lives in its `_<i>` C field (cut 1: `_0`);
                // a table enum's is the positional `[i+1]` slot; a FLATTENED field's
                // is the parent's `{prefix}__i` slot (see `scrut_payload`).
                let cdata = en.is_some_and(|e| {
                    EnumPlan::for_name(self.ws, e).is_some_and(|plan| plan.cdata().is_some())
                });
                let flat_shape = en.and_then(|e| {
                    EnumPlan::for_name(self.ws, e).and_then(|plan| plan.flat().cloned())
                });
                for (i, sp) in sub.into_iter().enumerate() {
                    // POSITION -> SLOT. They differ once any position ahead of
                    // this one is widened: `V2(Vec2)` at position 0 spans two
                    // slots, so a sibling `Vec4Array(buf, n)` finds `n` at slot 2.
                    // Indexing by position read the widened value's second leaf
                    // and handed `nil` to the count.
                    let slot = flat_shape.as_ref().map_or(i, |s| s.slot_offset(i));
                    // Thread the sub-pattern's enum (from this variant's payload
                    // type) so a NESTED enum pattern (`L(J(x))`) tests its own tag
                    // repr -- else an unqualified `J` defaults to the `.__variant`
                    // table path and faults on a cdata inner enum.
                    let sub_enum = en.and_then(|e| self.variant_payload_enum(e, variant, i));
                    let payload_ty =
                        ty.and_then(|ty| self.member_ty(ty, variant, Member::Position(i)));
                    // A payload position widened to a `nobox` value's leaves binds
                    // THE LEAVES -- rebuilding a box here to bind one cell is the
                    // materialization the widening exists to avoid.
                    if let Some(vs) = self.flat_payload_leaf_shape(en, variant, i) {
                        if let Pattern::Bind(b) = self.body.pattern_arena[sp] {
                            let Some(shape) = flat_shape.as_ref() else {
                                return Err(gap("flat enum payload lost its compiler-owned shape"));
                            };
                            let leaves = self.scrut_payload_leaves(scrut, i, &vs, shape);
                            self.bind_leaves(b, &leaves, vs)?;
                            continue;
                        }
                        // A position widened for a NESTED FLAT ENUM holds that
                        // enum's own cells, not a value: the inner pattern has to
                        // test the tag SLOT and read the payload SLOTS. Reading the
                        // position as one value hands it the inner tag string, and
                        // `.__variant` on a string matches no arm.
                        if let Some(inner) = self.nested_flat_enum_scrut(scrut, slot, &vs)? {
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
                let name = if enum_name.or(scrut_enum).is_some() {
                    name.map(|name| Ustr::from(crate::mangle::declared_name(name.as_str())))
                } else {
                    name
                };
                let en = enum_name.or(scrut_enum);
                if let Some(variant) = name {
                    if enum_name.is_some() || (top && variants.contains(&variant)) {
                        // `test_tag` picks the repr: a cdata record enum compares
                        // `.__tag`, a table enum `.__variant`.
                        self.test_tag(scrut, variant, en, fail);
                    }
                }
                // A FLATTENED field reads its `{prefix}__<field>` slot; a cdata enum's
                // record payload lives in `_<field>` C slots; a table enum (or a plain
                // struct destructure, `name` = None) reads the field key directly.
                let cdata = name.is_some()
                    && en.is_some_and(|e| {
                        EnumPlan::for_name(self.ws, e).is_some_and(|plan| plan.cdata().is_some())
                    });
                // Snapshot a bound value before an arm can write back into its source.
                let struct_destructure = name
                    .is_some_and(|n| self.ws.struct_by_name.contains_key(&n))
                    && enum_name.is_none()
                    && !(top && name.is_some_and(|n| variants.contains(&n)));
                for rfidx in fields {
                    let rpf = self.body.record_pat_field_arena[rfidx];
                    // Embedded children need to be rebuilt from their parent's leaves.
                    let embedded = match (name, scrut) {
                        (Some(sname), Scrut::Val(_))
                            if self.ws.struct_by_name.contains_key(&sname) =>
                        {
                            self.struct_field_plan(sname, rpf.name, false)?
                                .embed()
                                .map(|(child, _)| (sname, child))
                        }
                        _ => None,
                    };
                    let fv = match (embedded, scrut) {
                        (Some((sname, cname)), Scrut::Val(v)) => {
                            let parent = *v;
                            if self.struct_field_plan(sname, rpf.name, false)?.nullable() {
                                self.materialize_embed_nullable(parent, rpf.name, cname)?
                            } else {
                                self.materialize_embed(parent, rpf.name, cname)?
                            }
                        }
                        _ => self.scrut_named(scrut, rpf.name, cdata)?,
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
                    let field_ty = match (ty, name) {
                        (Some(ty), Some(owner)) => {
                            self.member_ty(ty, owner, Member::Field(rpf.name))
                        }
                        _ => None,
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
                // Assumes an int discriminant (the common range-pattern case).
                let val = self.scrut_value(scrut);
                if let Some(s) = start {
                    let sv = self.lower_expr(s)?;
                    let cond = self.emit_cur(Inst::IntLeq(sv, val)); // start <= val
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
                // `A | B | C` matches if ANY alternative does: test each in turn,
                // success -> a shared `matched` continuation, mismatch -> the next
                // alternative (last -> the arm `fail`). Alternatives must be
                // binding-free (a binding set on only one branch would be undefined
                // on the others in the shared body).
                let alts: Vec<Idx<Pattern>> = alternatives.into_iter().collect();
                for &alt in &alts {
                    if self.pattern_binds(alt, variants, top) {
                        return Err(gap("or-pattern with bindings (later)"));
                    }
                }
                let matched = self.f.new_block();
                for (i, &alt) in alts.iter().enumerate() {
                    let is_last = i + 1 == alts.len();
                    let next_alt = if is_last { fail } else { self.f.new_block() };
                    // An or-pattern preserves `top`, so a top-level `A | B` on a
                    // payload-free enum keeps the int-compare path -> forward
                    // `scrut_enum` (nested alternatives carry `None` anyway).
                    self.match_pattern(scrut, alt, next_alt, variants, top, scrut_enum, ty)?;
                    // success path of this alternative -> the shared continuation.
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

    /// Whether `pat` introduces a real binding (not a top-level variant-name tag
    /// test). Used to reject or-pattern alternatives that bind.
    pub(crate) fn pattern_binds(
        &self,
        pat: Idx<Pattern>,
        variants: &HashSet<Ustr>,
        top: bool,
    ) -> bool {
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

    /// The enum NAME of payload field `i` of `enum_name::variant`, if that field's
    /// type is itself an enum (`None` otherwise) -- threaded into a nested match so
    /// the inner pattern tests the inner enum's tag repr.
    pub(crate) fn variant_payload_enum(
        &self,
        enum_name: Ustr,
        variant: Ustr,
        i: usize,
    ) -> Option<Ustr> {
        use bone_core::hir::VariantPayload;
        let eid = self.ws.enums_by_name.get(&enum_name)?;
        let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
        let v = ed.variants.iter().find(|v| v.name == variant)?;
        let ty = match &v.payload {
            VariantPayload::Tuple(tys) => *tys.get(i)?,
            _ => return None,
        };
        match static_ty(self.ws, ty)? {
            TypeExpr::App {
                ctor: TypeCtor::Nominal(n),
                ..
            } if self.ws.enums_by_name.contains_key(&n) => Some(n),
            _ => None,
        }
    }

    /// Lower a match/if-let/while-let scrutinee: a flattened enum field stays a
    /// `Scrut::Flat` (tag/payload read from the parent's slots, no box); anything
    /// else lowers to an ordinary value.
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

    /// Preserve a tuple element's flat-enum representation until the pattern
    /// proves it needs a whole value.
    pub(crate) fn scrut_from_repr(&mut self, repr: ValueRepr) -> Result<Scrut, CompilerError> {
        match repr {
            ValueRepr::Group { name, parts, .. }
                if EnumPlan::for_name(self.ws, name).is_some_and(|plan| plan.flat().is_some()) =>
            {
                let shape = EnumPlan::for_name(self.ws, name)
                    .and_then(|plan| plan.flat().cloned())
                    .ok_or_else(|| {
                        gap(format!("flat enum `{name}` lost its match representation"))
                    })?;
                let cells = self.repr_group_to_cells(&shape, parts)?;
                Ok(Scrut::Cells {
                    tag: cells.tag,
                    positional: cells.positional,
                    named: cells.named,
                    shape,
                })
            }
            ValueRepr::Tuple {
                parts,
                present: None,
            } => parts
                .into_iter()
                .map(|p| self.scrut_from_repr(p))
                .collect::<Result<_, _>>()
                .map(Scrut::Tuple),
            repr @ ValueRepr::Group { name, .. } if self.is_value_struct(name) => {
                let boxed = self.pack_maybe_absent(repr.clone())?;
                Ok(Scrut::Repr { repr, boxed })
            }
            other => Ok(Scrut::Val(self.pack_maybe_absent(other)?)),
        }
    }

    /// The scrutinee as an unpacked value, boxing only what is already a box.
    fn scrut_repr(&mut self, scrut: &Scrut) -> ValueRepr {
        match scrut {
            Scrut::Val(v) => ValueRepr::Scalar(*v),
            Scrut::Repr { repr, .. } => repr.clone(),
            Scrut::Tuple(parts) => ValueRepr::Tuple {
                parts: parts.iter().map(|p| self.scrut_repr(p)).collect(),
                present: None,
            },
            Scrut::Flat { .. } | Scrut::Cells { .. } => ValueRepr::Scalar(self.scrut_value(scrut)),
        }
    }

    /// The scrutinee as a whole value -- materializing a flattened field / threaded
    /// cells into a boxed enum (only on a path that genuinely needs the whole value).
    pub(crate) fn scrut_value(&mut self, scrut: &Scrut) -> Idx<Inst> {
        match scrut {
            Scrut::Val(v) | Scrut::Repr { boxed: v, .. } => *v,
            Scrut::Tuple(parts) => {
                let values = parts.iter().map(|p| self.scrut_value(p)).collect();
                self.element_tuple(values)
            }
            Scrut::Flat {
                parent,
                prefix,
                shape,
            } => self.materialize_flat(*parent, *prefix, shape),
            Scrut::Cells {
                tag,
                positional,
                named,
                shape,
            } => self.materialize_flat_cells(
                shape,
                FlatCells {
                    tag: *tag,
                    positional: positional.clone(),
                    named: named.clone(),
                },
            ),
        }
    }

    pub(crate) fn flat_payload_leaf_shape(
        &self,
        enum_name: Option<Ustr>,
        variant: Ustr,
        i: usize,
    ) -> Option<ValueShape> {
        use bone_core::hir::VariantPayload;
        let en = enum_name?;
        let shape = EnumPlan::for_name(self.ws, en).and_then(|plan| plan.flat().cloned())?;
        let eid = self.ws.enums_by_name.get(&en)?;
        let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
        let v = ed.variants.iter().find(|v| v.name == variant)?;
        let VariantPayload::Tuple(tys) = &v.payload else {
            return None;
        };
        let TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            ..
        } = static_ty(self.ws, *tys.get(i)?)?
        else {
            return None;
        };
        shape.position_shape(i, Some(n)).cloned()
    }

    /// The `width` slot values of a widened payload position, in leaf order.
    pub(crate) fn scrut_payload_leaves(
        &mut self,
        scrut: &Scrut,
        i: usize,
        vs: &ValueShape,
        shape: &EnumFlatShape,
    ) -> Vec<Idx<Inst>> {
        let off = shape.slot_offset(i);
        (0..vs.leaf_count())
            .map(|k| self.scrut_slot(scrut, off + k))
            .collect()
    }

    /// Read a nested flat enum beginning at its position in the outer wire.
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
        let width = shape.tuple_width();
        let leaves: Vec<Idx<Inst>> = (0..shape.leaf_count())
            .map(|k| self.scrut_slot(scrut, slot + k))
            .collect();
        let named = shape
            .record_fields
            .iter()
            .enumerate()
            .map(|(k, rf)| (*rf, leaves[1 + width + k]))
            .collect();
        Ok(Some(Scrut::Cells {
            tag: leaves[0],
            positional: leaves[1..1 + width].to_vec(),
            named,
            shape,
        }))
    }

    /// One raw positional SLOT of the scrutinee (not a position -- a slot).
    fn scrut_slot(&mut self, scrut: &Scrut, slot: usize) -> Idx<Inst> {
        match scrut {
            Scrut::Repr { boxed, .. } => self.scrut_slot(&Scrut::Val(*boxed), slot),
            Scrut::Cells { positional, .. } => positional[slot],
            Scrut::Flat { parent, prefix, .. } => {
                self.read_field(*parent, flat_payload_slot(*prefix, slot))
            }
            Scrut::Val(v) => self.emit_cur(Inst::TupleIndex {
                base: *v,
                index: slot,
            }),
            Scrut::Tuple(_) => {
                let value = self.scrut_value(scrut);
                self.emit_cur(Inst::TupleIndex {
                    base: value,
                    index: slot,
                })
            }
        }
    }

    /// The scrutinee's `i`th positional payload: threaded cells hold it directly; a
    /// flat field reads the parent's `{prefix}__i` slot; a cdata enum reads its
    /// `_<i>` C field; a table enum reads the positional `[i+1]` slot.
    pub(crate) fn scrut_payload(&mut self, scrut: &Scrut, i: usize, cdata: bool) -> Idx<Inst> {
        match scrut {
            Scrut::Repr { boxed, .. } => self.scrut_payload(&Scrut::Val(*boxed), i, cdata),
            Scrut::Cells { positional, .. } => positional[i],
            Scrut::Flat { parent, prefix, .. } => {
                self.read_field(*parent, flat_payload_slot(*prefix, i))
            }
            Scrut::Val(v) if cdata => self.read_field(*v, crate::names::indexed("_", "", i)),
            Scrut::Val(v) => self.emit_cur(Inst::TupleIndex { base: *v, index: i }),
            Scrut::Tuple(_) => {
                let value = self.scrut_value(scrut);
                self.emit_cur(Inst::TupleIndex {
                    base: value,
                    index: i,
                })
            }
        }
    }

    /// The scrutinee's named (record) payload `field`: threaded cells hold it
    /// directly; a flat field reads the parent's `{prefix}__<field>` slot; a cdata
    /// enum reads its `_<field>` C slot; a table enum reads the field key directly.
    pub(crate) fn scrut_named(
        &mut self,
        scrut: &Scrut,
        field: Ustr,
        cdata: bool,
    ) -> Result<Idx<Inst>, CompilerError> {
        match scrut {
            Scrut::Repr { boxed, .. } => self.scrut_named(&Scrut::Val(*boxed), field, cdata),
            Scrut::Cells { named, shape, .. } => named
                .iter()
                .find(|(n, _)| *n == field)
                .map(|(_, v)| *v)
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
            Scrut::Tuple(_) => {
                let value = self.scrut_value(scrut);
                Ok(self.read_field(value, field))
            }
        }
    }

    /// Tag test -> continue on match, else `fail`. Five reprs: a payload-free enum
    /// IS its ordinal int (compare ints); a cdata enum compares `.__tag` (int); a
    /// table enum compares `.__variant` (string); a FLATTENED field compares its
    /// `{prefix}__tag` slot; threaded CELLS compare the held tag (both strings).
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
            Scrut::Cells { tag, .. } => Some(*tag),
            Scrut::Val(_) | Scrut::Tuple(_) | Scrut::Repr { .. } => None,
        };
        if let Some(tag) = flat_tag {
            if let Inst::Const(Lit::Str(actual)) = self.f.insts[tag] {
                // Inactive flat payload lanes may carry a different literal type.
                let cont = self.f.new_block();
                self.f.set_term(
                    self.cur,
                    Term::Goto(if actual == variant { cont } else { fail }),
                );
                self.cur = cont;
                return;
            }
            let want = self.emit_cur(Inst::Const(Lit::Str(variant)));
            let cond = self.emit_cur(Inst::Eq(tag, want));
            self.branch_or_fail(cond, fail);
            return;
        }
        let val = self.scrut_value(scrut);
        if let Some(ord) = enum_name.and_then(|en| payload_free_ordinal(self.ws, en, variant)) {
            let want = self.emit_cur(Inst::Const(Lit::Int(ord)));
            let cond = self.emit_cur(Inst::Eq(val, want));
            self.branch_or_fail(cond, fail);
            return;
        }
        if let Some(ord) = enum_name
            .and_then(|en| EnumPlan::for_name(self.ws, en).and_then(|plan| plan.cdata().cloned()))
            .and_then(|s| s.tag_of(variant))
        {
            let tag = self.read_field(val, crate::names::ustr("__tag"));
            let want = self.emit_cur(Inst::Const(Lit::Int(ord)));
            let cond = self.emit_cur(Inst::Eq(tag, want));
            self.branch_or_fail(cond, fail);
            return;
        }
        let tag = self.read_field(val, crate::names::ustr("__variant"));
        let want = self.emit_cur(Inst::Const(Lit::Str(variant)));
        let cond = self.emit_cur(Inst::Eq(tag, want));
        self.branch_or_fail(cond, fail);
    }

    /// `cond` true -> a fresh continuation block (`self.cur` advances), false ->
    /// `fail`.
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
        self.lower_if_let_inner(
            pattern,
            value,
            then_branch,
            else_branch,
            result_shape,
            false,
        )?
        .ok_or_else(|| gap("value-position if let produced no result"))
    }

    pub(crate) fn lower_if_let_stmt(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<(), CompilerError> {
        self.lower_if_let_inner(pattern, value, then_branch, else_branch, None, true)
            .map(drop)
    }

    fn lower_if_let_inner(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
        result_shape: Option<RepLayout>,
        statement: bool,
    ) -> Result<Option<ValueRepr>, CompilerError> {
        let scalar_result = (!statement && result_shape.is_none()).then(|| {
            let ty = self.current_expr_unit_storage_ty_id();
            self.f.new_cell_typed(None, ty)
        });
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.lower_let_test(pattern, value, else_bb)?;

        if statement {
            self.lower_statement_arms(then_branch, else_branch, else_bb, join)?;
            Ok(None)
        } else if let Some(shape) = result_shape {
            let values = self.value_join(shape);
            self.lower_value_arms(
                values,
                else_bb,
                join,
                |s| s.lower_expr_repr(then_branch),
                else_branch,
            )
            .map(Some)
        } else {
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
            Ok(Some(ValueRepr::Scalar(self.emit_cur(Inst::Load(result)))))
        }
    }

    /// How an `if let`/`while let` tests its scrutinee. A nullable `nobox`
    /// binding and a names-only pattern over a nullable tuple branch on the
    /// presence leaf they carry, so neither packs a box to compare with nil.
    fn let_scrutinee(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
    ) -> Result<LetScrutinee, CompilerError> {
        if let Pattern::Bind(binding) = self.body.pattern_arena[pattern]
            // A bind named like a scrutinee variant is a TAG TEST, not a
            // binding -- it must go through the generic match machinery.
            && !self
                .scrut_variants(value)
                .contains(&self.body.binding_arena[binding].name)
            && self.expr_is_nullable(value)
            && let Some(shape) = self.nobox_binding_shape(binding)?.or(self
                .nullable_value_shape_of(value)?
                .map(|mut shape| {
                    shape.nullable = false;
                    shape
                }))
        {
            return Ok(LetScrutinee::Nobox { binding, shape });
        }
        if self.pattern_is_names_only(pattern)
            && let Some(shape) = self.nullable_tuple_wire_shape(value)?
        {
            return Ok(LetScrutinee::Tuple(TupleLayout {
                slots: shape.slots,
                nullable: false,
            }));
        }
        Ok(LetScrutinee::Generic)
    }

    /// Test `value` against an `if let`/`while let` pattern. A miss jumps to
    /// `miss`; a match leaves `self.cur` where the pattern's names are bound.
    fn lower_let_test(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        miss: Idx<Block>,
    ) -> Result<(), CompilerError> {
        let kind = self.let_scrutinee(pattern, value)?;
        if let LetScrutinee::Generic = kind {
            let scrut = self.lower_scrut(value)?;
            return self.match_let(pattern, value, &scrut, miss);
        }
        let (present, values) = self.lower_present_let(value, &kind)?;
        let then_bb = self.f.new_block();
        self.branch_present_let(present, then_bb, miss);
        self.bind_present_let(pattern, kind, &values)
    }

    /// `let <pattern> = value` with a pattern beyond names and tuples: run the
    /// general match machinery with an unreachable miss block (bone proved the
    /// pattern irrefutable).
    pub(crate) fn lower_let_match(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        // `miss` keeps its default Unreachable terminator: no value escapes it.
        let miss = self.f.new_block();
        self.lower_let_test(pattern, value, miss)
    }

    /// `let <pattern> = value else { .. }`: test and bind exactly like
    /// `if let`, but the bindings live on in the ENCLOSING block and the else
    /// block diverges (checked in bone), so there is no join to build.
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
        // The else end is unreachable (it diverges); fall back to the match
        // path, where the statement list continues with the bindings live.
        self.cur = cont;
        Ok(())
    }

    /// A nullable scrutinee misses on nil before its pattern is matched;
    /// otherwise a plain binding would always match and bind nil.
    fn match_let(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        scrut: &Scrut,
        miss: Idx<Block>,
    ) -> Result<(), CompilerError> {
        let variants = self.scrut_variants(value);
        let scrut_enum = self.scrut_enum_name(value);
        self.match_let_inner(pattern, value, scrut, miss, &variants, scrut_enum)
    }

    fn match_let_inner(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        scrut: &Scrut,
        miss: Idx<Block>,
        variants: &HashSet<Ustr>,
        scrut_enum: Option<Ustr>,
    ) -> Result<(), CompilerError> {
        let scrut_ty = self.concrete_expr_ty(value).map(|ty| ty.peel_nullable().0);
        if self.expr_is_nullable(value) {
            let val_v = self.scrut_value(scrut);
            let cont = self.f.new_block();
            self.branch_on_nil(val_v, miss, cont);
            self.cur = cont;
        }
        self.match_pattern(scrut, pattern, miss, variants, true, scrut_enum, scrut_ty)
    }

    /// The concrete type of one member of a value of type `owner_ty`: a
    /// variant's payload position or record field (`owner` names the variant),
    /// a struct field, or a newtype's wrapped value. `owner_ty`'s own generic
    /// arguments substitute the declaration, so a nested generic instance
    /// answers for itself.
    fn member_ty(&self, owner_ty: &MonoTy, owner: Ustr, member: Member) -> Option<MonoTy> {
        use bone_core::hir::VariantPayload;
        let MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params,
        } = owner_ty
        else {
            return None;
        };
        let (generics, decl) = if let Some(eid) = self.ws.enums_by_name.get(name) {
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
        } else if let Some(sid) = self.ws.struct_by_name.get(name) {
            let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
            let Member::Field(f) = member else {
                return None;
            };
            (&sd.generics, sd.fields.iter().find(|sf| sf.name == f)?.ty)
        } else {
            let nid = self.ws.newtypes_by_name.get(name)?;
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

    /// Read the flat slots of `val_v` back into per-element scruts.
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

    /// Whether `pat` can match a NULL scrutinee: a hole, a plain binding, the
    /// `null` literal, or an or-pattern with such an alternative. Every other
    /// top-level pattern tests structure, which a nil cannot carry.
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

    /// The presence and bound leaves of a presence-carrying scrutinee.
    fn lower_present_let(
        &mut self,
        value: Idx<Expr>,
        kind: &LetScrutinee,
    ) -> Result<(Idx<Inst>, Vec<Idx<Inst>>), CompilerError> {
        match kind {
            LetScrutinee::Nobox { shape, .. } => {
                let mut scrut_shape = shape.clone();
                scrut_shape.nullable = true;
                let bundle = self.lower_expr_repr(value)?;
                let owned = self.snapshot_opaque_value(bundle.value().clone())?;
                let bundle = ValueBundle::new(bundle.representation(), owned);
                let leaves =
                    self.repr_to_slot_values_bundle(&bundle, &RepLayout::Leaves(scrut_shape))?;
                let (&present, values) = leaves
                    .split_first()
                    .expect("a nullable shape carries its presence leaf first");
                Ok((present, values.to_vec()))
            }
            LetScrutinee::Tuple(shape) => {
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
                    shape,
                )?;
                Ok((present, slots))
            }
            LetScrutinee::Generic => Err(gap("a generic let scrutinee carries no presence")),
        }
    }

    fn branch_present_let(&mut self, present: Idx<Inst>, then_bb: Idx<Block>, miss: Idx<Block>) {
        self.branch_to(present, then_bb, miss);
        self.cur = then_bb;
    }

    fn bind_present_let(
        &mut self,
        pattern: Idx<Pattern>,
        kind: LetScrutinee,
        values: &[Idx<Inst>],
    ) -> Result<(), CompilerError> {
        match kind {
            LetScrutinee::Nobox { binding, shape } => self.bind_leaves(binding, values, shape),
            LetScrutinee::Tuple(shape) => {
                let mut idx = 0;
                self.bind_names_slot(pattern, &RepLayout::Tuple(shape), values, &mut idx)
            }
            LetScrutinee::Generic => Ok(()),
        }
    }

    /// The DECLARED nullable-tuple shape of a call, without lowering it.
    ///
    /// Read off the CALLEE's return type, never off `e`'s own type: safe-nav
    /// gives `h?.pair()` the same `Nullable(Tuple)` with no presence slot, so
    /// keying on the expression type would branch on a slot that is not there.
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
        if self.ws.newtypes_by_name.contains_key(&name) {
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
        // The CALLEE's instantiation, via the same oracle `lower_call` uses --
        // not the enclosing body's `spec_subst`, which is empty in a
        // non-generic caller and left every slot `Opaque`.
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

    /// Whether `e` is one of the three `?.` forms -- the expressions whose join
    /// mints its own presence tag.
    pub(crate) fn is_safe_nav(&self, e: Idx<Expr>) -> bool {
        matches!(
            self.body.expr_arena[e],
            Expr::SafeNav { .. } | Expr::SafeNavCall { .. } | Expr::SafeNavLet { .. }
        )
    }

    /// Whether `pat` is names and holes only -- irrefutable, so the presence slot
    /// is the ONLY test the match needs. Anything that carries its own test (a
    /// literal, a variant, a record) is excluded and still matches against the
    /// packed scrutinee.
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

    /// `for i in lo..hi { body }` (exclusive): a header testing `i < hi`, a body,
    /// a **latch** that increments `i` (so `continue` still steps), and an exit.
    /// The loop var and the loop bound are both cells.
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

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i_v = self.emit_cur(Inst::Load(i_cell));
        let h_v = self.emit_cur(Inst::Load(hi_cell));
        let is_uint = matches!(self.arith_ty(low), Some(ArithTy::Uint))
            || matches!(self.arith_ty(high), Some(ArithTy::Uint));
        let cmp = if is_uint {
            self.emit_cur(Inst::UintLt(i_v, h_v))
        } else {
            self.emit_cur(Inst::IntLt(i_v, h_v))
        };
        self.branch_to(cmp, body_bb, exit);

        self.cur = body_bb;
        self.push_loop_target(latch, exit, self.defers.len()); // continue -> latch (runs the step)
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        self.goto_if_open(latch);

        self.cur = latch;
        let i_v2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(if is_uint {
            Lit::Uint(1)
        } else {
            Lit::Int(1)
        }));
        let inc = if is_uint {
            self.emit_cur(Inst::UintAdd(i_v2, one))
        } else {
            self.emit_cur(Inst::IntAdd(i_v2, one))
        };
        self.store_cell(i_cell, inc);
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Member {
    Position(usize),
    Field(Ustr),
    Wrapped,
}
