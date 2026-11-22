use super::*;

impl<'a> Builder<'a> {
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
            if let StructFieldPlan::Inline {
                child,
                nullable,
                flat: None,
            } = plan
            {
                return Ok(StructFieldPlan::Direct {
                    child: Some(child),
                    nullable,
                });
            }
        }
        Ok(plan)
    }

    /// Embedded children use dotted keys on the parent.
    pub(crate) fn physical_field(
        &self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<(Idx<Expr>, Ustr), CompilerError> {
        let (base, key, _, _) = self.physical_field_layout(base, field)?;
        Ok((base, key))
    }

    /// A flattened value's projected shape comes from the leaf layout that stores
    /// it, even when an intermediate child permits boxing.
    pub(crate) fn physical_field_layout(
        &self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<(Idx<Expr>, Ustr, Option<ValueShape>, bool), CompilerError> {
        if field.as_str() == "len"
            && matches!(
                self.concrete_expr_ty(base).map(|ty| ty.peel_nullable().0),
                Some(MonoTy::App {
                    ctor: MonoCtor::Array,
                    ..
                })
            )
        {
            return Ok((base, self.vec_length_field(base)?, None, false));
        }
        let mut parts = vec![field];
        let mut cur = base;
        let mut physical = (base, field, None, false);
        while let Expr::FieldAccess {
            base: owner,
            field: child_field,
        } = self.body.expr_arena[cur]
        {
            if self.swizzle_of(owner, child_field).is_some() {
                break;
            }
            let Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(sname),
                ..
            }) = self.field_base_ty(owner)
            else {
                break;
            };
            if self.scx.target == bone_core::hir::FnBackend::Lua
                && !self.scx.values_first_class
                && self.is_value_struct(sname)
                && self.is_mutable_place(owner)?
                && self.native_projection_holder(owner)
                && self
                    .scx
                    .rep_arena()
                    .nominal_value_storage_required(self.ws, self.scx.target, sname)
                    .map_err(|e| self.gap_here(e.message))?
                    == ValueStorage::CData
            {
                let path = std::iter::once(child_field.as_str())
                    .chain(parts.iter().rev().map(|part| part.as_str()))
                    .collect::<Vec<_>>()
                    .join(".");
                let paths = self
                    .scx
                    .rep_arena()
                    .nominal_value_leaf_paths_required(self.ws, self.scx.target, sname)
                    .map_err(|e| self.gap_here(e.message))?;
                if paths.iter().any(|leaf| leaf.as_str() == path) {
                    physical = (owner, cdata_leaf_alias(self.ws, sname, &path), None, true);
                }
            }
            let plan = self.struct_field_plan(sname, child_field, false)?;
            let child = match plan.inline_child() {
                Some(child) => Some(child),
                None => self.leaf_table_value_child(sname, child_field)?,
            };
            if let Some(child) = child {
                let shape = self.nobox_embed_group_shape(child)?;
                let paths = self.embed_leaf_paths(child)?;
                let relative = parts
                    .iter()
                    .rev()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                let prefix = format!("{relative}.");
                if paths
                    .iter()
                    .any(|path| path.as_str() == relative || path.as_str().starts_with(&prefix))
                {
                    let path: Vec<_> = parts.iter().rev().copied().collect();
                    let projected = shape.and_then(|shape| {
                        match self.value_projection_range(&shape, &path)?.1 {
                            FieldShape::Nested(sub) => Some(sub),
                            FieldShape::Prim => None,
                        }
                    });
                    physical = (
                        owner,
                        crate::names::ustr(&format!("{child_field}.{relative}")),
                        projected,
                        false,
                    );
                }
            }
            parts.push(child_field);
            cur = owner;
        }
        Ok(physical)
    }

    // Groups, tuple projections and container views have no boxed native holder.
    fn native_projection_holder(&self, mut base: Idx<Expr>) -> bool {
        loop {
            match self.body.expr_arena[base] {
                Expr::FieldAccess { base: owner, .. } => base = owner,
                Expr::Var(binding) => {
                    return !self.group_binds.contains_key(&raw(binding))
                        && !self.tuple_binds.contains_key(&raw(binding));
                }
                _ => return false,
            }
        }
    }

    pub(crate) fn leaf_table_base(&self, base: Idx<Expr>) -> Result<Option<Ustr>, CompilerError> {
        if self.scx.values_first_class {
            return Ok(None);
        }
        let Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(sname),
            ..
        }) = self.field_base_ty(base)
        else {
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

    /// A nested value may occupy leaves even under a Direct field plan.
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

    /// The child's leaves go to the parent's dotted keys under `key`, in the order
    /// the parent's `<Name>_lct` ctor keys them.
    pub(crate) fn store_leaf_table_child(
        &mut self,
        parent: Idx<Inst>,
        sname: Ustr,
        field: Ustr,
        key: Ustr,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let shape = self.nominal_rep_shape_required(sname, false)?;
        let Some((_, FieldShape::Nested(sub))) = shape.fields.iter().find(|(f, _)| *f == field)
        else {
            return Err(self.gap_here(format!(
                "leaf-table value `{sname}` has no nested field `{field}`"
            )));
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
            self.write_field(parent, crate::names::ustr(&k), v);
        }
        Ok(())
    }

    pub(crate) fn materialize_embed(
        &mut self,
        parent: Idx<Inst>,
        prefix: Ustr,
        cname: Ustr,
    ) -> Result<Idx<Inst>, CompilerError> {
        let mut fields = Vec::new();
        self.collect_embed_slots(parent, prefix, "", cname, &mut fields)?;
        self.materialize_embed_fields(cname, fields)
    }

    pub(crate) fn materialize_embed_fields(
        &mut self,
        cname: Ustr,
        fields: Vec<(Ustr, Idx<Inst>)>,
    ) -> Result<Idx<Inst>, CompilerError> {
        if self.is_value_struct(cname) {
            let representation = self.nominal_value_representation(cname)?;
            Ok(self.emit_cur(Inst::MakeValueStruct {
                name: cname,
                representation,
                form: ValueCtorForm::Declared,
                fields,
            }))
        } else {
            Ok(self.emit_cur(Inst::MakeStruct {
                ty: Some(cname),
                fields,
            }))
        }
    }

    pub(crate) fn collect_embed_slots(
        &mut self,
        parent: Idx<Inst>,
        prefix: Ustr,
        rel: &str,
        cname: Ustr,
        out: &mut Vec<(Ustr, Idx<Inst>)>,
    ) -> Result<(), CompilerError> {
        for f in self.struct_field_names_required(cname)? {
            let key = crate::names::ustr(&format!("{prefix}.{f}"));
            let relkey = format!("{rel}{f}");
            let field_plan = self.struct_field_plan(cname, f, false)?;
            if let Some((inner, nullable)) = field_plan.embed() {
                if nullable {
                    let v = self.read_field(parent, key);
                    out.push((crate::names::ustr(relkey.as_str()), v));
                }
                self.collect_embed_slots(parent, key, &format!("{relkey}."), inner, out)?;
            } else if let Some(shape) = field_plan.flat_shape() {
                for slot in flat_slot_names(f, &shape) {
                    let src = crate::names::ustr(&format!("{prefix}.{slot}"));
                    let v = self.read_field(parent, src);
                    out.push((crate::names::ustr(&format!("{rel}{slot}")), v));
                }
            } else {
                let v = self.read_field(parent, key);
                out.push((crate::names::ustr(relkey.as_str()), v));
            }
        }
        Ok(())
    }

    pub(crate) fn nobox_embed_group_shape(
        &self,
        cname: Ustr,
    ) -> Result<Option<ValueShape>, CompilerError> {
        if !self.is_nobox_struct(cname) {
            return Ok(None);
        }
        self.nominal_rep_shape(cname, true)
    }

    pub(crate) fn embed_group(
        &mut self,
        parent: Idx<Inst>,
        prefix: Ustr,
        shape: &ValueShape,
    ) -> ValueRepr {
        let leaves = super::shape::leaf_names(shape, prefix.as_str(), ".")
            .into_iter()
            .map(|key| self.read_field(parent, crate::names::ustr(&key)))
            .collect::<Vec<_>>();
        Self::rebuild_from_leaves(shape, &leaves)
    }

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
            let present = self.read_field(parent, prefix);
            if let ValueRepr::Group { present: slot, .. } = &mut repr {
                *slot = Some(present);
            }
        }
        Ok(repr)
    }

    pub(crate) fn embed_leaf_values(
        &mut self,
        cname: Ustr,
        value: Idx<Expr>,
    ) -> Result<Vec<(Ustr, Idx<Inst>)>, CompilerError> {
        if let Some(shape) = self.nobox_embed_group_shape(cname)? {
            let paths = value_leaf_paths(&shape);
            let repr = self.lower_expr_repr(value)?;
            let leaves = self.repr_to_storage_leaves(&repr, &shape, "embedded value")?;
            return Self::pair_embed_leaves(paths, leaves);
        }
        if let Expr::StructLiteral {
            name,
            fields,
            bases,
        } = self.body.expr_arena[value]
        {
            // A functional-update base reads unwritten fields off another instance; the
            // general path owns that base-copy rule.
            if name == cname
                && bases.into_iter().next().is_none()
                && self.sibling_defaults(name).is_none()
            {
                // A field group (`Ty { a + b: expr }`) shares one value expr: lower each
                // distinct one once so it is evaluated once.
                let mut by_value: HashMap<u32, Idx<Inst>> = HashMap::default();
                let mut flat_by_value: HashMap<u32, FlatCells> = HashMap::default();
                let mut written: Vec<(Ustr, Idx<Expr>)> = Vec::new();
                for fidx in fields {
                    let lf = self.body.struct_lit_field_arena[fidx];
                    written.push((lf.name, lf.value));
                }
                let mut out = Vec::new();
                for f in self.struct_field_names_required(cname)? {
                    let inner = self.struct_field_plan(cname, f, true)?.inline_child();
                    // A flat-enum field contributes its sub-slots, never one boxed leaf.
                    let flat = if inner.is_none() {
                        self.struct_field_plan(cname, f, false)?.flat_shape()
                    } else {
                        None
                    };
                    match written.iter().find(|(n, _)| *n == f).map(|(_, v)| *v) {
                        Some(ve) => match (inner, flat) {
                            // A nullable inner embed owns a presence slot, and `= null` must not be
                            // read leaf-wise off nil.
                            (Some(ic), _) => {
                                let n = self.struct_field_plan(cname, f, true)?.nullable();
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
                        None => {
                            let d = self.lower_struct_default(cname, f)?;
                            match (inner, flat) {
                                (Some(ic), _) => {
                                    out.extend(prefixed(f, self.embed_leaf_reads(ic, d)?))
                                }
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
        let v = self.lower_expr(value)?;
        self.embed_leaf_reads(cname, v)
    }

    pub(crate) fn embed_leaf_paths(&self, cname: Ustr) -> Result<Vec<Ustr>, CompilerError> {
        if let Some(shape) = self.nobox_embed_group_shape(cname)? {
            return Ok(value_leaf_paths(&shape));
        }
        let mut out = Vec::new();
        for f in self.struct_field_names_required(cname)? {
            match self.struct_field_plan(cname, f, true)?.inline_child() {
                Some(inner) => out.extend(
                    self.embed_leaf_paths(inner)?
                        .into_iter()
                        .map(|k| crate::names::ustr(&format!("{f}.{k}"))),
                ),
                None => match self.struct_field_plan(cname, f, false)?.flat_shape() {
                    Some(shape) => out.extend(flat_slot_names(f, &shape)),
                    None => out.push(f),
                },
            }
        }
        Ok(out)
    }

    /// Presence is published last so errors or yields cannot expose a partial value.
    pub(crate) fn embed_nullable_leaf_values(
        &mut self,
        cname: Ustr,
        value: Idx<Expr>,
    ) -> Result<(Idx<Inst>, Vec<(Ustr, Idx<Inst>)>), CompilerError> {
        let paths = self.embed_leaf_paths(cname)?;
        // `= null` clears presence and every leaf: a stale reference leaf would keep
        // its pointee alive.
        if matches!(self.body.expr_arena[value], Expr::Null) {
            let nil = self.emit_cur(Inst::Const(Lit::Nil));
            return Ok((nil, paths.into_iter().map(|k| (k, nil)).collect()));
        }
        if matches!(self.body.expr_arena[value], Expr::StructLiteral { name, .. } if name == cname)
        {
            let leaves = self.embed_leaf_values(cname, value)?;
            let present = self.emit_cur(Inst::Const(Lit::Bool(true)));
            return Ok((present, leaves));
        }
        if let Some(mut shape) = self.nobox_embed_group_shape(cname)? {
            let paths = value_leaf_paths(&shape);
            shape.nullable = true;
            let bundle = self.lower_expr_repr(value)?;
            let leaves =
                self.repr_to_slot_values_bundle(&bundle, &RepLayout::Leaves(shape.clone()))?;
            let mut leaves = leaves.into_iter();
            let present = leaves
                .next()
                .ok_or_else(|| gap("nullable embed shape produced no presence leaf"))?;
            return Ok((present, Self::pair_embed_leaves(paths, leaves.collect())?));
        }
        // Dynamic `Child?`: leaves are read only on the present arm.
        let v = self.lower_expr(value)?;
        let cells = self.fresh_cells(paths.len());
        let absent_bb = self.f.new_block();
        let present_bb = self.f.new_block();
        let join = self.f.new_block();
        let is_nil = self.branch_on_nil(v, absent_bb, present_bb);

        self.cur = absent_bb;
        for &c in &cells {
            let n = self.emit_cur(Inst::Const(Lit::Nil));
            self.store_cell(c, n);
        }
        self.goto_if_open(join);

        self.cur = present_bb;
        for (&c, (_, lv)) in cells.iter().zip(self.embed_leaf_reads(cname, v)?) {
            self.store_cell(c, lv);
        }
        self.goto_if_open(join);

        self.cur = join;
        let leaves = paths
            .into_iter()
            .zip(cells)
            .map(|(k, c)| (k, self.emit_cur(Inst::Load(c))))
            .collect();
        let present = self.emit_cur(Inst::Not(is_nil));
        Ok((present, leaves))
    }

    pub(crate) fn embed_leaf_reads(
        &mut self,
        cname: Ustr,
        value: Idx<Inst>,
    ) -> Result<Vec<(Ustr, Idx<Inst>)>, CompilerError> {
        self.mark_record_owner(value, Some(cname));
        let mut out = Vec::new();
        for f in self.struct_field_names_required(cname)? {
            if self
                .struct_field_plan(cname, f, true)?
                .inline_child()
                .is_none()
            {
                if let Some(shape) = self.struct_field_plan(cname, f, false)?.flat_shape() {
                    for slot in flat_slot_names(f, &shape) {
                        let v = self.read_field(value, slot);
                        out.push((slot, v));
                    }
                    continue;
                }
            }
            let part = self.read_field(value, f);
            match self.struct_field_plan(cname, f, true)?.inline_child() {
                Some(inner) => out.extend(prefixed(f, self.embed_leaf_reads(inner, part)?)),
                None => out.push((f, part)),
            }
        }
        Ok(out)
    }

    pub(crate) fn embed_read_is_nullable(
        &self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<bool, CompilerError> {
        match self.field_base_ty(base) {
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(sname),
                ..
            }) => Ok(self.struct_field_plan(sname, field, false)?.nullable()),
            _ => Ok(false),
        }
    }

    /// `(presence, relative leaves)`. Relative so one lowering can be attached under
    /// several field names when a field group shares one value expr.
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

    pub(crate) fn store_embed(
        &mut self,
        parent: Idx<Inst>,
        field: Ustr,
        cname: Ustr,
        nullable: bool,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        // LuaJIT inserts missing hash keys even for nil stores. Re-clearing an
        // absent embed can therefore rehash the parent on every frame. Host
        // record stores remain observable even when the slot is absent.
        let skip_clear = if nullable
            && matches!(self.body.expr_arena[value], Expr::Null)
            && !self.callback_records.contains_key(&parent)
        {
            // False presence still needs normalization for boxed embed readers.
            let present = self.read_field(parent, field);
            let clear = self.f.new_block();
            let join = self.f.new_block();
            self.branch_on_nil(present, join, clear);
            self.cur = clear;
            Some(join)
        } else {
            None
        };
        let leaves = self.embed_value_leaves(nullable, cname, value)?;
        for (key, v) in Self::embed_field_slots(field, leaves) {
            self.write_field(parent, key, v);
        }
        if let Some(join) = skip_clear {
            self.f.set_term(self.cur, Term::Goto(join));
            self.cur = join;
        }
        Ok(())
    }

    /// Presence-gated, so an absent field reads as null and never materializes a
    /// box from stale leaves.
    pub(crate) fn materialize_embed_nullable(
        &mut self,
        parent: Idx<Inst>,
        field: Ustr,
        cname: Ustr,
    ) -> Result<Idx<Inst>, CompilerError> {
        let present = self.read_field(parent, field);
        let is_nil = self.is_nil(present);
        self.nil_or_join(is_nil, false, |s| s.materialize_embed(parent, field, cname))
    }

    pub(crate) fn pair_embed_leaves(
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
