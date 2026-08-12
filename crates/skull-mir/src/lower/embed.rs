//! `embed`ded and flattened struct fields: reading a physical slot through its
//! dotted parent key, the nullable-embed read/store pair, and the group/leaf
//! plumbing that materializes one.
use super::*;

impl Builder<'_> {
    pub(crate) fn flat_field_of(&self, base: Idx<Expr>, field: Ustr) -> Option<EnumFlatShape> {
        let TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. } = self.expr_ty(base)? else {
            return None;
        };
        if self.is_value_struct(sname) {
            return None;
        }
        let sid = *self.ws.struct_by_name.get(&sname)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        let fdef = sd.fields.iter().find(|f| f.name == field)?;
        let TypeExpr::App { ctor: TypeCtor::Nominal(ename), .. } = static_ty(self.ws, fdef.ty)? else {
            return None;
        };
        EnumPlan::for_name(self.ws, ename).and_then(|plan| plan.flat().cloned())
    }

    /// Same as [`Self::flat_field_of`] but for a known struct name + field (used at a
    /// struct literal, where the literal's nominal type is the struct being built).
    pub(crate) fn flat_field_of_struct(&self, sname: Ustr, field: Ustr) -> Option<EnumFlatShape> {
        if self.is_value_struct(sname) {
            return None;
        }
        let sid = *self.ws.struct_by_name.get(&sname)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        let fdef = sd.fields.iter().find(|f| f.name == field)?;
        let TypeExpr::App { ctor: TypeCtor::Nominal(ename), .. } = static_ty(self.ws, fdef.ty)? else {
            return None;
        };
        EnumPlan::for_name(self.ws, ename).and_then(|plan| plan.flat().cloned())
    }

    // --- `embed` storage: a child's leaves ARE the parent's physical keys ---
    //
    // Key encoding is the dotted path shared with flattened enum fields (see
    // `flat_tag_slot`): `embed a: X` with leaf `b` lives at `parent["a.b"]`,
    // recursively. `.` is not an identifier character, so the encoding cannot
    // collide with a user field or a control key, and it is injective over
    // paths. Enforcement of the source-level rules (projection-only, site
    // legality) is bone's half; this is storage only.

    /// The embedded child's struct name for `sname.field`, when `field` is
    /// declared `embed` on a REFERENCE parent. An FFI-clean `value` parent is
    /// already contiguous cdata, so `embed` there is a no-op GUARANTEE (it
    /// asserts the layout) and must not re-encode anything -- but a LEAF-TABLE
    /// value parent stores exactly like a reference parent does (dotted leaf
    /// keys, presence at the field's own name), so it takes the same path.
    pub(crate) fn embed_field_of_struct(&self, sname: Ustr, field: Ustr) -> Option<Ustr> {
        self.embed_field_of_struct_inner(sname, field, false)
    }

    /// [`Self::embed_field_of_struct`] for a child whose leaves are ALREADY being
    /// spliced into an enclosing table.
    ///
    /// The contiguous-cdata bail above answers "how does `sname` store this field
    /// inside its own layout" -- the right question where storage is being
    /// decided, and the wrong one inside the three splice walks: those run only
    /// after a caller settled that `sname`'s leaves go into the ENCLOSING table,
    /// so there is no cdata for a nested value field to sit inside. Asking the
    /// storage question there sent an FFI-clean nested `nobox` down the one-boxed
    /// -leaf arm, which is the box the mark forbids.
    pub(crate) fn embed_field_of_spliced(&self, sname: Ustr, field: Ustr) -> Option<Ustr> {
        self.embed_field_of_struct_inner(sname, field, true)
    }

    fn embed_field_of_struct_inner(&self, sname: Ustr, field: Ustr, spliced: bool) -> Option<Ustr> {
        // This runs on EVERY field access, so bail in O(1) when the workspace
        // declares no embed field at all. The flag is set while registering
        // fields and never cleared: a stale `true` costs one bool check, a
        // stale `false` would silently skip the flattening.
        if !(self.ws.has_embed_fields || self.ws.has_nobox_types) {
            return None;
        }
        if !spliced
            && self.is_value_struct(sname)
            && !matches!(
                value_storage_for_nominal(self.ws, self.scx.target, sname),
                Some(ValueStorage::LeafTable)
            )
        {
            return None;
        }
        let sid = *self.ws.struct_by_name.get(&sname)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        let fdef = sd.fields.iter().find(|f| f.name == field)?;
        // `resolve_ty` peels `Nullable`, which is what a nullable embed needs:
        // presence is a separate leaf, the child's own leaves are unchanged.
        let TypeExpr::App { ctor: TypeCtor::Nominal(cname), .. } = self.resolve_ty(fdef.ty)? else {
            return None;
        };
        let child = self
            .ws
            .struct_by_name
            .get(&cname)
            .map(|sid| &self.ws.modules[sid.module.index()].structs[sid.def]);
        if !fdef.is_embed && !child.is_some_and(|s| s.is_nobox) {
            return None;
        }
        Some(cname)
    }

    /// Whether an embed field is declared nullable. Presence is its own LEAF --
    /// it lives at the field's own declared name, which flattening leaves free
    /// (every physical key is dotted), so no extra encoding is needed.
    pub(crate) fn embed_field_is_nullable(&self, sname: Ustr, field: Ustr) -> bool {
        let Some(sid) = self.ws.struct_by_name.get(&sname) else { return false };
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        sd.fields
            .iter()
            .find(|f| f.name == field)
            .is_some_and(|f| matches!(self.ty_node(f.ty), Some(TypeExpr::Nullable(_))))
    }

    pub(crate) fn embed_field_of(&self, base: Idx<Expr>, field: Ustr) -> Option<Ustr> {
        let TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. } = self.expr_ty(base)? else {
            return None;
        };
        self.embed_field_of_struct(sname, field)
    }

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
    pub(crate) fn physical_field(&self, base: Idx<Expr>, field: Ustr) -> (Idx<Expr>, Ustr) {
        let mut parts = vec![field];
        let mut cur = base;
        let mut innermost = None;
        while let Expr::FieldAccess { base: b2, field: f2 } = self.body.expr_arena[cur] {
            let Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. }) = self.expr_ty(b2) else {
                break;
            };
            if self.embed_field_of_spliced(sname, f2).is_none() {
                break;
            }
            parts.push(f2);
            innermost = Some((sname, f2));
            cur = b2;
        }
        if !innermost.is_some_and(|(s, f)| self.embed_field_of_struct(s, f).is_some()) {
            return (base, field);
        }
        if parts.len() == 1 {
            return (cur, field);
        }
        parts.reverse();
        let key: Vec<&str> = parts.iter().map(|p| p.as_str()).collect();
        (cur, Ustr::from(&key.join(".")))
    }

    /// Re-materialize an embedded field `parent.{prefix}` from its physical
    /// leaves, at a whole-value-read boundary (a scalar-ABI call argument, `==`,
    /// interpolation). Projection-only guarantees the result is a TRANSIENT --
    /// it can never be bound, returned, captured or stored -- so it stays
    /// sinkable rather than becoming the storage the feature removed.
    pub(crate) fn materialize_embed(&mut self, parent: Idx<Inst>, prefix: Ustr, cname: Ustr) -> Idx<Inst> {
        let mut fields = Vec::new();
        self.collect_embed_slots(parent, prefix, "", cname, &mut fields);
        if self.is_value_struct(cname) {
            self.emit_cur(Inst::MakeValueStruct { name: cname, fields })
        } else {
            self.emit_cur(Inst::MakeStruct { ty: Some(cname), fields })
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
    ) {
        for f in self.struct_field_names(cname).unwrap_or_default() {
            let key = Ustr::from(&format!("{prefix}.{f}"));
            let relkey = format!("{rel}{f}");
            match self.embed_field_of_struct(cname, f) {
                Some(inner) => {
                    if self.embed_field_is_nullable(cname, f) {
                        let v = self.emit_cur(Inst::Field { base: parent, field: key });
                        out.push((Ustr::from(relkey.as_str()), v));
                    }
                    self.collect_embed_slots(parent, key, &format!("{relkey}."), inner, out);
                }
                None => match self.flat_field_of_struct(cname, f) {
                    Some(shape) => {
                        for slot in flat_slot_names(f, &shape) {
                            let src = Ustr::from(&format!("{prefix}.{slot}"));
                            let v = self.emit_cur(Inst::Field { base: parent, field: src });
                            out.push((Ustr::from(&format!("{rel}{slot}")), v));
                        }
                    }
                    None => {
                        let v = self.emit_cur(Inst::Field { base: parent, field: key });
                        out.push((Ustr::from(relkey.as_str()), v));
                    }
                },
            }
        }
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
    pub(crate) fn nobox_embed_group_shape(&self, cname: Ustr) -> Option<ValueShape> {
        if !self.is_nobox_struct(cname) || self.struct_has_flat_field(cname) {
            return None;
        }
        self.nominal_rep_shape(cname, true)
    }

    /// Whether any field of `cname`, at any depth, flattens to several parent
    /// slots rather than one.
    pub(crate) fn struct_has_flat_field(&self, cname: Ustr) -> bool {
        self.struct_field_names(cname).unwrap_or_default().iter().any(|&f| {
            self.flat_field_of_struct(cname, f).is_some()
                || self.embed_field_of_struct(cname, f).is_some_and(|inner| {
                    inner != cname && self.struct_has_flat_field(inner)
                })
        })
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
                for f in self.struct_field_names(cname).unwrap_or_default() {
                    let inner = self.embed_field_of_spliced(cname, f);
                    // A flat-enum field contributes its sub-slots, never one
                    // boxed leaf -- same expansion the non-embed struct literal
                    // does via `flat_literal_slots`.
                    let flat = if inner.is_none() { self.flat_field_of_struct(cname, f) } else { None };
                    match written.iter().find(|(n, _)| *n == f).map(|(_, v)| *v) {
                        Some(ve) => match (inner, flat) {
                            // Through the nullable-aware pair, not `embed_leaf_values`
                            // directly: a nullable inner embed owns a presence slot,
                            // and `= null` must not be read leaf-wise off nil.
                            (Some(ic), _) => {
                                let n = self.embed_field_is_nullable(cname, f);
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
                                (Some(ic), _) => out.extend(prefixed(f, self.embed_leaf_reads(ic, d))),
                                (None, Some(shape)) => {
                                    let cells = self.spread_box_to_cells(d, &shape);
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
        let v = self.lower_expr(value)?;
        Ok(self.embed_leaf_reads(cname, v))
    }

    /// Every relative leaf path of embedded child `cname`, declaration order.
    pub(crate) fn embed_leaf_paths(&self, cname: Ustr) -> Vec<Ustr> {
        let mut out = Vec::new();
        for f in self.struct_field_names(cname).unwrap_or_default() {
            match self.embed_field_of_spliced(cname, f) {
                Some(inner) => out.extend(
                    self.embed_leaf_paths(inner)
                        .into_iter()
                        .map(|k| Ustr::from(&format!("{f}.{k}"))),
                ),
                None => match self.flat_field_of_struct(cname, f) {
                    Some(shape) => out.extend(flat_slot_names(f, &shape)),
                    None => out.push(f),
                },
            }
        }
        out
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
        let paths = self.embed_leaf_paths(cname);
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
        if let Some(mut shape) = self.nobox_embed_group_shape(cname) {
            shape.nullable = true;
            let r = self.lower_expr_repr(value)?;
            let leaves = self.repr_to_leaves(&r, &shape);
            let (present, rest) = leaves.split_first().expect("a nullable shape leads with presence");
            debug_assert_eq!(paths.len(), rest.len(), "leaf path/value counts must agree");
            return Ok((*present, paths.into_iter().zip(rest.iter().copied()).collect()));
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
        for (&c, (_, lv)) in cells.iter().zip(self.embed_leaf_reads(cname, v)) {
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
    pub(crate) fn embed_leaf_reads(&mut self, cname: Ustr, value: Idx<Inst>) -> Vec<(Ustr, Idx<Inst>)> {
        let mut out = Vec::new();
        for f in self.struct_field_names(cname).unwrap_or_default() {
            // A flat-enum field is read out sub-slot-wise; emitting the whole
            // field read first would be a dead load of a key nothing writes.
            if self.embed_field_of_spliced(cname, f).is_none() {
                if let Some(shape) = self.flat_field_of_struct(cname, f) {
                    for slot in flat_slot_names(f, &shape) {
                        let v = self.emit_cur(Inst::Field { base: value, field: slot });
                        out.push((slot, v));
                    }
                    continue;
                }
            }
            let part = self.emit_cur(Inst::Field { base: value, field: f });
            match self.embed_field_of_spliced(cname, f) {
                Some(inner) => out.extend(prefixed(f, self.embed_leaf_reads(inner, part))),
                None => out.push((f, part)),
            }
        }
        out
    }

    /// Whether the embed field `base.field` is declared nullable.
    pub(crate) fn embed_read_is_nullable(&self, base: Idx<Expr>, field: Ustr) -> bool {
        match self.expr_ty(base) {
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. }) => {
                self.embed_field_is_nullable(sname, field)
            }
            _ => false,
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
        let nullable = self.embed_read_is_nullable(base, field);
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
    ) -> Idx<Inst> {
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
        let boxed = self.materialize_embed(parent, field, cname);
        self.store_and_join(result, boxed, join);

        self.cur = join;
        self.emit_cur(Inst::Load(result))
    }
}
