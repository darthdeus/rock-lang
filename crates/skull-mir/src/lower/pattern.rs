//! Pattern matching: `match` as expression and statement, arm tests, scrutinee
//! payload reads (positional and named), tag testing, and the `if let` /
//! `while let` / `for` range forms built on the same machinery.
use super::*;

impl Builder<'_> {
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
        let repr = self.lower_match_repr(scrut, arms, RetSlot::Opaque)?;
        self.pack_maybe_absent(repr)
    }

    pub(crate) fn lower_match_repr(
        &mut self,
        scrut: Idx<Expr>,
        arms: comfy_la_arena::IdxRange<MatchArm>,
        shape: RetSlot,
    ) -> Result<ValueRepr, CompilerError> {
        let results: Vec<Idx<Cell>> =
            (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
        let variants = self.scrut_variants(scrut);
        let scrut_enum = self.scrut_enum_name(scrut);
        let scrut_s = self.lower_scrut(scrut)?;
        let join = self.f.new_block();

        for aidx in arms {
            let arm = self.body.match_arm_arena[aidx];
            let next = self.f.new_block();
            // Emit this arm's pattern tests (failures jump to `next`); on success
            // `self.cur` is the block where the bindings are in scope.
            self.match_pattern(&scrut_s, arm.pattern, next, &variants, true, scrut_enum)?;
            if let Some(g) = arm.guard {
                let kept = self.f.new_block();
                self.lower_branch(g, kept, next, false)?;
                self.cur = kept;
            }
            let bv = self.lower_expr_repr(arm.body)?;
            self.store_bundle_and_join(&results, &shape, &bv, join)?;

            self.cur = next;
        }
        // No arm matched: bone checks exhaustiveness, so this is unreachable.
        self.f.set_term(self.cur, Term::Unreachable);

        self.cur = join;
        let slots: Vec<Idx<Inst>> =
            results.iter().map(|&cell| self.emit_cur(Inst::Load(cell))).collect();
        let mut idx = 0;
        Ok(self.slot_repr(&shape, &slots, &mut idx))
    }

    /// `match` in STATEMENT position (value discarded): mirrors [`Self::lower_match`]
    /// but lowers each arm body as a statement with NO result cell / store / load.
    pub(crate) fn lower_match_stmt(
        &mut self,
        scrut: Idx<Expr>,
        arms: comfy_la_arena::IdxRange<MatchArm>,
    ) -> Result<(), CompilerError> {
        let variants = self.scrut_variants(scrut);
        let scrut_enum = self.scrut_enum_name(scrut);
        let scrut_s = self.lower_scrut(scrut)?;
        let join = self.f.new_block();
        for aidx in arms {
            let arm = self.body.match_arm_arena[aidx];
            let next = self.f.new_block();
            self.match_pattern(&scrut_s, arm.pattern, next, &variants, true, scrut_enum)?;
            if let Some(g) = arm.guard {
                let kept = self.f.new_block();
                self.lower_branch(g, kept, next, false)?;
                self.cur = kept;
            }
            self.lower_stmt_expr(arm.body)?;
            if self.is_open() {
                self.f.set_term(self.cur, Term::Goto(join));
            }
            self.cur = next;
        }
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
    ) -> Result<(), CompilerError> {
        match self.body.pattern_arena[pat].clone() {
            Pattern::Wildcard => Ok(()),
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
                if let Scrut::Cells { tag, positional, named, shape } = scrut {
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
                let mut val = self.scrut_value(scrut);
                // The scrutinee's storage still holds the bound box; a binder a
                // ref borrow can write through gets its own copy.
                if !self.binding_is_ref(b)
                    && self.binding_ref_borrowed(b)
                    && let Some(ty) = self.binding_ty(b).and_then(|ty| self.concrete_ty_id(ty))
                {
                    val = self.copied_value_box(&ty, val)?;
                }
                let cell = self.new_binding_cell(
                    b,
                    name,
                    self.binding_ty(b),
                    Ustr::from(""),
                );
                self.emit_cur(Inst::Store { cell, value: val });
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
            Pattern::EnumVariant { enum_name, variant, sub, .. } => {
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
                            self.match_pattern(&inner, sp, fail, variants, false, sub_enum)?;
                            continue;
                        }
                    }

                    let payload = self.scrut_payload(scrut, slot, cdata);

                    self.match_pattern(&Scrut::Val(payload), sp, fail, variants, false, sub_enum)?;
                }
                Ok(())
            }
            Pattern::Tuple { elements } => {
                if let Scrut::Tuple(parts) = scrut {
                    for (i, sp) in elements.into_iter().enumerate() {
                        let elem = parts
                            .get(i)
                            .ok_or_else(|| gap("tuple pattern exceeds its scrutinee representation"))?;
                        self.match_pattern(elem, sp, fail, variants, false, None)?;
                    }
                } else {
                    let val = self.scrut_value(scrut);
                    for (i, sp) in elements.into_iter().enumerate() {
                        let elem = self.emit_cur(Inst::TupleIndex { base: val, index: i });
                        self.match_pattern(&Scrut::Val(elem), sp, fail, variants, false, None)?;
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
                // Emit the discriminant tag test for an enum-variant record
                // pattern: qualified (`enum_name` set) OR an unqualified variant
                // name belonging to the scrutinee enum (`top`, like the Bind case).
                // A plain struct-destructure record pattern (`name` not a variant)
                // emits none. Without this, an unqualified `Circle { .. }` arm with
                // no tag test would match ANY variant and index missing fields.
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
                // A struct destructure reads fields out of an owner the arm can
                // still write (`src.w.f0 = 99`), so a bound value field is a
                // snapshot, as `let w = src.w` is. An enum payload cannot be
                // written through its owner; its binder copies where a ref
                // borrow could write it, and its `V?` descendants where it
                // escapes ([`Self::owned_value_children`]).
                let struct_destructure = name.is_some_and(|n| self.ws.struct_by_name.contains_key(&n))
                    && enum_name.is_none()
                    && !(top && name.is_some_and(|n| variants.contains(&n)));
                for rfidx in fields {
                    let rpf = self.body.record_pat_field_arena[rfidx];
                    // An embed/nobox field has NO slot under its own name, so a
                    // BINDING for it rebuilds from the parent's dotted leaves --
                    // the same rule the field-read paths follow. `scrut_named`
                    // would emit `Inst::Field` against a key that does not exist.
                    // The shared field-storage plan fails closed on anything that is not
                    // a plain struct (an enum variant name is not in
                    // `struct_by_name`), so a variant record falls through.
                    let embedded = match (name, scrut) {
                        (Some(sname), Scrut::Val(_)) if self.ws.struct_by_name.contains_key(&sname) => {
                            self
                                .struct_field_plan(sname, rpf.name, false)?
                                .embed()
                                .map(|(child, _)| (sname, child))
                        }
                        _ => None,
                    };
                    let fv = match (embedded, scrut) {
                        (Some((sname, cname)), Scrut::Val(v)) => {
                            let parent = *v;
                            if self
                                .struct_field_plan(sname, rpf.name, false)?
                                .nullable()
                            {
                                self.materialize_embed_nullable(parent, rpf.name, cname)?
                            } else {
                                self.materialize_embed(parent, rpf.name, cname)?
                            }
                        }
                        _ => self.scrut_named(scrut, rpf.name, cdata)?,
                    };
                    let fv = match self.body.pattern_arena[rpf.pat] {
                        Pattern::Bind(b)
                            if struct_destructure && !self.binding_is_ref(b) && !self.binding_ref_borrowed(b) =>
                        {
                            match self.binding_ty(b).and_then(|ty| self.concrete_ty_id(ty)) {
                                Some(ty) => self.copied_value_box(&ty, fv)?,
                                None => fv,
                            }
                        }
                        _ => fv,
                    };
                    self.match_pattern(&Scrut::Val(fv), rpf.pat, fail, variants, false, None)?;
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
                    self.match_pattern(scrut, alt, next_alt, variants, top, scrut_enum)?;
                    // success path of this alternative -> the shared continuation.
                    if self.is_open() {
                        self.f.set_term(self.cur, Term::Goto(matched));
                    }
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
    pub(crate) fn pattern_binds(&self, pat: Idx<Pattern>, variants: &HashSet<Ustr>, top: bool) -> bool {
        match self.body.pattern_arena[pat].clone() {
            Pattern::Wildcard | Pattern::Literal(_) | Pattern::Range { .. } => false,
            Pattern::Bind(b) => {
                let name = self.body.binding_arena[b].name;
                !(top && variants.contains(&name))
            }
            Pattern::EnumVariant { sub, .. } => {
                sub.into_iter().any(|p| self.pattern_binds(p, variants, false))
            }
            Pattern::Tuple { elements } => {
                elements.into_iter().any(|p| self.pattern_binds(p, variants, false))
            }
            Pattern::Record { fields, .. } => fields
                .into_iter()
                .any(|f| self.pattern_binds(self.body.record_pat_field_arena[f].pat, variants, false)),
            Pattern::Or { alternatives } => {
                alternatives.into_iter().any(|p| self.pattern_binds(p, variants, top))
            }
        }
    }

    /// The enum NAME of payload field `i` of `enum_name::variant`, if that field's
    /// type is itself an enum (`None` otherwise) -- threaded into a nested match so
    /// the inner pattern tests the inner enum's tag repr.
    pub(crate) fn variant_payload_enum(&self, enum_name: Ustr, variant: Ustr, i: usize) -> Option<Ustr> {
        use bone_semantic::hir::VariantPayload;
        let eid = self.ws.enums_by_name.get(&enum_name)?;
        let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
        let v = ed.variants.iter().find(|v| v.name == variant)?;
        let ty = match &v.payload {
            VariantPayload::Tuple(tys) => *tys.get(i)?,
            _ => return None,
        };
        match static_ty(self.ws, ty)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if self.ws.enums_by_name.contains_key(&n) => {
                Some(n)
            }
            _ => None,
        }
    }

    /// Lower a match/if-let/while-let scrutinee: a flattened enum field stays a
    /// `Scrut::Flat` (tag/payload read from the parent's slots, no box); anything
    /// else lowers to an ordinary value.
    pub(crate) fn lower_scrut(&mut self, e: Idx<Expr>) -> Result<Scrut, CompilerError> {
        if let Expr::FieldAccess { base, field } = self.body.expr_arena[e].clone() {
            if let Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. }) = self.field_base_ty(base) {
                if let Some(shape) = self.struct_field_plan(sname, field, false)?.flat_shape() {
                    let parent = self.lower_expr(base)?;
                    return Ok(Scrut::Flat { parent, prefix: field, shape });
                }
            }
        }
        if let Expr::Tuple(elements) = self.body.expr_arena[e].clone() {
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
                    .ok_or_else(|| gap(format!("flat enum `{name}` lost its match representation")))?;
                let cells = self.repr_group_to_cells(&shape, parts)?;
                Ok(Scrut::Cells {
                    tag: cells.tag,
                    positional: cells.positional,
                    named: cells.named,
                    shape,
                })
            }
            other => Ok(Scrut::Val(self.pack_maybe_absent(other)?)),
        }
    }

    /// The scrutinee as a whole value -- materializing a flattened field / threaded
    /// cells into a boxed enum (only on a path that genuinely needs the whole value).
    pub(crate) fn scrut_value(&mut self, scrut: &Scrut) -> Idx<Inst> {
        match scrut {
            Scrut::Val(v) => *v,
            Scrut::Tuple(parts) => {
                let values = parts.iter().map(|p| self.scrut_value(p)).collect();
                self.emit_cur(Inst::MakeTuple(values))
            }
            Scrut::Flat { parent, prefix, shape } => self.materialize_flat(*parent, *prefix, shape),
            Scrut::Cells { tag, positional, named, shape } => self.materialize_flat_cells(
                shape,
                FlatCells {
                    tag: *tag,
                    positional: positional.clone(),
                    named: named.clone(),
                },
            ),
        }
    }

    /// The leaf shape of THIS VARIANT's payload at position `i`, when the position
    /// is widened to a scalarizable value's leaves. `None` for every ordinary position.
    ///
    /// Both the position AND the variant have to be asked. A position is widened
    /// because SOME variant carries a scalarizable value there; a sibling variant with
    /// an ordinary payload at the same position occupies only the first slot, and
    /// reading it through the widened shape binds a float as if it were a value
    /// struct. Read off the enum DECLARATION, the same source the ctor's placement
    /// reads, so a write and a read of one position cannot disagree.
    pub(crate) fn flat_payload_leaf_shape(
        &self,
        enum_name: Option<Ustr>,
        variant: Ustr,
        i: usize,
    ) -> Option<ValueShape> {
        use bone_semantic::hir::VariantPayload;
        let en = enum_name?;
        let shape = EnumPlan::for_name(self.ws, en).and_then(|plan| plan.flat().cloned())?;
        let eid = self.ws.enums_by_name.get(&en)?;
        let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
        let v = ed.variants.iter().find(|v| v.name == variant)?;
        let VariantPayload::Tuple(tys) = &v.payload else { return None };
        let TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } = static_ty(self.ws, *tys.get(i)?)?
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

    /// A widened payload position holding a nested FLAT enum, read as that enum's
    /// own cells from the parent's slots starting at `slot`. `None` when the
    /// position is widened for a value struct instead.
    ///
    /// The inner enum's OWN plan sizes the read, because that is the plan the
    /// ctor's leaf bundle writes with (`lower_flat_value` -> `repr_to_leaves_bundle`
    /// over the inner `Group`): `[__tag, _0.., <rec>..]`. `vs` is what the PARENT
    /// reserved for the position, so a parent that reserved less than the inner
    /// plan needs would read a neighbouring position's slot; that is a plan
    /// disagreement, not something to paper over.
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
        let leaves: Vec<Idx<Inst>> =
            (0..shape.leaf_count()).map(|k| self.scrut_slot(scrut, slot + k)).collect();
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
            Scrut::Cells { positional, .. } => positional[slot],
            Scrut::Flat { parent, prefix, .. } => {
                self.emit_cur(Inst::Field { base: *parent, field: flat_payload_slot(*prefix, slot) })
            }
            Scrut::Val(v) => self.emit_cur(Inst::TupleIndex { base: *v, index: slot }),
            Scrut::Tuple(_) => {
                let value = self.scrut_value(scrut);
                self.emit_cur(Inst::TupleIndex { base: value, index: slot })
            }
        }
    }

    /// The scrutinee's `i`th positional payload: threaded cells hold it directly; a
    /// flat field reads the parent's `{prefix}__i` slot; a cdata enum reads its
    /// `_<i>` C field; a table enum reads the positional `[i+1]` slot.
    pub(crate) fn scrut_payload(&mut self, scrut: &Scrut, i: usize, cdata: bool) -> Idx<Inst> {
        match scrut {
            Scrut::Cells { positional, .. } => positional[i],
            Scrut::Flat { parent, prefix, .. } => {
                self.emit_cur(Inst::Field { base: *parent, field: flat_payload_slot(*prefix, i) })
            }
            Scrut::Val(v) if cdata => {
                self.emit_cur(Inst::Field { base: *v, field: Ustr::from(format!("_{i}").as_str()) })
            }
            Scrut::Val(v) => self.emit_cur(Inst::TupleIndex { base: *v, index: i }),
            Scrut::Tuple(_) => {
                let value = self.scrut_value(scrut);
                self.emit_cur(Inst::TupleIndex { base: value, index: i })
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
            Scrut::Cells { named, shape, .. } => named
                .iter()
                .find(|(n, _)| *n == field)
                .map(|(_, v)| *v)
                .ok_or_else(|| gap(format!(
                    "record field `{field}` is absent from flat enum `{}`",
                    shape.name
                ))),
            Scrut::Flat { parent, prefix, .. } => {
                Ok(self.emit_cur(Inst::Field { base: *parent, field: flat_named_slot(*prefix, field) }))
            }
            Scrut::Val(v) => {
                let f = if cdata { slot_name(field.as_str()) } else { field };
                Ok(self.emit_cur(Inst::Field { base: *v, field: f }))
            }
            Scrut::Tuple(_) => {
                let value = self.scrut_value(scrut);
                Ok(self.emit_cur(Inst::Field { base: value, field }))
            }
        }
    }

    /// Tag test -> continue on match, else `fail`. Five reprs: a payload-free enum
    /// IS its ordinal int (compare ints); a cdata enum compares `.__tag` (int); a
    /// table enum compares `.__variant` (string); a FLATTENED field compares its
    /// `{prefix}__tag` slot; threaded CELLS compare the held tag (both strings).
    pub(crate) fn test_tag(&mut self, scrut: &Scrut, variant: Ustr, enum_name: Option<Ustr>, fail: Idx<Block>) {
        let flat_tag = match scrut {
            Scrut::Flat { parent, prefix, .. } => {
                Some(self.emit_cur(Inst::Field { base: *parent, field: flat_tag_slot(*prefix) }))
            }
            Scrut::Cells { tag, .. } => Some(*tag),
            Scrut::Val(_) | Scrut::Tuple(_) => None,
        };
        if let Some(tag) = flat_tag {
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
            let tag = self.emit_cur(Inst::Field { base: val, field: Ustr::from("__tag") });
            let want = self.emit_cur(Inst::Const(Lit::Int(ord)));
            let cond = self.emit_cur(Inst::Eq(tag, want));
            self.branch_or_fail(cond, fail);
            return;
        }
        let tag = self.emit_cur(Inst::Field {
            base: val,
            field: Ustr::from("__variant"),
        });
        let want = self.emit_cur(Inst::Const(Lit::Str(variant)));
        let cond = self.emit_cur(Inst::Eq(tag, want));
        self.branch_or_fail(cond, fail);
    }

    /// `cond` true -> a fresh continuation block (`self.cur` advances), false ->
    /// `fail`.
    pub(crate) fn branch_or_fail(&mut self, cond: Idx<Inst>, fail: Idx<Block>) {
        let cont = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond,
                then_bb: cont,
                else_bb: fail,
            },
        );
        self.cur = cont;
    }

    pub(crate) fn lower_if_let(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
        result_shape: Option<RetSlot>,
    ) -> Result<ValueRepr, CompilerError> {
        self.lower_if_let_inner(pattern, value, then_branch, else_branch, result_shape, false)?
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
        result_shape: Option<RetSlot>,
        statement: bool,
    ) -> Result<Option<ValueRepr>, CompilerError> {
        enum IfLetScrutinee {
            Generic,
            Nobox {
                binding: Idx<BindingInfo>,
                values: Vec<Idx<Inst>>,
                shape: ValueShape,
                then_bb: Idx<Block>,
            },
            Tuple {
                pattern: Idx<Pattern>,
                slots: Vec<Idx<Inst>>,
                shape: TupleRetShape,
                then_bb: Idx<Block>,
            },
        }

        let use_repr = result_shape.is_some();
        let shape = result_shape.unwrap_or(RetSlot::Opaque);
        let result_count = if statement {
            0
        } else if use_repr {
            shape.slot_count()
        } else {
            1
        };
        let result_ty = if statement || use_repr {
            None
        } else {
            self.current_expr_unit_storage_ty_id()
        };
        let nobox_binding = match self.body.pattern_arena[pattern] {
            Pattern::Bind(binding) if self.expr_is_nullable(value) => {
                self.nobox_binding_shape(binding)?
                    .or(self.nullable_value_shape_of(value)?.map(|mut shape| {
                        shape.nullable = false;
                        shape
                    }))
                    .map(|shape| (binding, shape))
            }
            _ => None,
        };
        let tuple_shape = if nobox_binding.is_none() && self.pattern_is_names_only(pattern) {
            self.nullable_tuple_wire_shape(value)?
        } else {
            None
        };

        let (scrutinee, results, else_bb, join) = if let Some((binding, binding_shape)) = nobox_binding
        {
            let mut scrut_shape = binding_shape.clone();
            scrut_shape.nullable = true;
            let bundle = self.lower_expr_repr(value)?;
            let leaves = self
                .repr_to_slot_values_bundle(&bundle, &RetSlot::Leaves(scrut_shape))?;
            let (present, values) = leaves
                .split_first()
                .expect("a nullable shape carries its presence leaf first");
            let results: Vec<Idx<Cell>> = (0..result_count)
                .map(|_| {
                    if use_repr {
                        self.f.new_cell(None)
                    } else {
                        self.f.new_cell_typed(None, result_ty)
                    }
                })
                .collect();
            let then_bb = self.f.new_block();
            let else_bb = self.f.new_block();
            let join = self.f.new_block();
            self.f.set_term(
                self.cur,
                Term::Branch { cond: *present, then_bb, else_bb },
            );
            (
                IfLetScrutinee::Nobox {
                    binding,
                    values: values.to_vec(),
                    shape: binding_shape,
                    then_bb,
                },
                results,
                else_bb,
                join,
            )
        } else if let Some(tuple_shape) = tuple_shape {
            let results: Vec<Idx<Cell>> = (0..result_count)
                .map(|_| {
                    if use_repr {
                        self.f.new_cell(None)
                    } else {
                        self.f.new_cell_typed(None, result_ty)
                    }
                })
                .collect();
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
            let bound_shape = TupleRetShape {
                slots: tuple_shape.slots,
                nullable: false,
            };
            let slots = self.tuple_repr_to_slots(
                ValueRepr::Tuple { parts, present: None },
                &bound_shape,
            )?;
            let then_bb = self.f.new_block();
            let else_bb = self.f.new_block();
            let join = self.f.new_block();
            self.f.set_term(
                self.cur,
                Term::Branch { cond: present, then_bb, else_bb },
            );
            (
                IfLetScrutinee::Tuple {
                    pattern,
                    slots,
                    shape: bound_shape,
                    then_bb,
                },
                results,
                else_bb,
                join,
            )
        } else {
            let results: Vec<Idx<Cell>> = (0..result_count)
                .map(|_| {
                    if use_repr {
                        self.f.new_cell(None)
                    } else {
                        self.f.new_cell_typed(None, result_ty)
                    }
                })
                .collect();
            let scrut = self.lower_scrut(value)?;
            let else_bb = self.f.new_block();
            let join = self.f.new_block();
            if self.expr_is_nullable(value) {
                let val_v = self.scrut_value(&scrut);
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let is_nil = self.emit_cur(Inst::Eq(val_v, nil));
                let cont = self.f.new_block();
                self.f.set_term(
                    self.cur,
                    Term::Branch { cond: is_nil, then_bb: else_bb, else_bb: cont },
                );
                self.cur = cont;
            }
            let variants = self.scrut_variants(value);
            let scrut_enum = self.scrut_enum_name(value);
            self.match_pattern(&scrut, pattern, else_bb, &variants, true, scrut_enum)?;
            (IfLetScrutinee::Generic, results, else_bb, join)
        };

        match scrutinee {
            IfLetScrutinee::Nobox {
                binding,
                values,
                shape,
                then_bb,
            } => {
                self.cur = then_bb;
                self.bind_leaves(binding, &values, shape)?;
            }
            IfLetScrutinee::Tuple {
                pattern,
                slots,
                shape,
                then_bb,
            } => {
                self.cur = then_bb;
                let mut idx = 0;
                self.bind_names_slot(pattern, &RetSlot::Nested(shape), &slots, &mut idx)?;
            }
            IfLetScrutinee::Generic => {}
        }

        if statement {
            self.lower_stmt_expr(then_branch)?;
            if self.is_open() {
                self.f.set_term(self.cur, Term::Goto(join));
            }
            self.cur = else_bb;
            if let Some(else_branch) = else_branch {
                self.lower_stmt_expr(else_branch)?;
            }
            if self.is_open() {
                self.f.set_term(self.cur, Term::Goto(join));
            }
            self.cur = join;
            Ok(None)
        } else if use_repr {
            let tv = self.lower_expr_repr(then_branch)?;
            self.store_bundle_and_join(&results, &shape, &tv, join)?;

            self.cur = else_bb;
            let ev = match else_branch {
                Some(eb) => self.lower_expr_repr(eb)?,
                None => ValueBundle::new(
                    tv.representation(),
                    ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))),
                ),
            };
            self.store_bundle_and_join(&results, &shape, &ev, join)?;

            self.cur = join;
            let slots: Vec<Idx<Inst>> =
                results.iter().map(|&cell| self.emit_cur(Inst::Load(cell))).collect();
            let mut idx = 0;
            Ok(Some(self.slot_repr(&shape, &slots, &mut idx)))
        } else {
            let result = results[0];
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

    /// The DECLARED nullable-tuple shape of a call, without lowering it.
    ///
    /// Read off the CALLEE's return type, never off `e`'s own type: safe-nav
    /// gives `h?.pair()` the same `Nullable(Tuple)` with no presence slot, so
    /// keying on the expression type would branch on a slot that is not there.
    pub(crate) fn nullable_tuple_call_shape(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<TupleRetShape>, CompilerError> {
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
        let Some(shape) = self.call_ret_tuple_shape(e)? else { return Ok(None) };
        Ok(shape.nullable.then_some(shape))
    }

    /// THE oracle for "a presence slot for this nullable tuple is really on the
    /// wire" -- what every consumer of one (`let`, `== null`, `if let`) reads,
    /// so none of them can branch on a slot another one did not put there.
    ///
    /// Each source carries its own warrant for answering, and none of them is
    /// the expression's own type:
    /// - a direct CALL answers from the callee's DECLARED return
    ///   ([`Self::nullable_tuple_call_shape`]);
    /// - a NAME answers from the shape its slot cells were bound with, which is
    ///   whatever this oracle said at the `let`;
    /// - a SAFE-NAV answers from its own join, which manufactures the presence
    ///   rather than receiving it -- the sanctioned exception `repr_join_slot`
    ///   already relies on, and the reason the shape is built here instead of
    ///   read off a declaration.
    pub(crate) fn nullable_tuple_wire_shape(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<TupleRetShape>, CompilerError> {
        if let Expr::Var(b) = self.body.expr_arena[e] {
            let Some((_, shape)) = self.tuple_binds.get(&raw(b)) else { return Ok(None) };
            return Ok(shape.nullable.then(|| shape.clone()));
        }
        if self.is_safe_nav(e) {
            let Some(inner) = self.peeled_tuple_shape(e)?.filter(TupleRetShape::splats) else {
                return Ok(None);
            };
            return Ok(Some(TupleRetShape { nullable: true, ..inner }));
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
        match self.body.pattern_arena[pat].clone() {
            Pattern::Bind(_) | Pattern::Wildcard => true,
            Pattern::Tuple { elements } => {
                elements.into_iter().all(|p| self.pattern_is_names_only(p))
            }
            _ => false,
        }
    }

    /// [`Self::bind_pattern_slot`] driven by a `Pattern` instead of a
    /// `BindingPattern`, for the `if let` forms. Only the shapes the pattern arena
    /// can spell and the binding arena cannot are handled here -- a HOLE, which
    /// has no `BindingInfo` to name -- and everything else delegates, so the two
    /// cannot drift about what a slot is.
    ///
    /// A hole still CONSUMES its slots: the values are positional, so binding
    /// nothing without advancing would hand the next element this one's leaves.
    fn bind_names_slot(
        &mut self,
        pat: Idx<Pattern>,
        slot: &RetSlot,
        values: &[Idx<Inst>],
        idx: &mut usize,
    ) -> Result<(), CompilerError> {
        match self.body.pattern_arena[pat].clone() {
            Pattern::Wildcard => *idx += slot.slot_count(),
            Pattern::Bind(b) => {
                self.bind_pattern_slot(&BindingPattern::Leaf(b), slot, values, idx)?;
            }
            Pattern::Tuple { elements } => {
                let RetSlot::Nested(inner) = slot else {
                    return Err(gap("a tuple pattern against a non-tuple wire slot"));
                };
                if elements.len() != inner.slots.len() {
                    return Err(gap("a tuple pattern of a different arity than its wire slot"));
                }
                for (p, s) in elements.into_iter().zip(inner.slots.clone()) {
                    self.bind_names_slot(p, &s, values, idx)?;
                }
            }
            _ => return Err(gap("bind_names_slot on a pattern that needs a test")),
        }
        Ok(())
    }

    /// `while let pat = value { body }`: re-evaluate `value` each iteration in
    /// the header, match it, run the body on success, exit on failure.
    pub(crate) fn lower_while_let(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let header = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let variants = self.scrut_variants(value);
        let scrut_enum = self.scrut_enum_name(value);
        let scrut = self.lower_scrut(value)?;
        // A nullable scrutinee (`while let v = some_opt`) exits the loop on nil
        // (the runtime value of a `T?` is the unwrapped value or nil) — mirrors
        // `if let`. Without this, a plain Bind always "matches" and the loop runs
        // with `v = nil`, then a use of `v` faults. (A flat-field scrutinee is
        // never nullable, so `scrut_value` is a plain `Val` here.)
        if self.expr_is_nullable(value) {
            let val_v = self.scrut_value(&scrut);
            let nil = self.emit_cur(Inst::Const(Lit::Nil));
            let is_nil = self.emit_cur(Inst::Eq(val_v, nil));
            let cont = self.f.new_block();
            self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: exit, else_bb: cont });
            self.cur = cont;
        }
        // Match: failure exits the loop; on success `self.cur` holds the bindings.
        self.match_pattern(&scrut, pattern, exit, &variants, true, scrut_enum)?;
        self.loop_targets.push((header, exit, self.defers.len()));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(header));
        }

        self.cur = exit;
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
            Ustr::from(""),
        );
        self.binds.insert(raw(var_b), i_cell);
        self.f.mark_loop_binding(i_cell);
        let lo_v = self.lower_expr(low)?;
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: lo_v,
        });
        let hi_v = self.lower_expr(high)?;
        let hi_cell = self.f.new_cell(None);
        self.emit_cur(Inst::Store {
            cell: hi_cell,
            value: hi_v,
        });

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
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: cmp,
                then_bb: body_bb,
                else_bb: exit,
            },
        );

        self.cur = body_bb;
        self.loop_targets.push((latch, exit, self.defers.len())); // continue -> latch (runs the step)
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(latch));
        }

        self.cur = latch;
        let i_v2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(if is_uint { Lit::Uint(1) } else { Lit::Int(1) }));
        let inc = if is_uint {
            self.emit_cur(Inst::UintAdd(i_v2, one))
        } else {
            self.emit_cur(Inst::IntAdd(i_v2, one))
        };
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: inc,
        });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }
}
