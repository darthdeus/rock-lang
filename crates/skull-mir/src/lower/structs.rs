//! Struct and enum CONSTRUCTION: struct literals and their defaults, the
//! value-struct predicates the walk consults, enum-variant packing, swizzles,
//! and the compile-time `@struct_expand` / `@shader_expand` expansions.
use super::*;
use crate::monoty::MonoTy;

impl Builder<'_> {
    pub(crate) fn lower_exprs(
        &mut self,
        range: comfy_la_arena::IdxRange<Expr>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        range.into_iter().map(|e| self.lower_expr(e)).collect()
    }

    /// A struct literal -> `MakeStruct`, fields in DECLARED order. Every declared
    /// field must be present; an omitted (defaulted) field gaps for now —
    /// injecting its default means lowering the `StructDefaults` body, a later
    /// step. Emitting an incomplete struct would be a silent miscompile.
    pub(crate) fn lower_struct_literal(
        &mut self,
        e: Idx<Expr>,
        name: Ustr,
        fields: comfy_la_arena::IdxRange<bone_semantic::hir::StructLitField>,
        bases: comfy_la_arena::IdxRange<bone_semantic::hir::Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        // Functional-update base (`Struct { ..base, .. }`): evaluate once, before
        // fields (source order), and read unwritten fields off it. Inference
        // guarantees at most one base and that it has this struct's type.
        let base_inst: Option<Idx<Inst>> = match bases.into_iter().next() {
            Some(b) => Some(self.lower_expr(b)?),
            None => None,
        };
        // A field GROUP (`Struct { a + b + c: expr }`) expands to several fields
        // sharing ONE `value` Expr index; evaluate each distinct value-expr once
        // (correct eval count, and grouped fields share the same instance), in
        // source order. A flattened enum field evaluates to (tag, payload slots)
        // instead of one boxed value (`docs/enum-allocation-report.md`).
        let mut lit_box: Vec<(Ustr, Idx<Inst>)> = Vec::new();
        let mut lit_flat: Vec<(Ustr, FlatCells)> = Vec::new();
        let mut lit_embed: Vec<(Ustr, Vec<(Ustr, Idx<Inst>)>)> = Vec::new();
        let mut by_value: HashMap<u32, Idx<Inst>> = HashMap::new();
        let mut flat_by_value: HashMap<u32, FlatCells> = HashMap::new();
        let mut embed_by_value: HashMap<u32, (Option<Idx<Inst>>, Vec<(Ustr, Idx<Inst>)>)> =
            HashMap::new();
        for fidx in fields {
            let lf = self.body.struct_lit_field_arena[fidx];
            let key: u32 = lf.value.into_raw().into();
            let field_plan = self.struct_field_plan(name, lf.name, false)?;
            if let Some(shape) = field_plan.flat_shape() {
                let cells = match flat_by_value.get(&key) {
                    Some(c) => c.clone(),
                    None => {
                        let c = self.lower_flat_value(&shape, lf.value)?;
                        flat_by_value.insert(key, c.clone());
                        c
                    }
                };
                lit_flat.push((lf.name, cells));
            } else if let Some((cname, nullable)) = field_plan.embed() {
                // An embedded field's leaves ARE this parent's keys, so its
                // value is spliced rather than lowered to a child box. Keyed by
                // the value expr so a field GROUP still evaluates once.
                let leaves = match embed_by_value.get(&key) {
                    Some(l) => l.clone(),
                    None => {
                        let l = self.embed_value_leaves(nullable, cname, lf.value)?;
                        embed_by_value.insert(key, l.clone());
                        l
                    }
                };
                lit_embed.push((lf.name, Self::embed_field_slots(lf.name, leaves)));
            } else {
                let v = match by_value.get(&key) {
                    Some(&v) => v,
                    None => {
                        let expected = self.concrete_struct_field_ty(e, lf.name);
                        let v = self.lower_expr_with_expected(lf.value, expected.as_deref())?;
                        by_value.insert(key, v);
                        v
                    }
                };
                lit_box.push((lf.name, v));
            }
        }
        let out = match self.struct_field_names(name) {
            Some(decl) => {
                let mut fs = Vec::new();
                for fname in decl {
                    if let Some((_, cells)) = lit_flat.iter().find(|(n, _)| *n == fname) {
                        fs.extend(self.flat_literal_slots(fname, cells.clone()));
                    } else if let Some((_, slots)) = lit_embed.iter().find(|(n, _)| *n == fname) {
                        fs.extend(slots.clone());
                    } else if let Some(&(_, v)) = lit_box.iter().find(|(n, _)| *n == fname) {
                        fs.push((fname, v));
                    } else if let Some((cname, nullable)) = self
                        .struct_field_plan(name, fname, false)?
                        .embed()
                    {
                        // Unwritten embed field: copy the base's leaves through
                        // (including its presence slot), else the struct default.
                        match base_inst {
                            Some(bi) => {
                                for k in self.embed_leaf_paths(cname)? {
                                    let key = Ustr::from(&format!("{fname}.{k}"));
                                    let v = self.emit_cur(Inst::Field { base: bi, field: key });
                                    fs.push((key, v));
                                }
                                if nullable {
                                    let p = self.emit_cur(Inst::Field { base: bi, field: fname });
                                    fs.push((fname, p));
                                }
                            }
                            None => {
                                let slots = self.with_struct_default(name, fname, |s, def| {
                                    s.embed_value_leaves(nullable, cname, def)
                                })?;
                                fs.extend(Self::embed_field_slots(fname, slots));
                            }
                        }
                    } else if let Some(shape) = self
                        .struct_field_plan(name, fname, false)?
                        .flat_shape()
                    {
                        // Unwritten flat field: from the base if present, else the
                        // struct default. A base stores the field in physical
                        // dotted slots, while the default is still a boxed enum.
                        let cells = match base_inst {
                            Some(bi) => self.load_flat_cells(bi, fname, &shape),
                            None => {
                                let dv = self.lower_struct_default(name, fname)?;
                                self.spread_maybe_null_box_to_cells(dv, &shape)?
                            }
                        };
                        fs.extend(self.flat_literal_slots(fname, cells));
                    } else {
                        let v = match base_inst {
                            Some(bi) => self.emit_cur(Inst::Field { base: bi, field: fname }),
                            None => self.lower_struct_default(name, fname)?,
                        };
                        fs.push((fname, v));
                    }
                }
                fs
            }
            None => lit_box, // unknown nominal (foreign module): emit as-given
        };
        // A hand-built `NativeBuffer { ptr: .., len: .. }` must carry the same
        // leaf views the `native_buffer_alloc` intrinsic attaches -- element
        // access decides on the TYPE, so a buffer of a leaf-addressed element
        // would otherwise index a nil view. (Loud, not silent: leaf addressing
        // never changes storage, so the only failure available is a missing
        // view, and that errors on first access.)
        let out = self.with_leaf_views(e, name, out)?;
        // Non-value structs only — a `value` literal is built via the repr layer
        // (`value_literal_repr` -> `pack`, the single value-cdata source), so it
        // never reaches here.
        Ok(self.emit_cur(Inst::MakeStruct {
            ty: Some(name),
            fields: out,
        }))
    }

    /// Append the leaf-view fields to a hand-written `NativeBuffer` literal's
    /// fields, keyed off its declared element type. A no-op for any other
    /// struct, for a primitive element, and for one already carrying views.
    pub(crate) fn with_leaf_views(
        &mut self,
        e: Idx<Expr>,
        name: Ustr,
        mut fields: Vec<(Ustr, Idx<Inst>)>,
    ) -> Result<Vec<(Ustr, Idx<Inst>)>, CompilerError> {
        if !bone_semantic::prelude::is_intrinsic_key(name, "NativeBuffer") {
            return Ok(fields);
        }
        // The element comes from the literal's own inferred type, the same
        // oracle element ACCESS consults -- so construction and access can never
        // disagree about whether a buffer is leaf-addressed.
        let Some(elem) = self.nb_value_elem(e)? else {
            return Ok(fields);
        };
        let Some(&(_, ptr)) = fields.iter().find(|(f, _)| f.as_str() == "ptr") else {
            return Ok(fields);
        };
        fields.extend(self.leaf_view_fields(elem, ptr));
        Ok(fields)
    }

    pub(crate) fn struct_field_names(&self, name: Ustr) -> Option<Vec<Ustr>> {
        let sid = *self.ws.struct_by_name.get(&name)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        Some(sd.fields.iter().map(|f| f.name).collect())
    }

    pub(crate) fn struct_field_names_required(
        &self,
        name: Ustr,
    ) -> Result<Vec<Ustr>, CompilerError> {
        self.struct_field_names(name)
            .ok_or_else(|| self.gap_here(format!("nominal `{name}` has no declared field plan")))
    }

    /// Whether nominal `name` is a `value` struct (C-layout cdata).
    /// Whether `e`'s type is a `value` struct -- the shape whose rvalue lowering
    /// packs a fresh cdata rather than handing back shared storage.
    pub(crate) fn expr_is_value_struct(&self, e: Idx<Expr>) -> bool {
        if let Some(ty) = self.concrete_expr_ty(e) {
            return matches!(
                ty.peel_nullable().0,
                MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name), ..
                } if self.is_value_struct(name.to_owned().into())
            );
        }
        false
    }

    pub(crate) fn is_value_struct(&self, name: Ustr) -> bool {
        self.ws
            .struct_by_name
            .get(&name)
            .map(|sid| self.ws.modules[sid.module.index()].structs[sid.def].is_value)
            .unwrap_or(false)
    }

    /// Whether `e` denotes a mutable PLACE -- storage a later write can reach
    /// through, so a whole-value read out of it must copy. A value local/param is
    /// NOT one (bone rejects leaf writes there), nor is a call result; without
    /// that exclusion every uniform-struct field read would rebuild its value.
    ///
    /// Sibling, NOT a duplicate: bone's `value_field_write_is_place`
    /// (`bone-inference/src/fields.rs`) decides whether a write TARGET is legal
    /// storage. This decides whether a later write can reach a value being READ,
    /// so it stops at the first container/global (`vlocal.buf[i]` is shared
    /// storage even though the chain starts at a value binding) where bone walks
    /// to the root. They must AGREE that a temporary is neither.
    pub(crate) fn is_mutable_place(&self, e: Idx<Expr>) -> Result<bool, CompilerError> {
        match &self.body.expr_arena[e] {
            Expr::Global(_) | Expr::Index { .. } => Ok(true),
            Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. } => {
                self.is_mutable_place(*base)
            }
            // A reference-struct binding aliases shared storage; a value binding
            // is a copy of its own.
            Expr::Var(b) => Ok(!self.binding_is_nonnull_value_struct(*b)),
            _ => Ok(false),
        }
    }

    /// The value-struct type of `e`, EITHER tier. `None` for a non-value type or
    /// unsolved inference, and for a `V?` -- an ABSENT value has no leaves, so an
    /// unconditional split would index nil. A nullable read that still needs the
    /// copy takes the presence-aware path instead
    /// ([`Self::nullable_place_value_of`]).
    pub(crate) fn value_struct_of(&self, e: Idx<Expr>) -> Option<Ustr> {
        let ty = self.concrete_expr_ty(e)?;
        let (payload, nullable) = ty.peel_nullable();
        if nullable {
            return None;
        }
        let name = payload.nominal()?;
        self.is_value_struct(name).then_some(name)
    }

    /// The value-struct type of `e` IGNORING nullability (`expr_ty` peels
    /// `Nullable`, so a `V?` resolves to `V` here).
    pub(crate) fn value_struct_name_of(&self, e: Idx<Expr>) -> Option<Ustr> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let name = ty.peel_nullable().0.nominal()?;
            return self.is_value_struct(name).then_some(name);
        }
        None
    }

    /// The value-struct type of a NULLABLE whole-value read out of a mutable place
    /// -- a field, a global, a container element. The copy is required exactly as
    /// it is for the non-nullable twin (`value_struct_of(e).filter(|_|
    /// is_mutable_place(base))`); presence is the only difference, and it is
    /// supplied by `nullable_boxed_value_copy`.
    ///
    /// A `nobox` global is excluded: its leaves ARE the storage, so the read is
    /// already a snapshot and there is no box to copy out of. So is a read whose
    /// only consumer is a nil test ([`Builder::presence_only`]).
    pub(crate) fn nullable_place_value_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<Ustr>, CompilerError> {
        if self.presence_only == Some(e) {
            return Ok(None);
        }
        if !self.expr_is_nullable(e) {
            return Ok(None);
        }
        let is_place = match &self.body.expr_arena[e] {
            Expr::Global(_) => self.nobox_global_place(e)?.is_none(),
            Expr::Index { .. } => true,
            Expr::FieldAccess { base, .. } => self.is_mutable_place(*base)?,
            _ => false,
        };
        if !is_place {
            return Ok(None);
        }
        Ok(self.value_struct_name_of(e))
    }

    /// The nominal type name of a non-nullable AGGREGATE expression -- a value
    /// struct or a cdata-payload enum. [`Self::value_struct_of`] gates on
    /// `is_value_struct`, which excludes enums; `value_shape` accepts both (it
    /// falls back to `enum_value_shape`), so resolving the name is enough and
    /// the shape lookup stays the thing that fails closed on a non-aggregate.
    pub(crate) fn value_aggregate_of(&self, e: Idx<Expr>) -> Option<Ustr> {
        if self.expr_is_nullable(e) {
            return None;
        }
        if let Some(ty) = self.concrete_expr_ty(e) {
            return ty.peel_nullable().0.nominal();
        }
        None
    }

    /// Whether binding `b`'s bone type is a NON-nullable value struct -- the one
    /// license for SROA to rematerialize an escaping load of its cell (item 8).
    /// Only a bare value-struct `App` qualifies; unsolved inference / non-value
    /// types return false (conservative).
    ///
    /// Nullability is asked through [`Self::is_nullable_ty`] and NOT off the
    /// resolved node. The concrete slot is checked before structural source
    /// lookup, so a generic bound to `BV2?` cannot be mistaken for a bare `BV2`
    /// and license SROA to rebuild an absent row as `BV2_ct(nil, nil)`.
    pub(crate) fn binding_is_nonnull_value_struct(&self, b: Idx<BindingInfo>) -> bool {
        let Some(ty) = self.binding_ty(b) else { return false };
        if let Some(concrete) = self.concrete_ty_id(ty) {
            let (payload, nullable) = concrete.peel_nullable();
            return !nullable
                && matches!(
                    payload,
                    MonoTy::App {
                        ctor: crate::monoty::MonoCtor::Nominal(name), ..
                    } if self.is_value_struct(name.to_owned().into())
                );
        }
        if self.is_nullable_ty(ty) {
            return false;
        }
        let Some(rt) = self.resolve_tyid(ty) else { return false };
        matches!(
            self.ty_node(rt),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) if self.is_value_struct(name)
        )
    }

    /// The fn-typed field call `e` goes through, if inference resolved it to
    /// one (`BodyInference::fn_field_calls`): only method syntax does, so a
    /// call written by name reaches the function even when its first
    /// argument has a fn-typed field of that name.
    pub(crate) fn fn_field_of_call(&self, e: Idx<Expr>) -> Option<Ustr> {
        self.inf?.fn_field_calls.get(&e).copied()
    }

    /// Whether `name` is a module-level global of fn type — recovers `g(args)`
    /// where `g` is a fn-valued global (not a declared function).
    pub(crate) fn is_fn_global(&self, name: Ustr) -> bool {
        self.ws
            .globals_by_name
            .get(&name)
            .copied()
            .map(|ty| matches!(self.resolve_ty(ty), Some(TypeExpr::Function { .. })))
            .unwrap_or(false)
    }

    /// Inject a struct field's default by lowering its initializer from the
    /// struct's `StructDefaults` body (a different body, possibly a different
    /// module) into the current CFG. Self-contained defaults (literals) work; a
    /// default that reads a sibling field gaps (no `SiblingRef` lowering).
    pub(crate) fn lower_struct_default(
        &mut self,
        struct_name: Ustr,
        field: Ustr,
    ) -> Result<Idx<Inst>, CompilerError> {
        let expected = self
            .cur_expr
            .and_then(|literal| self.concrete_struct_field_ty(literal, field));
        self.with_struct_default(struct_name, field, |s, def| {
            s.lower_expr_with_expected(def, expected.as_deref())
        })
    }

    /// The declaring struct's generic ARGUMENTS at this injection.
    ///
    /// The expression is the site cursor: an injection happens while its own
    /// literal is current. Deriving from a cursor is only sound because
    /// [`crate::struct_defaults::args_at`] verifies the answer is an
    /// application of this very struct -- a route that injects while some other
    /// expression is current rejects instead of binding the wrong literal's
    /// arguments.
    fn struct_default_args(
        &mut self,
        struct_name: Ustr,
        sid: bone_semantic::prelude::StructId,
    ) -> Result<Vec<crate::monoty::MonoTy>, CompilerError> {
        let generics = crate::struct_defaults::declared_generics(self.ws, sid);
        if generics == 0 {
            return Ok(Vec::new());
        }
        let Some(e) = self.cur_expr else {
            return Err(gap(format!(
                "generic struct `{struct_name}` injects a field default outside any \
                 typed expression, so its arguments cannot be determined"
            )));
        };
        let ty = self
            .concrete_ty(e)?
            .ok_or_else(|| gap("generic struct default has no concrete literal type"))?;
        let (payload, _) = ty.peel_nullable();
        let crate::monoty::MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params,
        } = payload
        else {
            return Err(gap(format!(
                "generic struct default `{struct_name}` has a non-struct concrete type"
            )));
        };
        if name.as_str() != struct_name.as_str() || params.len() != generics {
            return Err(gap(format!(
                "generic struct default `{struct_name}` has mismatched concrete arguments"
            )));
        }
        Ok(params.to_vec())
    }

    /// Run `f` on a field's default INITIALIZER EXPRESSION with the defaults
    /// body's arena and inference installed. Consumers that need more than a
    /// boxed value (an embed field's presence-plus-leaves) must see the expr,
    /// not its lowering: reading leaves off a lowered `null` indexes nil.
    pub(crate) fn with_struct_default<T>(
        &mut self,
        struct_name: Ustr,
        field: Ustr,
        f: impl FnOnce(&mut Self, Idx<Expr>) -> Result<T, CompilerError>,
    ) -> Result<T, CompilerError> {
        let sid = *self
            .ws
            .struct_by_name
            .get(&struct_name)
            .ok_or_else(|| gap(format!("unknown struct `{struct_name}` for default")))?;
        let owner = BodyOwner::StructDefaults(sid.def);
        let defaults_body = self.ws.modules[sid.module.index()]
            .bodies
            .get(&owner)
            .ok_or_else(|| {
                gap(format!(
                    "struct `{struct_name}` field `{field}` omitted but has no default"
                ))
            })?;
        let def_expr = *defaults_body.field_defaults.get(&field).ok_or_else(|| {
            gap(format!(
                "struct `{struct_name}` literal omits field `{field}` (no default)"
            ))
        })?;
        let defaults_inf = self
            .ws
            .inference(sid.module)
            .and_then(|ir| ir.per_fn.get(&owner));
        let args = self.struct_default_args(struct_name, sid)?;
        let domain = crate::site::SiteDomain::StructDefaults(sid.module, sid.def, args);
        if let Some(defaults_inf) = defaults_inf {
            let plan = self.concrete_plan;
            let tables = plan
                .domain_types(&domain)
                .ok_or_else(|| gap(format!("concrete plan is missing type tables for site domain {domain:?}")))?;
            super::validate_concrete_type_tables(
                self.ws,
                defaults_inf,
                defaults_body,
                tables,
                "concrete struct-default plan",
                std::iter::empty(),
            )?;
        }
        // Lower the default expr in the defaults body's context, into the current
        // block, then restore.
        let saved_body = self.body;
        let saved_inf = self.inf;
        self.body = defaults_body;
        self.inf = defaults_inf;
        // `site_owner` is NOT swapped -- the sites still belong to the enclosing
        // function INSTANCE -- but their `Idx<Expr>` now indexes THIS arena, so
        // they are stamped in the struct's own domain. The walk records the same
        // domain for the same defaults (`instance_graph::struct_default_edges`),
        // which is what lets them join instead of being deliberately unstamped.
        //
        // The ARGUMENTS are part of that key: `Box<int>` and `Box<string>` inject
        // the same declaration's initializers and resolve different callees, so a
        // declaration-keyed domain would collapse them onto one obligation.
        let result = self.in_domain(domain, |s| f(s, def_expr));
        self.body = saved_body;
        self.inf = saved_inf;
        result
    }

    /// An enum variant -> `MakeEnum`. Unit variants have empty payload; tuple
    /// variants fill `positional`; record variants fill `named`.
    /// A cdata-eligible enum (`enum_value_shape`) as a `value`-struct-style Group
    /// `[tag, _0, .., _{n-1}]`: the variant's int tag + its payload values in the
    /// shared positional slots, padding any unused slot with the slot type's zero.
    /// `pack` turns it into `<Enum>_ct(tag, _0, ..)`; the value-struct ABI/SROA/
    /// field machinery do the rest.
    pub(crate) fn enum_cdata_repr(
        &mut self,
        shape: &EnumCdataShape,
        variant: Ustr,
        args: VariantArgs,
    ) -> Result<ValueRepr, CompilerError> {
        let tag = shape.tag_of(variant).ok_or_else(|| gap("enum variant not in shape"))?;
        let tag_inst = self.emit_cur(Inst::Const(Lit::Int(tag)));
        // Map this variant's payload values onto their slot names.
        let mut by_slot: HashMap<Ustr, Idx<Inst>> = HashMap::new();
        match args {
            VariantArgs::Unit => {}
            VariantArgs::Tuple(range) => {
                for (i, v) in self.lower_exprs(range)?.into_iter().enumerate() {
                    by_slot.insert(slot_name(&i.to_string()), v);
                }
            }
            VariantArgs::Record(fields) => {
                for fidx in fields {
                    let lf = self.body.struct_lit_field_arena[fidx];
                    let v = self.lower_expr(lf.value)?;
                    by_slot.insert(slot_name(lf.name.as_str()), v);
                }
            }
        }
        let mut parts = Vec::with_capacity(1 + shape.slots.len());
        parts.push(ValueRepr::Scalar(tag_inst));
        for (nm, c) in &shape.slots {
            let v = match by_slot.get(nm) {
                Some(v) => *v,
                None => {
                    // This variant has no such field -> pad with the slot's zero.
                    let zero = match *c {
                        "int" => Lit::Int(0),
                        "float" => Lit::Float(0.0),
                        "bool" => Lit::Bool(false),
                        "uint32_t" => Lit::Uint(0),
                        other => {
                            return Err(gap(format!(
                                "enum cdata slot has unknown scalar type `{other}`"
                            )));
                        }
                    };
                    self.emit_cur(Inst::Const(zero))
                }
            };
            parts.push(ValueRepr::Scalar(v));
        }
        Ok(ValueRepr::Group {
            name: shape.name,
            parts,
            present: None,
        })
    }

    /// The compile-time expansion kind for `func`, routed through the intrinsic
    /// registry ([`crate::intrinsics::expand_kind`]) -- the ONE recognizer, so
    /// there is no second bespoke annotation check.
    pub(crate) fn fn_expand_kind(&self, func: FuncRef) -> Option<ExpandKind> {
        crate::intrinsics::expand_kind(&self.ws.module(func.module).functions[func.func])
    }

    pub(crate) fn expand_materialize(
        &mut self,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let [arg] = arg_idxs else {
            return Err(gap("materialize expects one argument"));
        };
        let repr = self.lower_expr_repr(*arg)?.into_value();
        Ok(ValueRepr::Scalar(self.materialize_repr(repr)?))
    }

    /// Expand the generic Scene bucket key from the concrete call instance.
    /// The value is a compiler identity, not a tag read from the constructed
    /// root, so construction and lifecycle-plan emission share `MonoTy`.
    pub(crate) fn expand_scene_type_key(
        &mut self,
        call: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let fname = self.ws.module(func.module).functions[func.func].name;
        let [ ] = arg_idxs else {
            return Err(CompilerError::new_with_label(
                self.body.expr_spans[call],
                format!("`@scene_type_key` fn `{fname}` takes no arguments"),
            ));
        };
        let instance = self
            .concrete_callee_for_site(call)?
            .ok_or_else(|| self.gap_here("Scene type-key expansion has no concrete callee"))?;
        let arg_types = instance.arg_types(self.scx.interner());
        let [ty] = arg_types.as_slice() else {
            return Err(CompilerError::new_with_label(
                self.body.expr_spans[call],
                format!("`@scene_type_key` fn `{fname}` must have exactly one type argument"),
            ));
        };
        let key = ty.canonical();
        Ok(ValueRepr::Scalar(
            self.emit_cur(Inst::Const(Lit::Str(Ustr::from(key.as_str())))),
        ))
    }

    /// The box `materialize(v)` asks for: the ordinary whole-value pack, with
    /// every ctor it mints marked EXPLICIT so the guarantee's verifier lets it
    /// through. Routing it through `pack` rather than a second recursion is what
    /// gives the explicit box the tiers the implicit one already had -- the
    /// GC-table, leaf-table and flat-enum arms.
    ///
    /// An ABSENT value has no leaves to build from -- the ctor would get nil per
    /// field and fail the cdata conversion -- so a presence-carrying group
    /// branches: nil when absent, the ctor when present. The branch is legal
    /// HERE precisely because it is not legal in `repack_bound_next_element`:
    /// there the present arm would be an implicit materialization the mark
    /// forbids, while `materialize` IS the authorized box, so building one is
    /// the request.
    pub(crate) fn materialize_repr(&mut self, repr: ValueRepr) -> Result<Idx<Inst>, CompilerError> {
        self.pack_maybe_absent_boxed(repr, BoxKind::Explicit)
    }

    /// Expand a `@struct_expand fn f<T>(u: T) -> [(string, E)]` call into its
    /// literal pairs list. See the free helpers above and
    /// docs/shaders.md (Layer 2).
    pub(crate) fn expand_struct_pairs(
        &mut self,
        call: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let call_span = self.body.expr_spans[call];
        let fname = self.ws.module(func.module).functions[func.func].name;
        let reject = |span: Span, msg: String| CompilerError::new_with_label(span, msg);

        let [arg] = arg_idxs else {
            return Err(reject(
                call_span,
                format!("`@struct_expand` fn `{fname}` takes exactly one argument"),
            ));
        };
        let arg = *arg;
        let arg_span = self.body.expr_spans[arg];

        let ret = self.ws.module(func.module).functions[func.func]
            .return_ty
            .ok_or_else(|| {
                reject(call_span, format!("`@struct_expand` fn `{fname}` must return `[(string, <enum>)]`"))
            })?;
        let enum_name = pairs_return_enum(self.ws, ret).ok_or_else(|| {
            reject(
                call_span,
                format!(
                    "`@struct_expand` fn `{fname}` must return `[(string, <enum>)]`, got `{}`",
                    self.ws.render(None, ret)
                ),
            )
        })?;

        // The argument must be a value struct: its Group's parts are the field
        // values in declaration order, each lowered exactly once.
        let repr = self.lower_expr_repr(arg)?.into_value();
        let ValueRepr::Group {
            name: struct_name,
            parts,
            ..
        } = repr
        else {
            return Err(reject(
                arg_span,
                format!("`@struct_expand` fn `{fname}` argument must be a value struct"),
            ));
        };
        let Some(fields) = value_struct_field_tys(self.ws, struct_name) else {
            return Err(reject(
                arg_span,
                format!("`@struct_expand` fn `{fname}` argument must be a value struct, got `{struct_name}`"),
            ));
        };
        // Validation (`struct_expand_rejection`) already gated this program, so a
        // resolution failure here is unreachable -- surface it rather than skip.
        let variants = struct_expand_variants(self.ws, enum_name, &fields)
            .map_err(|msg| reject(arg_span, format!("`@struct_expand`: {msg}")))?;

        let mut tuples = Vec::with_capacity(fields.len());
        for (i, ((field, _), variant)) in fields.iter().zip(variants).enumerate() {
            let variant_v = self.make_single_payload_variant(enum_name, variant, parts[i].clone())?;
            let key = self.emit_cur(Inst::Const(Lit::Str(*field)));
            let tup = self.emit_cur(Inst::MakeTuple(vec![key, variant_v]));
            tuples.push(tup);
        }
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::MakeVec(tuples))))
    }

    /// Expand a `rock_shader(vert, frag)` call to its pipeline key const
    /// `"<vert>+<frag>"` (docs/shaders.md). The driver
    /// (`skull::mir_emit`) emits the matching top-level `__rock_shader_register`;
    /// this side produces only the key -- no GLSL, so skull-mir stays free of the
    /// GLSL backend. Rejections flow through the shared `resolve_shader_pair`; the
    /// fatal gate is `shader_expand_rejection`.
    pub(crate) fn expand_shader_pipeline(
        &mut self,
        call: Idx<Expr>,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let pair = resolve_shader_pair(self.ws, self.body, call, arg_idxs)?;
        let key = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(pair.key.as_str()))));
        Ok(ValueRepr::Scalar(key))
    }

    /// `build_var("NAME")` / `embed_string("path")` -> the resolved value as a
    /// string const (docs/comptime-plan.md tier 1). Resolution goes through the
    /// shared `resolve_comptime_value`; the fatal gate is
    /// `comptime_input_rejection`.
    pub(crate) fn expand_comptime_input(
        &mut self,
        call: Idx<Expr>,
        arg_idxs: &[Idx<Expr>],
        kind: ExpandKind,
    ) -> Result<ValueRepr, CompilerError> {
        let resolved =
            super::resolve_comptime_value(self.ws, self.body, call, arg_idxs, kind)?;
        // The literal arg was just validated by the resolver.
        let Expr::String(key) = self.body.expr_arena[arg_idxs[0]] else { unreachable!() };
        let input = match (kind, resolved.file_abs) {
            (ExpandKind::BuildVar, _) => crate::mir::ComptimeInputId::Var(key),
            // File reads are keyed by the ABSOLUTE path the resolver landed
            // on, so invalidation and the watcher need no root re-derivation.
            (_, Some(abs)) => crate::mir::ComptimeInputId::File(abs),
            (_, None) => unreachable!("embed resolution always lands on a file"),
        };
        let inst = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(resolved.value.as_str()))));
        self.f.comptime_reads.push(crate::mir::ComptimeRead {
            input,
            value_hash: crate::mir::comptime_value_hash(&resolved.value),
            span: self.body.expr_spans[call],
        });
        Ok(ValueRepr::Scalar(inst))
    }

    pub(crate) fn expand_compute_artifact(
        &mut self,
        call: Idx<Expr>,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let entry = resolve_compute_entry(self.ws, self.body, call, arg_idxs)?;
        let key = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(
            self.ws.module(entry.module).functions[entry.func].name.as_str(),
        ))));
        Ok(ValueRepr::Scalar(key))
    }

    /// `@uniform_expand` -> one direct typed store per primitive leaf of `u`,
    /// at its std140 element index. No helper call per leaf, and no pack: the
    /// Group's leaves go straight from registers into the views.
    pub(crate) fn expand_uniform_block(
        &mut self,
        call: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let call_span = self.body.expr_spans[call];
        let fname = self.ws.module(func.module).functions[func.func].name;
        let reject = |span: Span, msg: String| CompilerError::new_with_label(span, msg);

        // TWO shapes, one annotation, because both answer the same question --
        // what is `T`'s std140 plan. The 1-arg form yields that plan's
        // FINGERPRINT as a constant, which is how a call site says WHICH of a
        // program's blocks it means: with one block per stage the shader name
        // no longer identifies one, and the value's type is what does.
        let (fv_e, iv_e, u_e) = match arg_idxs {
            [u_e] => {
                // The 1-arg form is the fingerprint, so it must DECLARE `-> int`.
                // A 1-arg fn returning unit is a malformed packer, not a
                // fingerprint, and stays rejected (tests/errors/uniform_expand.rock.err).
                // Same shape as `@vertex_expand`, which switches on its declared
                // return type rather than guessing from the call.
                let returns_int = self.ws.module(func.module).functions[func.func]
                    .return_ty
                    .map(|rt| self.ws.render(None, rt) == "int" )
                    .unwrap_or(false);
                if !returns_int {
                    return Err(reject(call_span, uniform_expand_arity_msg(fname)));
                }
                let u_span = self.body.expr_spans[*u_e];
                let Some(struct_name) = self.value_struct_name_of(*u_e) else {
                    return Err(reject(u_span, uniform_expand_arg_msg(fname)));
                };
                let plan = uniform_block_checked(self.ws, struct_name)
                    .map_err(|m| reject(u_span, format!("`@uniform_expand`: {m}")))?;
                let fp = self.emit_cur(Inst::Const(Lit::Int(plan.fingerprint() as i64)));
                return Ok(ValueRepr::Scalar(fp));
            }
            [fv_e, iv_e, u_e] => (*fv_e, *iv_e, *u_e),
            _ => return Err(reject(call_span, uniform_expand_arity_msg(fname))),
        };
        let u_span = self.body.expr_spans[u_e];

        let fv = self.lower_expr(fv_e)?;
        let iv = self.lower_expr(iv_e)?;

        // From the TYPE, not from the repr's variant: a group read out of a
        // local arrives as a `Scalar` (a loaded cell), which `repr_to_leaves`
        // handles perfectly well but which carries no struct name.
        let Some(struct_name) = self.value_struct_name_of(u_e) else {
            return Err(reject(u_span, uniform_expand_arg_msg(fname)));
        };
        let plan = uniform_block_checked(self.ws, struct_name)
            .map_err(|m| reject(u_span, format!("`@uniform_expand`: {m}")))?;
        let stores = plan.leaf_stores();

        // A block held in a GLOBAL is read one leaf at a time, each leaf
        // loaded right before its own store, so at most the box and one leaf
        // are live at any store. The whole-value read below loads every leaf
        // into a register before the first store; inside a trace that keeps
        // all of them (41 for the engine block) live in every exit snapshot
        // taken during the pack, against a 16-register side-trace budget
        // (docs/trace-pressure-work-plan.md).
        if let Some(source) = self.global_uniform_source(u_e, struct_name)? {
            let leaf_count = source.leaf_count();
            if leaf_count != stores.len() {
                return Err(reject(
                    u_span,
                    format!(
                        "`@uniform_expand`: `{struct_name}` has {leaf_count} leaves but its std140 \
                         plan has {} slots",
                        stores.len()
                    ),
                ));
            }
            for (i, store) in stores.iter().enumerate() {
                let leaf = self.global_uniform_leaf(&source, i);
                self.emit_uniform_leaf_store(fv, iv, leaf, store);
            }
            return Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))));
        }

        let leaves = self.uniform_block_leaves(u_e, struct_name)?;
        // `uniform_block_checked` already proved the two walkers agree on the
        // leaf PATH sequence, so this can only differ if the repr handed back a
        // different leaf count than the shape describes.
        if leaves.len() != stores.len() {
            return Err(reject(
                u_span,
                format!(
                    "`@uniform_expand`: `{struct_name}` lowered to {} leaves but its std140 plan \
                     has {} slots",
                    leaves.len(),
                    stores.len()
                ),
            ));
        }

        for (leaf, store) in leaves.iter().zip(&stores) {
            self.emit_uniform_leaf_store(fv, iv, *leaf, store);
        }
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))))
    }

    /// The leaves of a uniform-block argument, whatever WIRE its expression
    /// arrived on.
    ///
    /// A uniform block's packing is a property of its TYPE
    /// (`docs/luajit-wide-value-abi.md` gate 4), so the leaf shape comes from
    /// the nominal rather than from the argument's `RepPlan`. It has to: an
    /// ordinary `value` wider than the ABI cut crosses a Rock-to-Rock call as
    /// one boxed slot, and that slot's plan is `Opaque` -- it carries no leaf
    /// list of its own, exactly as a global's boxed cdata does on the path
    /// just above. Reading the box field by field is what `repr_to_leaves_shape`
    /// already does for every other boxed source.
    fn uniform_block_leaves(
        &mut self,
        u_e: Idx<Expr>,
        struct_name: Ustr,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let repr = self.lower_expr_repr(u_e)?;
        if self.scx.rep_arena().value_shape(repr.representation()).is_some() {
            return self.repr_to_leaves_bundle(&repr);
        }
        let shape = self.nominal_rep_shape_required(struct_name, false)?;
        self.repr_to_leaves_shape(repr.value(), &shape)
    }

    /// One std140 slot of a uniform block: a direct typed store of `leaf`
    /// through the view its wire type selects.
    fn emit_uniform_leaf_store(
        &mut self,
        fv: Idx<Inst>,
        iv: Idx<Inst>,
        leaf: Idx<Inst>,
        store: &crate::gpu_layout::LeafStore,
    ) {
        // `LeafStore.offset` is a BYTE offset; a leaf view indexes in units
        // of its own primitive, and every std140 slot here is 4-byte.
        let idx = self.emit_cur(Inst::Const(Lit::Int((store.offset / 4) as i64)));
        let base = match store.view {
            crate::gpu_layout::WireView::Float => fv,
            crate::gpu_layout::WireView::Int => iv,
            crate::gpu_layout::WireView::Uint => iv,
        };
        let leaf = match store.view {
            crate::gpu_layout::WireView::Uint => self.emit_cur(Inst::UintBitcastInt(leaf)),
            _ => leaf,
        };
        self.emit_cur(Inst::StoreIndex {
            base,
            index: idx,
            value: leaf,
            kind: ContainerKind::NativeBufferLeaf,
        });
    }

    /// `u` as a global place the packer can read leaf by leaf, or `None` when
    /// `u` is anything else (a local group, a literal, a call result), which
    /// takes the whole-value path. A global whose shape has a nullable or enum
    /// child is left to that path too: those leaves need presence branches or
    /// a tag spread, and no uniform block carries either.
    fn global_uniform_source(
        &mut self,
        u_e: Idx<Expr>,
        struct_name: Ustr,
    ) -> Result<Option<GlobalUniformSource>, CompilerError> {
        let Expr::Global(name) = self.body.expr_arena[u_e].clone() else {
            return Ok(None);
        };
        if let Some((leaves, Some(_))) = self.nobox_global_place(u_e)? {
            return Ok(Some(GlobalUniformSource::LeafGlobals(leaves)));
        }
        let shape = self.nominal_rep_shape_required(struct_name, false)?;
        if !self.shape_is_plain_keys(&shape) {
            return Ok(None);
        }
        // The key paths `cdata_leaves` reads a box with, one leaf at a time.
        let paths = self.boxed_leaf_paths(&shape)?;
        let boxed = self.emit_cur(Inst::Global(name));
        Ok(Some(GlobalUniformSource::Boxed { boxed, paths }))
    }

    /// Load leaf `i` of a global uniform source, at the point of the call.
    fn global_uniform_leaf(&mut self, source: &GlobalUniformSource, i: usize) -> Idx<Inst> {
        match source {
            GlobalUniformSource::LeafGlobals(leaves) => self.emit_cur(Inst::Global(leaves[i])),
            GlobalUniformSource::Boxed { boxed, paths } => {
                let mut v = *boxed;
                for field in &paths[i] {
                    v = self.emit_cur(Inst::Field { base: v, field: *field });
                }
                v
            }
        }
    }

    /// `@uniform_values` -> a typed packet of logical scalar leaves. The
    /// physical target never sees this representation's offsets because there
    /// are none; each backend projects the packet through its own live ABI.
    pub(crate) fn expand_uniform_values(
        &mut self,
        call: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let call_span = self.body.expr_spans[call];
        let fname = self.ws.module(func.module).functions[func.func].name;
        let reject = |span: Span, msg: String| CompilerError::new_with_label(span, msg);
        let [u_e] = arg_idxs else {
            return Err(reject(
                call_span,
                format!("`@uniform_values` fn `{fname}` takes exactly one uniform group"),
            ));
        };
        let u_span = self.body.expr_spans[*u_e];
        let Some(struct_name) = self.value_struct_name_of(*u_e) else {
            return Err(reject(
                u_span,
                format!("`@uniform_values` fn `{fname}`'s argument must be a concrete value struct"),
            ));
        };
        let members = uniform_semantic_checked(self.ws, struct_name)
            .map_err(|m| reject(u_span, format!("`@uniform_values`: {m}")))?;
        let Some(TypeExpr::App { ctor: TypeCtor::Nominal(block_name), .. }) = self.expr_ty(call) else {
            return Err(reject(
                call_span,
                format!("`@uniform_values` fn `{fname}` must return the canonical packet struct"),
            ));
        };
        let packet = uniform_values_return_shape(self.ws, block_name)
            .map_err(|m| reject(call_span, format!("`@uniform_values`: {m}")))?;
        let leaves = self.uniform_block_leaves(*u_e, struct_name)?;
        let expected: Vec<crate::gpu_layout::WireKind> = members
            .iter()
            .flat_map(|(_, kind)| std::iter::repeat(*kind).take(kind.leaf_suffixes().len()))
            .collect();
        if leaves.len() != expected.len() {
            return Err(reject(
                u_span,
                format!(
                    "`@uniform_values`: `{struct_name}` lowered to {} leaves but its logical interface has {}",
                    leaves.len(),
                    expected.len()
                ),
            ));
        }
        let mut canonical = Vec::with_capacity(leaves.len());
        for (leaf, kind) in leaves.into_iter().zip(expected) {
            let variant = match kind {
                crate::gpu_layout::WireKind::I32 => packet.int_variant,
                crate::gpu_layout::WireKind::U32 => packet.uint_variant,
                crate::gpu_layout::WireKind::Bool32 => packet.bool_variant,
                crate::gpu_layout::WireKind::F32
                | crate::gpu_layout::WireKind::F32x2
                | crate::gpu_layout::WireKind::F32x3
                | crate::gpu_layout::WireKind::F32x4
                | crate::gpu_layout::WireKind::Mat4 => packet.float_variant,
            };
            canonical.push(self.make_single_payload_variant(packet.leaf_enum, variant, ValueRepr::Scalar(leaf))?);
        }
        let fingerprint = self.emit_cur(Inst::Const(Lit::Int(
            crate::shader_ir::semantic_uniform_group_fingerprint(struct_name, &members) as i64,
        )));
        let values = self.emit_cur(Inst::MakeVec(canonical));
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::MakeStruct {
            ty: Some(packet.block_name),
            fields: vec![(packet.fingerprint_field, fingerprint), (packet.leaves_field, values)],
        })))
    }

    /// `@vertex_expand` -> the element's vertex layout as a literal. The
    /// argument is used for its TYPE only and is deliberately not lowered: the
    /// call collapses to a constant, as `@shader_expand` does.
    pub(crate) fn expand_vertex_layout(
        &mut self,
        call: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let call_span = self.body.expr_spans[call];
        let f = &self.ws.module(func.module).functions[func.func];
        let (fname, ret) = (f.name, f.return_ty);
        let reject = |span: Span, msg: String| CompilerError::new_with_label(span, msg);

        let [arg] = arg_idxs else {
            return Err(reject(
                call_span,
                format!("`@vertex_expand` fn `{fname}` takes exactly one argument"),
            ));
        };
        let arg = *arg;
        let arg_span = self.body.expr_spans[arg];

        let Some(kind) = ret.and_then(|r| vertex_expand_return(self.ws, r)) else {
            return Err(reject(call_span, vertex_expand_ret_msg(self.ws, fname, ret)));
        };
        let Some(elem) = self.nb_value_elem(arg)? else {
            return Err(reject(arg_span, vertex_expand_arg_msg(fname)));
        };

        let rep_arena = self.scx.rep_arena();
        let plan = crate::vertex_plan::vertex_plan(self.ws, &rep_arena, elem)
            .map_err(|e| reject(arg_span, format!("`@vertex_expand`: {}", e.message)))?;
        // `float_unit_*` carry the integer-attribute and cumulative-offset
        // rejections, so the float-unit view is never emitted for a layout it
        // cannot describe.
        let bad = |m: String| reject(arg_span, format!("`@vertex_expand`: {m}"));
        let v = match kind {
            VertexExpandRet::Stride => {
                let s = plan.float_unit_stride().map_err(bad)?;
                self.emit_cur(Inst::Const(Lit::Int(s as i64)))
            }
            VertexExpandRet::Sizes => {
                let sizes = plan.float_unit_sizes().map_err(bad)?;
                let vs: Vec<Idx<Inst>> = sizes
                    .iter()
                    .map(|n| self.emit_cur(Inst::Const(Lit::Int(*n as i64))))
                    .collect();
                self.emit_cur(Inst::MakeVec(vs))
            }
        };
        Ok(ValueRepr::Scalar(v))
    }

    /// Construct `enum_name::variant(payload)` for a compiler-built variant with
    /// a single positional payload, through the same representation a source
    /// ctor takes: cdata `<Enum>_ct` when the enum is cdata-eligible, the flat
    /// cells (a scalarizable payload spread into its slots) when it is flat.
    pub(crate) fn make_single_payload_variant(
        &mut self,
        enum_name: Ustr,
        variant: Ustr,
        payload: ValueRepr,
    ) -> Result<Idx<Inst>, CompilerError> {
        if let Some(shape) = EnumPlan::for_name(self.ws, enum_name).and_then(|plan| plan.flat().cloned()) {
            let cells = self.flat_ctor_cells(&shape, variant, vec![FlatPayload::Repr(payload)], Vec::new())?;
            let r = self.flat_cells_to_repr(&shape, cells);
            return self.pack(r);
        }
        // A payload the enum cannot hold unboxed has nowhere to live but a box --
        // the same boundary `uniform(x: Vec2)` marks with an explicit
        // `materialize`. The compiler mints this box itself and there is no
        // source line to mark, so it mints the AUTHORIZED one rather than
        // rejecting a program the hand-written overload accepts.
        let payload = self.pack(payload)?;
        self.authorize_nobox_box(payload);
        if let Some(shape) = EnumPlan::for_name(self.ws, enum_name).and_then(|plan| plan.cdata().cloned()) {
            let tag = shape
                .tag_of(variant)
                .ok_or_else(|| gap("struct_expand: variant not in cdata enum shape"))?;
            let tag_inst = self.emit_cur(Inst::Const(Lit::Int(tag)));
            let mut parts = vec![ValueRepr::Scalar(tag_inst)];
            for (nm, c) in &shape.slots {
                let v = if *nm == slot_name("0") {
                    payload
                } else {
                    let zero = match *c {
                        "float" => Lit::Float(0.0),
                        "bool" => Lit::Bool(false),
                        _ => Lit::Int(0),
                    };
                    self.emit_cur(Inst::Const(zero))
                };
                parts.push(ValueRepr::Scalar(v));
            }
            return Ok(self.pack(ValueRepr::Group {
                name: enum_name,
                parts,
                present: None,
            })?);
        }
        Ok(self.emit_cur(Inst::MakeEnum {
            tag: variant,
            ty: enum_name,
            positional: vec![payload],
            named: vec![],
        }))
    }

    pub(crate) fn lower_enum_variant(
        &mut self,
        enum_name: Ustr,
        variant: Ustr,
        args: VariantArgs,
    ) -> Result<Idx<Inst>, CompilerError> {
        if let Some(shape) = EnumPlan::for_name(self.ws, enum_name).and_then(|plan| plan.cdata().cloned()) {
            let r = self.enum_cdata_repr(&shape, variant, args)?;
            return Ok(self.pack(r)?);
        }
        if let VariantArgs::Unit = args {
            // A payload-free enum is an `int` value GLOBALLY (the variant's
            // declaration-order ordinal). Fast int `match`/`==`/Map keys, and
            // C-layout-eligible as a value-struct field. See
            // docs/skull-enum-value-types.md.
            if let Some(ord) = payload_free_ordinal(self.ws, enum_name, variant) {
                return Ok(self.emit_cur(Inst::Const(Lit::Int(ord))));
            }
            // A unit variant of a MIXED enum (e.g. `Option::None`) keeps the
            // INTERNED singleton table (emitted by `enum_runtime_defs`): the
            // enum still has payload variants, so its values stay tables, and the
            // singleton is reference-equal across uses + usable as a Map key.
            let g = Ustr::from(
                format!("__rock_enum_{}_{variant}", crate::mangle::type_symbol(self.ws, enum_name)).as_str(),
            );
            return Ok(self.emit_cur(Inst::Global(g)));
        }
        let (positional, named) = match args {
            VariantArgs::Unit => (Vec::new(), Vec::new()),
            VariantArgs::Tuple(range) => (self.lower_exprs(range)?, Vec::new()),
            VariantArgs::Record(fields) => {
                let mut named = Vec::new();
                for fidx in fields {
                    let lf = self.body.struct_lit_field_arena[fidx];
                    let v = self.lower_expr(lf.value)?;
                    named.push((lf.name, v));
                }
                (Vec::new(), named)
            }
        };
        Ok(self.emit_cur(Inst::MakeEnum {
            tag: variant,
            ty: enum_name,
            positional,
            named,
        }))
    }

    // --- enum field flattening (docs/enum-allocation-report.md) ---

    /// `Some(shape)` iff `base.field` is a flattenable enum field: `base` is a
    /// non-`value` struct and the field's declared type is a [`flat_enum_shape`]
    /// enum. Drives the construct/store/match/materialize rewrites.
    /// A swizzle read (`v.yx`, `c.rgb` -- docs/vec-swizzles-plan.md): the base's
    /// inferred type is a float vec and `field` is a valid synthetic swizzle for
    /// it. Bone already accepted the read, so this only distinguishes swizzles
    /// from real fields (len-1 `x/y/z/w`) and non-vec bases.
    pub(crate) fn swizzle_of(&self, base: Idx<Expr>, field: Ustr) -> Option<Vec<usize>> {
        if field.len() == 1 && "xyzw".contains(field.as_str()) {
            return None;
        }
        if let Some(ty) = self.concrete_expr_ty(base) {
            let Some(name) = ty.peel_nullable().0.nominal() else { return None };
            return bone_semantic::inference::vec_swizzle_components(name.as_str(), field.as_str());
        }
        None
    }

    /// Component `Field` reads off an already-lowered (packed) vec value, grouped
    /// per `swizzle_group`. The read order is the swizzle's component order.
    pub(crate) fn swizzle_from_packed(&mut self, bv: Idx<Inst>, idxs: &[usize]) -> ValueRepr {
        let comps = ["x", "y", "z", "w"];
        let parts: Vec<ValueRepr> = idxs
            .iter()
            .map(|&i| {
                ValueRepr::Scalar(self.emit_cur(Inst::Field { base: bv, field: Ustr::from(comps[i]) }))
            })
            .collect();
        swizzle_group(parts)
    }
}

/// Where a global uniform block's leaves are read from, leaf by leaf.
enum GlobalUniformSource {
    /// A `nobox` global: one scalar global per leaf.
    LeafGlobals(Vec<Ustr>),
    /// A boxed global: the box plus each leaf's field path into it.
    Boxed { boxed: Idx<Inst>, paths: Vec<Vec<Ustr>> },
}

impl GlobalUniformSource {
    fn leaf_count(&self) -> usize {
        match self {
            GlobalUniformSource::LeafGlobals(leaves) => leaves.len(),
            GlobalUniformSource::Boxed { paths, .. } => paths.len(),
        }
    }
}
