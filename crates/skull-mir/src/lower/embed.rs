//! `embed`ded and flattened struct fields: reading a physical slot through its
//! dotted parent key, the nullable-embed read/store pair, and the group/leaf
//! plumbing that materializes one.
use super::*;

impl Builder<'_> {
    pub(crate) fn struct_field_plan(
        &self,
        sname: Ustr,
        field: Ustr,
        spliced: bool,
    ) -> Result<StructFieldPlan, CompilerError> {
        let plan = crate::lower::struct_field_plan(self.ws, self.scx.target, sname, field, spliced)
            .map_err(|e| self.gap_here(e.message))?;
        // First-class targets store inline children as declared fields, not leaf keys.
        if self.scx.values_first_class {
            if let StructFieldPlan::Inline { child, nullable, flat: None } = plan {
                return Ok(StructFieldPlan::Direct { child: Some(child), nullable });
            }
        }
        Ok(plan)
    }

    // --- `embed` storage: a child's leaves ARE the parent's physical keys ---
    //
    // Key encoding is the dotted path shared with flattened enum fields (see
    // `flat_tag_slot`): `embed a: X` with leaf `b` lives at `parent["a.b"]`,
    // recursively. `.` is not an identifier character, so the encoding cannot
    // collide with a user field or a control key, and it is injective over
    // paths. Enforcement of the source-level rules (projection-only, site
    // legality) is bone's half; this is storage only.

    /// Physical key for `base.field`, composing an `embed` chain into one dotted
    /// path and returning the expression that actually HOLDS it. `t.a.b.c` with
    /// `a` and `b` embedded is the single key `"a.b.c"` on `t` -- no intermediate
    /// projection exists at any level, because no intermediate box does.
    ///
    /// Links are matched under the SPLICED rule and the innermost one is then
    /// validated under the storage rule, because only the innermost link decides
    /// whether a table is reached at all. An FFI-clean value struct one level in
    /// (`o.m` at `struct O { m: Mid }`) splices its own nested leaves into `o`, so
    /// `o.m.v.y` is the single key `"m.v.y"` -- while the same projection off a
    /// standalone `Mid` local is real cdata and composes nothing. Matching every
    /// link under the storage rule stopped at the first FFI-clean link and read a
    /// `"m.v"` key nothing writes; matching every link under the spliced rule
    /// would compose a dotted key on that cdata local.
    ///
    /// A plain value child of a leaf-table value links like an inline child:
    /// that parent keys all its nested leaves by dotted path.
    pub(crate) fn physical_field(
        &self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<(Idx<Expr>, Ustr), CompilerError> {
        let mut parts = vec![field];
        let mut cur = base;
        let mut innermost = None;
        while let Expr::FieldAccess { base: b2, field: f2 } = self.body.expr_arena[cur] {
            if self.swizzle_of(b2, f2).is_some() {
                break;
            }
            let Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. }) = self.field_base_ty(b2) else {
                break;
            };
            if self
                .struct_field_plan(sname, f2, true)?
                .inline_child()
                .is_none()
                && self.leaf_table_value_child(sname, f2)?.is_none()
            {
                break;
            }
            parts.push(f2);
            innermost = Some((sname, f2));
            cur = b2;
        }
        if let Some((s, f)) = innermost
            && self
                .struct_field_plan(s, f, false)?
                .inline_child()
                .is_none()
            && self.leaf_table_value_child(s, f)?.is_none()
        {
            return Ok((base, field));
        }
        if innermost.is_none() {
            return Ok((base, field));
        }
        if parts.len() == 1 {
            return Ok((cur, field));
        }
        parts.reverse();
        let key: Vec<&str> = parts.iter().map(|p| p.as_str()).collect();
        Ok((cur, Ustr::from(&key.join("."))))
    }

    /// The leaf-table value `base` projects a field out of, seen through `ref`.
    pub(crate) fn leaf_table_base(&self, base: Idx<Expr>) -> Result<Option<Ustr>, CompilerError> {
        if self.scx.values_first_class {
            return Ok(None);
        }
        let Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. }) = self.field_base_ty(base) else {
            return Ok(None);
        };
        if !self.is_value_struct(sname) {
            return Ok(None);
        }
        let storage = self
            .scx
            .rep_arena()
            .nominal_value_storage_required(self.ws, self.scx.target, sname)
            .map_err(|e| self.gap_here(e.message))?;
        Ok((storage == ValueStorage::LeafTable).then_some(sname))
    }

    /// The plain (not `nobox`) value child `field` of leaf-table value `sname`.
    /// Its field plan is `Direct`, yet the parent keys every nested leaf by
    /// dotted path, so the child has no key of its own: `pos` of a leaf-table
    /// parent lives at `pos.x`/`pos.y`, exactly like an inline child.
    pub(crate) fn leaf_table_value_child(
        &self,
        sname: Ustr,
        field: Ustr,
    ) -> Result<Option<Ustr>, CompilerError> {
        if self.scx.values_first_class || !self.is_value_struct(sname) {
            return Ok(None);
        }
        let Some(child) = self.struct_field_plan(sname, field, false)?.direct_child() else {
            return Ok(None);
        };
        if !self.is_value_struct(child) {
            return Ok(None);
        }
        let storage = self
            .scx
            .rep_arena()
            .nominal_value_storage_required(self.ws, self.scx.target, sname)
            .map_err(|e| self.gap_here(e.message))?;
        Ok((storage == ValueStorage::LeafTable).then_some(child))
    }

    /// Write a whole [`Self::leaf_table_value_child`] `parent.{key} = value`:
    /// the child's leaves go to the parent's dotted keys under `key`, in the
    /// order the parent's `<Name>_lct` ctor keys them.
    pub(crate) fn store_leaf_table_child(
        &mut self,
        parent: Idx<Inst>,
        sname: Ustr,
        field: Ustr,
        key: Ustr,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let shape = self.nominal_rep_shape_required(sname, false)?;
        let Some((_, FieldShape::Nested(sub))) = shape.fields.iter().find(|(f, _)| *f == field) else {
            return Err(self.gap_here(format!("leaf-table value `{sname}` has no nested field `{field}`")));
        };
        let repr = self.lower_expr_repr(value)?;
        let leaves = self.repr_to_leaves_bundle_for_shape(&repr, sub)?;
        let keys = super::shape::leaf_names(sub, key.as_str(), ".");
        if keys.len() != leaves.len() {
            return Err(self.gap_here(format!(
                "`{sname}.{field}` has {} leaf keys but the value produced {} leaves",
                keys.len(),
                leaves.len()
            )));
        }
        for (k, v) in keys.into_iter().zip(leaves) {
            self.emit_cur(Inst::StoreField { base: parent, field: Ustr::from(&k), value: v });
        }
        Ok(())
    }

    /// Re-materialize an embedded field `parent.{prefix}` from its physical
    /// leaves, at a whole-value-read boundary (a scalar-ABI call argument, `==`,
    /// interpolation). Projection-only guarantees the result is a TRANSIENT --
    /// it can never be bound, returned, captured or stored -- so it stays
    /// sinkable rather than becoming the storage the feature removed.
    pub(crate) fn materialize_embed(
        &mut self,
        parent: Idx<Inst>,
        prefix: Ustr,
        cname: Ustr,
    ) -> Result<Idx<Inst>, CompilerError> {
        let mut fields = Vec::new();
        self.collect_embed_slots(parent, prefix, "", cname, &mut fields)?;
        if self.is_value_struct(cname) {
            let representation = self.nominal_value_representation(cname)?;
            Ok(self.emit_cur(Inst::MakeValueStruct {
                name: cname,
                representation,
                form: ValueCtorForm::Declared,
                fields,
            }))
        } else {
            Ok(self.emit_cur(Inst::MakeStruct { ty: Some(cname), fields }))
        }
    }

    /// The rebuilt child's slots: for each of `cname`'s fields, the key it holds
    /// on the CHILD paired with the read of the key it lives at on `parent`.
    /// `rel` is the child-relative prefix, so this is the exact inverse of the
    /// store side's keying ([`Builder::embed_field_slots`]: relative leaves under
    /// the field name, presence tag at the field name itself).
    ///
    /// An embed field stays FLAT here, for the reason the flat-enum arm already
    /// gives: a `Mid` whose `inner` is embedded is canonically
    /// `{["inner.v"]=3, inner=true, m=9}`, so re-boxing `inner` under its own name
    /// hands consumers a shape they never read. Recursing into a nested box (which
    /// is what this did) miscompiled BOTH directions across a call boundary -- an
    /// absent inner came back non-nil because the box always had one, and a
    /// PRESENT inner's leaf came back nil because the callee reads `["inner.v"]`
    /// and the box stored a table at `inner`.
    ///
    /// The presence tag needs no branch: an absent field's leaves and tag are all
    /// nil on the parent, and nil at those keys IS the canonical absent form (the
    /// ctor emits `["inner.v"] = nil, inner = nil`), so an unconditional read
    /// reproduces it.
    pub(crate) fn collect_embed_slots(
        &mut self,
        parent: Idx<Inst>,
        prefix: Ustr,
        rel: &str,
        cname: Ustr,
        out: &mut Vec<(Ustr, Idx<Inst>)>,
    ) -> Result<(), CompilerError> {
        for f in self.struct_field_names_required(cname)? {
            let key = Ustr::from(&format!("{prefix}.{f}"));
            let relkey = format!("{rel}{f}");
            let field_plan = self.struct_field_plan(cname, f, false)?;
            if let Some((inner, nullable)) = field_plan.embed() {
                if nullable {
                        let v = self.emit_cur(Inst::Field { base: parent, field: key });
                        out.push((Ustr::from(relkey.as_str()), v));
                }
                self.collect_embed_slots(parent, key, &format!("{relkey}."), inner, out)?;
            } else if let Some(shape) = field_plan.flat_shape() {
                    for slot in flat_slot_names(f, &shape) {
                        let src = Ustr::from(&format!("{prefix}.{slot}"));
                        let v = self.emit_cur(Inst::Field { base: parent, field: src });
                        out.push((Ustr::from(&format!("{rel}{slot}")), v));
                    }
            } else {
                    let v = self.emit_cur(Inst::Field { base: parent, field: key });
                    out.push((Ustr::from(relkey.as_str()), v));
            }
        }
        Ok(())
    }

    /// The shape to read a `nobox` embed child back as, or `None` to fall
    /// through to [`Builder::materialize_embed`] (and, for a marked child, to
    /// the guarantee's rejection).
    ///
    /// `None` for an unmarked child -- its transient box is legal and sinks.
    /// Also `None` when any field in the tree is a FLAT enum: those occupy
    /// several parent slots under [`flat_slot_names`], which the shape carries
    /// as one entry, so a Group built over them would hand the sink a leaf
    /// sequence that lines up wrong. Rejecting loudly beats a silent
    /// misalignment; the shape has no in-tree instance today.
    pub(crate) fn nobox_embed_group_shape(
        &self,
        cname: Ustr,
    ) -> Result<Option<ValueShape>, CompilerError> {
        if !self.is_nobox_struct(cname) || self.struct_has_flat_field(cname)? {
            return Ok(None);
        }
        self.nominal_rep_shape(cname, true)
    }

    /// Whether any field of `cname`, at any depth, flattens to several parent
    /// slots rather than one.
    pub(crate) fn struct_has_flat_field(&self, cname: Ustr) -> Result<bool, CompilerError> {
        for f in self.struct_field_names_required(cname)? {
            if self
                .struct_field_plan(cname, f, false)?
                .flat_shape()
                .is_some()
            {
                return Ok(true);
            }
            if let Some(inner) = self
                .struct_field_plan(cname, f, true)?
                .inline_child()
                && inner != cname
                && self.struct_has_flat_field(inner)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A whole read of a `nobox` embed field, as an unboxed Group over the
    /// parent's dotted leaves. Driven by the SHAPE rather than by the field
    /// list, so the parts line up with [`Builder::repr_to_leaves`]' order by
    /// construction instead of by two walks agreeing.
    pub(crate) fn embed_group(&mut self, parent: Idx<Inst>, prefix: Ustr, shape: &ValueShape) -> ValueRepr {
        let mut parts = Vec::new();
        for (fname, fs) in &shape.fields {
            let key = Ustr::from(&format!("{prefix}.{fname}"));
            parts.push(match fs {
                FieldShape::Prim => {
                    ValueRepr::Scalar(self.emit_cur(Inst::Field { base: parent, field: key }))
                }
                FieldShape::Nested(sub) => self.embed_group(parent, key, sub),
            });
        }
        ValueRepr::Group { name: shape.name, parts, present: None }
    }

    /// Read one physically embedded child without rediscovering its representation.
    pub(crate) fn load_embed_repr(
        &mut self,
        parent: Idx<Inst>,
        prefix: Ustr,
        cname: Ustr,
        nullable: bool,
    ) -> Result<ValueRepr, CompilerError> {
        let Some(shape) = self.nobox_embed_group_shape(cname)? else {
            let value = if nullable {
                self.materialize_embed_nullable(parent, prefix, cname)?
            } else {
                self.materialize_embed(parent, prefix, cname)?
            };
            return Ok(ValueRepr::Scalar(value));
        };
        let mut repr = self.embed_group(parent, prefix, &shape);
        if nullable {
            let present = self.emit_cur(Inst::Field { base: parent, field: prefix });
            if let ValueRepr::Group { present: slot, .. } = &mut repr {
                *slot = Some(present);
            }
        }
        Ok(repr)
    }

    /// The `(physical key, value)` pairs for an embedded field's leaves, the one
    /// source for both a parent literal's splice and a whole-field write.
    ///
    /// A DIRECT child literal FUSES: its field expressions lower straight into
    /// the parent's leaf slots, so the child box is never built at all. Any
    /// other expression is lowered once and read leaf-wise -- correct, and a
    /// transient the later passes can still sink. Mirrors `lower_flat_value`,
    /// which fuses a direct `E::V(..)` ctor the same way.
    ///
    /// All leaves are evaluated BEFORE any caller stores them, so a whole-field
    /// write is not observable half-applied.
    pub(crate) fn embed_leaf_values(
        &mut self,
        cname: Ustr,
        value: Idx<Expr>,
    ) -> Result<Vec<(Ustr, Idx<Inst>)>, CompilerError> {
        if let Expr::StructLiteral { name, fields, bases } = self.body.expr_arena[value].clone() {
            // A functional-update base reads unwritten fields off another
            // instance; leave that to the general path rather than duplicating
            // the base-copy rule here.
            if name == cname && bases.into_iter().next().is_none() {
                // A field GROUP (`Ty { a + b: expr }`) shares ONE value expr
                // across several fields: lower each distinct one once, so the
                // evaluation count is right and grouped leaves share it.
                let mut by_value: HashMap<u32, Idx<Inst>> = HashMap::new();
                let mut flat_by_value: HashMap<u32, FlatCells> = HashMap::new();
                let mut written: Vec<(Ustr, Idx<Expr>)> = Vec::new();
                for fidx in fields {
                    let lf = self.body.struct_lit_field_arena[fidx];
                    written.push((lf.name, lf.value));
                }
                let mut out = Vec::new();
                for f in self.struct_field_names_required(cname)? {
                    let inner = self
                        .struct_field_plan(cname, f, true)?
                        .inline_child();
                    // A flat-enum field contributes its sub-slots, never one
                    // boxed leaf -- same expansion the non-embed struct literal
                    // does via `flat_literal_slots`.
                    let flat = if inner.is_none() {
                        self
                            .struct_field_plan(cname, f, false)?
                            .flat_shape()
                    } else {
                        None
                    };
                    match written.iter().find(|(n, _)| *n == f).map(|(_, v)| *v) {
                        Some(ve) => match (inner, flat) {
                            // Through the nullable-aware pair, not `embed_leaf_values`
                            // directly: a nullable inner embed owns a presence slot,
                            // and `= null` must not be read leaf-wise off nil.
                            (Some(ic), _) => {
                                let n = self
                                    .struct_field_plan(cname, f, true)?
                                    .nullable();
                                let leaves = self.embed_value_leaves(n, ic, ve)?;
                                out.extend(Self::embed_field_slots(f, leaves));
                            }
                            (None, Some(shape)) => {
                                let k: u32 = ve.into_raw().into();
                                let cells = match flat_by_value.get(&k) {
                                    Some(c) => c.clone(),
                                    None => {
                                        let c = self.lower_flat_value(&shape, ve)?;
                                        flat_by_value.insert(k, c.clone());
                                        c
                                    }
                                };
                                out.extend(self.flat_literal_slots(f, cells));
                            }
                            (None, None) => {
                                let k: u32 = ve.into_raw().into();
                                let v = match by_value.get(&k) {
                                    Some(&v) => v,
                                    None => {
                                        let v = self.lower_expr(ve)?;
                                        by_value.insert(k, v);
                                        v
                                    }
                                };
                                out.push((f, v));
                            }
                        },
                        // Unwritten: the child's own field default.
                        None => {
                            let d = self.lower_struct_default(cname, f)?;
                            match (inner, flat) {
                                (Some(ic), _) => out.extend(prefixed(f, self.embed_leaf_reads(ic, d)?)),
                                (None, Some(shape)) => {
                                    let cells = self.spread_maybe_null_box_to_cells(d, &shape)?;
                                    out.extend(self.flat_literal_slots(f, cells));
                                }
                                (None, None) => out.push((f, d)),
                            }
                        }
                    }
                }
                return Ok(out);
            }
        }
        if let Some(shape) = self.nobox_embed_group_shape(cname)? {
            let paths = value_leaf_paths(&shape);
            let repr = self.lower_expr_repr(value)?;
            let leaves = self.repr_to_storage_leaves(&repr, &shape, "embedded value")?;
            return Self::pair_embed_leaves(paths, leaves);
        }
        let v = self.lower_expr(value)?;
        Ok(self.embed_leaf_reads(cname, v)?)
    }

    /// Every relative leaf path of embedded child `cname`, declaration order.
    pub(crate) fn embed_leaf_paths(&self, cname: Ustr) -> Result<Vec<Ustr>, CompilerError> {
        let mut out = Vec::new();
        for f in self.struct_field_names_required(cname)? {
            match self
                .struct_field_plan(cname, f, true)?
                .inline_child()
            {
                Some(inner) => out.extend(
                    self.embed_leaf_paths(inner)?
                        .into_iter()
                        .map(|k| Ustr::from(&format!("{f}.{k}"))),
                ),
                None => match self
                    .struct_field_plan(cname, f, false)?
                    .flat_shape()
                {
                    Some(shape) => out.extend(flat_slot_names(f, &shape)),
                    None => out.push(f),
                },
            }
        }
        Ok(out)
    }

    /// `(presence, leaves)` for a NULLABLE embed field. Callers publish presence
    /// LAST, so an error or yield mid-assignment leaves the field ABSENT rather
    /// than present-and-half-built.
    ///
    /// A statically-known null or a direct child literal needs no branch; any
    /// other expression is tested once at runtime, because reading leaves off an
    /// absent child would index nil.
    pub(crate) fn embed_nullable_leaf_values(
        &mut self,
        cname: Ustr,
        value: Idx<Expr>,
    ) -> Result<(Idx<Inst>, Vec<(Ustr, Idx<Inst>)>), CompilerError> {
        let paths = self.embed_leaf_paths(cname)?;
        // `= null`: clear presence AND every leaf. Scalar staleness is
        // unobservable under presence gating, but a stale REFERENCE leaf keeps
        // its pointee alive -- a leak, not a cosmetic -- so clear uniformly.
        if matches!(self.body.expr_arena[value], Expr::Null) {
            let nil = self.emit_cur(Inst::Const(Lit::Nil));
            return Ok((nil, paths.into_iter().map(|k| (k, nil)).collect()));
        }
        if matches!(self.body.expr_arena[value], Expr::StructLiteral { name, .. } if name == cname) {
            let leaves = self.embed_leaf_values(cname, value)?;
            let present = self.emit_cur(Inst::Const(Lit::Bool(true)));
            return Ok((present, leaves));
        }
        // A `nobox` child already threads as presence-plus-leaves, so take the
        // repr rather than a boxed value: no box to test, and no branch either,
        // because the repr carries its own presence and its leaves are nil on
        // the absent path by construction.
        if let Some(mut shape) = self.nobox_embed_group_shape(cname)? {
            let paths = value_leaf_paths(&shape);
            shape.nullable = true;
            let bundle = self.lower_expr_repr(value)?;
            let leaves = self.repr_to_slot_values_bundle(&bundle, &RetSlot::Leaves(shape.clone()))?;
            let mut leaves = leaves.into_iter();
            let present = leaves
                .next()
                .ok_or_else(|| gap("nullable embed shape produced no presence leaf"))?;
            return Ok((present, Self::pair_embed_leaves(paths, leaves.collect())?));
        }
        // Dynamic `Child?`: one presence test, leaves read only on the present
        // arm and joined through cells.
        let v = self.lower_expr(value)?;
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(v, nil));
        let cells: Vec<Idx<Cell>> = paths.iter().map(|_| self.f.new_cell(None)).collect();
        let absent_bb = self.f.new_block();
        let present_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: absent_bb, else_bb: present_bb });

        self.cur = absent_bb;
        for &c in &cells {
            let n = self.emit_cur(Inst::Const(Lit::Nil));
            self.emit_cur(Inst::Store { cell: c, value: n });
        }
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(join));
        }

        self.cur = present_bb;
        for (&c, (_, lv)) in cells.iter().zip(self.embed_leaf_reads(cname, v)?) {
            self.emit_cur(Inst::Store { cell: c, value: lv });
        }
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(join));
        }

        self.cur = join;
        let leaves = paths
            .into_iter()
            .zip(cells)
            .map(|(k, c)| (k, self.emit_cur(Inst::Load(c))))
            .collect();
        let present = self.emit_cur(Inst::Not(is_nil));
        Ok((present, leaves))
    }

    /// The same relative-path pairs read off an already-boxed child.
    pub(crate) fn embed_leaf_reads(
        &mut self,
        cname: Ustr,
        value: Idx<Inst>,
    ) -> Result<Vec<(Ustr, Idx<Inst>)>, CompilerError> {
        let mut out = Vec::new();
        for f in self.struct_field_names_required(cname)? {
            // A flat-enum field is read out sub-slot-wise; emitting the whole
            // field read first would be a dead load of a key nothing writes.
            if self
                .struct_field_plan(cname, f, true)?
                .inline_child()
                .is_none()
            {
                if let Some(shape) = self
                    .struct_field_plan(cname, f, false)?
                    .flat_shape()
                {
                    for slot in flat_slot_names(f, &shape) {
                        let v = self.emit_cur(Inst::Field { base: value, field: slot });
                        out.push((slot, v));
                    }
                    continue;
                }
            }
            let part = self.emit_cur(Inst::Field { base: value, field: f });
            match self
                .struct_field_plan(cname, f, true)?
                .inline_child()
            {
                Some(inner) => out.extend(prefixed(f, self.embed_leaf_reads(inner, part)?)),
                None => out.push((f, part)),
            }
        }
        Ok(out)
    }

    /// Whether the embed field `base.field` is declared nullable.
    pub(crate) fn embed_read_is_nullable(
        &self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<bool, CompilerError> {
        match self.field_base_ty(base) {
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. }) => {
                Ok(self
                    .struct_field_plan(sname, field, false)?
                    .nullable())
            }
            _ => Ok(false),
        }
    }

    /// `(presence, relative leaves)` for setting an embed field to `value`.
    /// Relative so one lowering can be attached under several field names (a
    /// field group shares one value expr); [`embed_field_slots`] re-keys it.
    pub(crate) fn embed_value_leaves(
        &mut self,
        nullable: bool,
        cname: Ustr,
        value: Idx<Expr>,
    ) -> Result<(Option<Idx<Inst>>, Vec<(Ustr, Idx<Inst>)>), CompilerError> {
        if nullable {
            let (present, leaves) = self.embed_nullable_leaf_values(cname, value)?;
            return Ok((Some(present), leaves));
        }
        Ok((None, self.embed_leaf_values(cname, value)?))
    }

    /// Absolute `(physical key, value)` pairs for embed field `field`: the
    /// child's leaves under the dotted path, plus the presence slot LAST when
    /// the field is nullable. The one source for a parent literal's splice and
    /// for a whole-field write, so the two cannot drift.
    pub(crate) fn embed_field_slots(
        field: Ustr,
        (present, leaves): (Option<Idx<Inst>>, Vec<(Ustr, Idx<Inst>)>),
    ) -> Vec<(Ustr, Idx<Inst>)> {
        let mut out = prefixed(field, leaves);
        if let Some(p) = present {
            out.push((field, p));
        }
        out
    }

    /// Write a whole embedded field `parent.{field} = value` leaf-wise.
    pub(crate) fn store_embed(
        &mut self,
        parent: Idx<Inst>,
        base: Idx<Expr>,
        field: Ustr,
        cname: Ustr,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let nullable = self.embed_read_is_nullable(base, field)?;
        let leaves = self.embed_value_leaves(nullable, cname, value)?;
        for (key, v) in Self::embed_field_slots(field, leaves) {
            self.emit_cur(Inst::StoreField { base: parent, field: key, value: v });
        }
        Ok(())
    }

    /// A whole read of a NULLABLE embed field: presence-gated, so an absent
    /// field reads as null and never materializes a box from stale leaves.
    pub(crate) fn materialize_embed_nullable(
        &mut self,
        parent: Idx<Inst>,
        field: Ustr,
        cname: Ustr,
    ) -> Result<Idx<Inst>, CompilerError> {
        let present = self.emit_cur(Inst::Field { base: parent, field });
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(present, nil));
        let result = self.f.new_cell(None);
        let nil_bb = self.f.new_block();
        let val_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: val_bb });

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(Lit::Nil));
        self.store_and_join(result, n, join);

        self.cur = val_bb;
        let boxed = self.materialize_embed(parent, field, cname)?;
        self.store_and_join(result, boxed, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    fn pair_embed_leaves(
        paths: Vec<Ustr>,
        leaves: Vec<Idx<Inst>>,
    ) -> Result<Vec<(Ustr, Idx<Inst>)>, CompilerError> {
        if paths.len() != leaves.len() {
            return Err(gap(format!(
                "embed shape has {} physical paths but lowering produced {} leaves",
                paths.len(),
                leaves.len()
            )));
        }
        Ok(paths.into_iter().zip(leaves).collect())
    }
}
