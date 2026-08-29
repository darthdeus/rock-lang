//! Element STORAGE lowering: native-buffer leaf addressing, and the column form
//! of a vec / map whose element is a `nobox` value (header, push, append, index,
//! per-leaf store, and the presence column beside the leaf columns).
//!
//! Column eligibility itself is decided in `shape` -- this is the code that
//! builds and reads the layout that module describes.
use super::*;

impl Builder<'_> {
    /// A column-stored element (`xs[i]` where `xs: [E]` stores flat columns) as
    /// an unboxed `Group`: bounds-check `i` once, then read leaf `k` from column
    /// `k`. No element table is touched because none exists.
    ///
    /// `value_leaf_paths` and `rebuild_from_leaves` both walk fields in
    /// declaration order depth-first, so the flat read order lines up with the
    /// nested shape without a second traversal.
    /// `Some((buf, checked_index, elem, dotted_path))` iff `<base>.<field>` is a
    /// field path rooted at `buf[i]` over a leaf-addressed NativeBuffer.
    ///
    /// Peels the FieldAccess chain to build the dotted path the leaf plan is
    /// keyed by (`buf[i].lo.n` -> `"lo.n"`), so a write at ANY depth lands on
    /// the right leaves. The bounds check is emitted once, here.
    pub(crate) fn nb_leaf_field_place(
        &mut self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<Option<(Idx<Inst>, Idx<Inst>, Ustr, String)>, CompilerError> {
        // Walk down through nested field accesses to the `buf[i]` root.
        let mut segs = vec![field];
        let mut cur = base;
        loop {
            match self.body.expr_arena[cur] {
                Expr::FieldAccess { base: b, field: f } => {
                    segs.push(f);
                    cur = b;
                }
                Expr::Index { base: ib, index } => {
                    let ContainerKind::NativeBufferValue(elem) = self.nb_kind(ib)? else {
                        return Ok(None);
                    };
                    if !self.is_leaf_addressed(elem)? {
                        return Ok(None);
                    }
                    let (b, _) = self.lower_index_base(ib)?;
                    let i = self.lower_expr(index)?;
                    // `cur` is the `Expr::Index` node this bounds check belongs
                    // to -- not the outer field access the assignment targets.
                    let checked = self.with_site_expr(cur, |s| s.nb_checked_index(b, i))?;
                    segs.reverse();
                    let path =
                        segs.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(".");
                    return Ok(Some((b, checked, elem, path)));
                }
                _ => return Ok(None),
            }
        }
    }

    /// Write `buf[i].<path> = value` leaf-wise. `path` names either one
    /// primitive leaf or a nested value field spanning several; both are just a
    /// prefix match over the element's leaf plan, so depth costs nothing.
    pub(crate) fn nb_leaf_field_store(
        &mut self,
        buf: Idx<Inst>,
        checked: Idx<Inst>,
        elem: Ustr,
        path: &str,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let container = self.native_buffer_plan(elem)?;
        let plan = container
            .leaf_addresses()
            .ok_or_else(|| gap("NativeBuffer element has no owned leaf address plan"))?;
        let dot = format!("{path}.");
        let hit: Vec<PlaceBundle> = plan
            .into_iter()
            .filter(|a| a.path.as_str() == path || a.path.as_str().starts_with(&dot))
            .cloned()
            .map(|address| container.place(address).map_err(|e| gap(e.message)))
            .collect::<Result<_, _>>()?;
        if hit.is_empty() {
            return Err(gap(format!("`{path}` is not a leaf of NativeBuffer<{elem}>")));
        }
        // An exact primitive leaf is the one hit whose path IS `path`. A NESTED
        // field spanning a single leaf also yields one hit, but its path is
        // `<path>.<leaf>` -- so discriminating on hit COUNT lowers that field's
        // aggregate RHS as a scalar and writes the whole cdata through a
        // primitive view. Everything that is not an exact leaf is an aggregate.
        let leaves = if hit.len() == 1 && hit[0].address().path.as_str() == path {
            vec![self.lower_expr(value)?]
        } else {
            let _sub = match self.value_aggregate_of(value) {
                Some(n) => self.nominal_rep_shape(n, false)?,
                None => None,
            }
            .ok_or_else(|| {
                gap(format!(
                    "whole write to `{path}` needs a value-struct or cdata-enum RHS"
                ))
            })?;
            let repr = self.lower_expr_repr(value)?;
            self.repr_to_leaves_bundle(&repr)?
        };
        if leaves.len() != hit.len() {
            return Err(gap(format!(
                "leaf store arity mismatch for `{elem}.{path}`: {} leaves vs {} slots",
                leaves.len(),
                hit.len()
            )));
        }
        let mut views = HashMap::new();
        for (place, leaf) in hit.iter().zip(leaves) {
            let a = place.address();
            let at = self.leaf_index(checked, a);
            let view = self.leaf_view(buf, a.view_field, &mut views);
            self.emit_cur(Inst::StoreIndex {
                base: view,
                index: at,
                value: leaf,
                kind: ContainerKind::NativeBufferLeaf,
            });
        }
        Ok(())
    }

    /// Whether `elem` is leaf-addressable: it has a validated C leaf layout
    /// whose every leaf divides its view cleanly. `false` keeps the buffer on
    /// the existing slot-reference path, which is CORRECT, just boxier --
    /// leaf addressing does not change storage, so the two address the same
    /// bytes and a program may mix them freely.
    ///
    /// Both targets. The views are `ffi.cast` pointers on luajit and TypedArrays
    /// over one ArrayBuffer on JS, but the ADDRESSING is the same C layout either
    /// way -- the owned leaf-address projection's stride and offset are already in units of each
    /// view's own element type, so one lowering serves both.
    fn native_buffer_plan(&self, elem: Ustr) -> Result<ContainerPlan, CompilerError> {
        let ty = MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(Ustr::from("NativeBuffer")),
            params: vec![MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(elem),
                params: Vec::new(),
            }],
        };
        self.container_plan_for_type(&ty, ContainerFamily::NativeBuffer)
    }

    pub(crate) fn is_leaf_addressed(&self, elem: Ustr) -> Result<bool, CompilerError> {
        Ok(self.native_buffer_plan(elem)?.is_leaf_addressed())
    }

    /// Read `buf[i].<path>`'s single primitive leaf, for a read-modify-write
    /// (`buf[i].pos.x++`, `buf[i].w += 1`). `None` when `target` is not such a
    /// place, or when the path is not exactly ONE leaf -- a `+=` over an
    /// aggregate has no scalar to combine.
    ///
    /// JS ONLY, deliberately. Every other read/write shape is leaf-addressed on
    /// both targets, but a read-modify-write reads a PLACE: luajit's `.ptr[i]`
    /// is a struct reference, so `buf[i].pos.x` is a real assignable lvalue and
    /// the ordinary field path is both correct and tighter. JS has no such
    /// reference -- `ptr[i]` is a number -- so without this the whole element is
    /// materialized and the write lands on a copy.
    pub(crate) fn nb_leaf_rmw_place(
        &mut self,
        target: Idx<Expr>,
    ) -> Result<Option<(Idx<Inst>, Idx<Inst>, Ustr, PlaceBundle)>, CompilerError> {
        if self.scx.target != bone_semantic::hir::FnBackend::Js {
            return Ok(None);
        }
        let Expr::FieldAccess { base, field } = self.body.expr_arena[target] else {
            return Ok(None);
        };
        let Some((buf, checked, elem, path)) = self.nb_leaf_field_place(base, field)? else {
            return Ok(None);
        };
        let container = self.native_buffer_plan(elem)?;
        let Some(addr) = container
            .leaf_addresses()
            .and_then(|plan| plan.iter().find(|a| a.path.as_str() == path))
            .cloned()
        else {
            return Ok(None);
        };
        let place = container.place(addr).map_err(|e| gap(e.message))?;
        Ok(Some((buf, checked, elem, place)))
    }

    /// The load half of [`Self::nb_leaf_rmw_place`].
    pub(crate) fn nb_leaf_load(
        &mut self,
        buf: Idx<Inst>,
        checked: Idx<Inst>,
        place: &PlaceBundle,
    ) -> Idx<Inst> {
        let addr = place.address();
        let at = self.leaf_index(checked, addr);
        let view = self.leaf_view(buf, addr.view_field, &mut HashMap::new());
        self.emit_cur(Inst::Index { base: view, index: at, kind: ContainerKind::NativeBufferLeaf })
    }

    /// The store half of [`Self::nb_leaf_rmw_place`].
    pub(crate) fn nb_leaf_store(
        &mut self,
        buf: Idx<Inst>,
        checked: Idx<Inst>,
        place: &PlaceBundle,
        value: Idx<Inst>,
    ) {
        let addr = place.address();
        let at = self.leaf_index(checked, addr);
        let view = self.leaf_view(buf, addr.view_field, &mut HashMap::new());
        self.emit_cur(Inst::StoreIndex {
            base: view,
            index: at,
            value,
            kind: ContainerKind::NativeBufferLeaf,
        });
    }

    /// `buf[i]` on a leaf-addressed NativeBuffer: bounds-check the ELEMENT once,
    /// then read each leaf from its own view. No slot reference is taken, so the
    /// read allocates nothing; the element is rebuilt as a `Group`, exactly the
    /// shape `let v = buf[i]` takes, so SROA peels it wherever the binding is
    /// only projected from.
    pub(crate) fn nb_leaf_element_repr(
        &mut self,
        buf: Idx<Inst>,
        index: Idx<Inst>,
        elem: Ustr,
    ) -> Result<ValueRepr, CompilerError> {
        let container = self.native_buffer_plan(elem)?;
        let shape = container
            .element_shape(&self.scx.rep_arena())
            .ok_or_else(|| gap("leaf-addressed element is not a value struct"))?;
        let plan: Vec<PlaceBundle> = container
            .leaf_addresses()
            .ok_or_else(|| gap("leaf-addressed element has no owned leaf address plan"))?
            .iter()
            .cloned()
            .map(|address| container.place(address).map_err(|e| gap(e.message)))
            .collect::<Result<_, _>>()?;
        let checked = self.nb_checked_index(buf, index)?;
        let mut views = HashMap::new();
        let leaves: Vec<Idx<Inst>> = plan
            .iter()
            .map(|place| {
                let a = place.address();
                let at = self.leaf_index(checked, a);
                let view = self.leaf_view(buf, a.view_field, &mut views);
                self.emit_cur(Inst::Index {
                    base: view,
                    index: at,
                    kind: ContainerKind::NativeBufferLeaf,
                })
            })
            .collect();
        let mut idx = 0;
        Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx))
    }

    /// `buf[i] = v` into a leaf-addressed NativeBuffer: bounds-check once, then
    /// store leaf `k` through its view. The element never materializes, which is
    /// what lets a `nobox` type live in one.
    pub(crate) fn nb_leaf_element_store(
        &mut self,
        buf: Idx<Inst>,
        index: Idx<Inst>,
        value: Idx<Expr>,
        elem: Ustr,
    ) -> Result<(), CompilerError> {
        let container = self.native_buffer_plan(elem)?;
        let plan: Vec<PlaceBundle> = container
            .leaf_addresses()
            .ok_or_else(|| gap("leaf-addressed element has no owned leaf address plan"))?
            .iter()
            .cloned()
            .map(|address| container.place(address).map_err(|e| gap(e.message)))
            .collect::<Result<_, _>>()?;
        let bundle = self.lower_expr_repr(value)?;
        let shape = container
            .element_shape(&self.scx.rep_arena())
            .ok_or_else(|| gap(format!("NativeBuffer<{elem}> has no value leaf plan")))?;
        let leaves = self.repr_to_storage_leaves(&bundle, &shape, "native buffer store")?;
        if leaves.len() != plan.len() {
            return Err(gap(format!(
                "leaf store arity mismatch for NativeBuffer<{elem}>: {} leaves vs {} slots",
                leaves.len(),
                plan.len()
            )));
        }
        // The check precedes every leaf write: checking after would let the
        // out-of-bounds writes happen first.
        let checked = self.nb_checked_index(buf, index)?;
        let mut views = HashMap::new();
        for (place, leaf) in plan.iter().zip(leaves) {
            let a = place.address();
            let at = self.leaf_index(checked, a);
            let view = self.leaf_view(buf, a.view_field, &mut views);
            self.emit_cur(Inst::StoreIndex {
                base: view,
                index: at,
                value: leaf,
                kind: ContainerKind::NativeBufferLeaf,
            });
        }
        Ok(())
    }

    /// The view projection for `field`, read ONCE per element access and reused
    /// across that element's leaves -- a table read per leaf is pure waste when
    /// every leaf of one type shares a view.
    pub(crate) fn leaf_view(
        &mut self,
        buf: Idx<Inst>,
        field: Ustr,
        cache: &mut HashMap<Ustr, Idx<Inst>>,
    ) -> Idx<Inst> {
        if let Some(v) = cache.get(&field) {
            return *v;
        }
        let v = self.emit_cur(Inst::Field { base: buf, field });
        cache.insert(field, v);
        v
    }

    /// `i * stride + offset` in the leaf's own view units. Both constants; the
    /// `+ 0` and `* 1` cases fold so a flat all-one-type element reads as
    /// plainly as it did before.
    pub(crate) fn leaf_index(&mut self, checked: Idx<Inst>, a: &LeafAddr) -> Idx<Inst> {
        let mut at = checked;
        if a.stride != 1 {
            let s = self.emit_cur(Inst::Const(Lit::Int(a.stride)));
            at = self.emit_cur(Inst::IntMul(at, s));
        }
        if a.offset != 0 {
            let o = self.emit_cur(Inst::Const(Lit::Int(a.offset)));
            at = self.emit_cur(Inst::IntAdd(at, o));
        }
        at
    }

    /// `__nb_checked_index(buf, i)` -- bounds-check the element access WITHOUT
    /// taking a slot reference, returning the index. One call per element
    /// access, never per leaf.
    pub(crate) fn nb_checked_index(
        &mut self,
        buf: Idx<Inst>,
        index: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        self.call_spine("__nb_checked_index", vec![buf, index])
    }

    pub(crate) fn column_element_repr(
        &mut self,
        vec: Idx<Inst>,
        index: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<ValueBundle, CompilerError> {
        let shape = self.column_shape(elem)?;
        let slot = self.checked_column_index(vec, index);
        // Presence leads the reads for a nullable element, which is exactly what
        // `rebuild_from_leaves` consumes it as -- the Group comes back with its
        // `present` set from the column instead of a nil compare on a box.
        let leaves: Vec<Idx<Inst>> = column_leaf_paths(&shape)
            .into_iter()
            .map(|path| {
                let col = self.emit_cur(Inst::Field { base: vec, field: path });
                self.emit_cur(Inst::Index {
                    base: col,
                    index: slot,
                    kind: ContainerKind::VecColumn,
                })
            })
            .collect();
        let mut idx = 0;
        Ok(ValueBundle::new(
            elem.representation(),
            self.rebuild_from_leaves(&shape, &leaves, &mut idx),
        ))
    }

    /// The unique stdlib function `name`, for a lowering that SYNTHESIZES a call
    /// (the cold column ops, which have no call expr of their own to resolve
    /// through). Rejects rather than falling back: a missing or ambiguous name
    /// means the stdlib and this lowering have drifted apart.
    ///
    /// Multiple decls of one name are the backend-sibling scheme, so pick the one
    /// serving this target; only a residual tie is ambiguous.
    pub(crate) fn stdlib_funcref(&self, name: &str) -> Result<FuncRef, CompilerError> {
        let Some(decls) = self.ws.functions_by_name.get(&Ustr::from(name)) else {
            return Err(gap(format!("stdlib `{name}` not found")));
        };
        let serving: Vec<FuncRef> = decls
            .iter()
            .filter(|(module, func)| {
                serves_target(
                    self.ws.modules[module.index()].functions[*func].backend,
                    self.scx.target,
                )
            })
            .map(|(module, func)| FuncRef { module: *module, func: *func })
            .collect();
        match serving.as_slice() {
            [one] => {
                // Stage-0e: a SYNTHESIZED call. It has no call expr, so it can
                // never appear in `call_generic_bindings` and never passes
                // through `call_spec` -- which is why round 1 saw none of these
                // and wrongly read the population as absent. A generic entry
                // reached this way has no instantiation record at all.
                if crate::ablation::enabled() {
                    let f = &self.ws.module(one.module).functions[one.func];
                    if !f.generics.is_empty() {
                        crate::ablation::note_examined();
                        crate::ablation::record(
                            crate::ablation::FatalCategory::MissingGraphEdge,
                            f.name.as_str(),
                            self.f.name.as_str(),
                            f.name_span,
                            "a COMPILER-SYNTHESIZED stdlib call (`stdlib_funcref`/`call_spine`, \
                             the column intercept): it has no call expr, so no \
                             `call_generic_bindings` entry can exist and the generic entry \
                             reaches its shared body with no instantiation record",
                        );
                    }
                }
                Ok(*one)
            }
            [] => Err(gap(format!(
                "stdlib `{name}` has no body serving the {:?} backend",
                self.scx.target
            ))),
            _ => Err(gap(format!("stdlib `{name}` is ambiguous"))),
        }
    }

    /// Column-aware lowering of `__vec_data_get/_set/_unset` when the receiver's
    /// element type is concretely column-stored. `Ok(None)` = not one of these, or
    /// a boxed element, in which case the stdlib `inline(lua)` body stands.
    ///
    /// `unset` is deliberately a NO-OP. On the boxed backing it exists to drop the
    /// reference so the element can be collected; a column holds scalars, so there
    /// is nothing to release, and nothing reads past `len` (every column access
    /// goes through `__vec_checked_index`, which bounds-checks against it). Writing
    /// nil into the columns would also be actively harmful: it punches holes in
    /// dense numeric arrays and can push them from array to hash part, which is the
    /// representation's whole advantage.
    pub(crate) fn column_data_primitive(
        &mut self,
        name: &str,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<Option<ValueRepr>, CompilerError> {
        match (name, arg_idxs) {
            ("__vec_data_get", [recv, index]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else { return Ok(None) };
                let vec = self.lower_expr(*recv)?;
                let i = self.lower_expr(*index)?;
                Ok(Some(self.column_element_repr(vec, i, elem)?.into_value()))
            }
            ("__vec_data_set", [recv, index, value]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else { return Ok(None) };
                let vec = self.lower_expr(*recv)?;
                let i = self.lower_expr(*index)?;
                self.column_element_store(vec, i, *value, elem)?;
                Ok(Some(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)))))
            }
            ("__vec_data_unset", [recv, _index]) => {
                let Some(_) = self.vec_column_elem_of(*recv)? else { return Ok(None) };
                Ok(Some(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)))))
            }
            ("__vec_data_capacity", [recv]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else { return Ok(None) };
                let shape = self.column_shape(elem)?;
                let vec = self.lower_expr(*recv)?;
                let header = self.column_header(vec);
                let reserved = self.emit_cur(Inst::Field {
                    base: header,
                    field: Ustr::from("cap"),
                });
                // The PRESENCE column when there is one: it holds a real bool for
                // every slot, where a leaf column of an absent element holds nil,
                // and `#` over a table with holes is undefined in Lua.
                let physical = match column_leaf_paths(&shape).first() {
                    Some(path) => {
                        let col = self.emit_cur(Inst::Field {
                            base: vec,
                            field: *path,
                        });
                        self.call_spine("table_len", vec![col])?
                    }
                    None => self.emit_cur(Inst::Field {
                        base: vec,
                        field: Ustr::from("len"),
                    }),
                };
                Ok(Some(ValueRepr::Scalar(
                    self.call_spine("__rock_col_capacity", vec![reserved, physical])?,
                )))
            }
            // Shift every column at the same slot, so they stay in lockstep. The
            // element is consumed as LEAVES, so the inserted value never boxes.
            ("__vec_data_insert", [recv, index, value]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else { return Ok(None) };
                let shape = self.column_shape(elem)?;
                let paths = column_leaf_paths(&shape);
                let vec = self.lower_expr(*recv)?;
                let i = self.lower_expr(*index)?;
                let repr = self.lower_expr_repr(*value)?;
                let leaves = self.repr_to_leaves_bundle(&repr)?;
                if leaves.len() != paths.len() {
                    return Err(gap(format!(
                        "column insert arity mismatch for [{}]",
                        elem.name
                    )));
                }
                let len = self.emit_cur(Inst::Field { base: vec, field: Ustr::from("len") });
                for (path, leaf) in paths.into_iter().zip(leaves) {
                    let col = self.emit_cur(Inst::Field { base: vec, field: path });
                    self.call_spine("__rock_col_insert", vec![col, i, len, leaf])?;
                }
                Ok(Some(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)))))
            }
            // A fresh column table whose columns are copies -- the clone must not
            // share a column with the original, or a later push would be seen by
            // both.
            ("__vec_data_clone", [recv]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else { return Ok(None) };
                let shape = self.column_shape(elem)?;
                let paths = column_leaf_paths(&shape);
                let vec = self.lower_expr(*recv)?;

                // Built as a `Vec`-tagged struct: `write_table_key` brackets a
                // dotted leaf path, so the emitted table is the same shape
                // `MakeVecColumns` writes for a literal.
                let mut fields = Vec::with_capacity(paths.len() + 2);
                let len = self.emit_cur(Inst::Field { base: vec, field: Ustr::from("len") });
                for path in paths {
                    let col = self.emit_cur(Inst::Field { base: vec, field: path });
                    let copied = self.call_spine("__rock_col_copy", vec![col, len])?;
                    fields.push((path, copied));
                }
                fields.push((Ustr::from("len"), len));
                let old_header = self.column_header(vec);
                let capacity = self.emit_cur(Inst::Field {
                    base: old_header,
                    field: Ustr::from("cap"),
                });
                let header = self.make_column_header(elem, capacity);
                fields.push((Ustr::from(VEC_COLUMN_HEADER_FIELD), header));
                Ok(Some(ValueRepr::Scalar(self.emit_cur(Inst::MakeStruct {
                    ty: Some(Ustr::from("Vec")),
                    fields,
                }))))
            }
            // The two ops that need the LOGICAL elements, not the physical
            // columns: a sort key is a function of the whole element, and
            // `to_lua_array`'s whole contract is handing a Lua array of elements
            // to a foreign API. `to_lua_array` redirects to the materializing
            // helper -- the storage contract's third category (erased/external
            // boundaries materialize explicitly), and the only place a column Vec
            // builds element tables.
            //
            // `sort` needs the element only to APPLY THE KEY, and the key is
            // reached through a fn value, whose ABI is derived from the fn type.
            // At a `nobox` element that ABI is the leaves, which are the columns,
            // so the key applies with no element at all and the two forms split
            // here rather than at a box the mark would forbid.
            ("__vec_data_sort", [recv, key_fn]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else { return Ok(None) };
                let vec = self.lower_expr(*recv)?;
                let k = self.lower_expr(*key_fn)?;
                if !elem.is_nobox() {
                    return Ok(Some(ValueRepr::Scalar(
                        self.call_spine("__rock_col_sort", vec![vec, k])?,
                    )));
                }
                let shape = self.column_shape(elem)?;
                // The key fn's ABI at a nullable element leads with presence, so
                // the path list must too -- it is the positional binding the
                // helper unpacks into the key.
                let paths: Vec<_> = column_leaf_paths(&shape)
                    .into_iter()
                    .map(|p| self.emit_cur(Inst::Const(Lit::Str(p))))
                    .collect();
                let paths = self.emit_cur(Inst::MakeTuple(paths));
                Ok(Some(ValueRepr::Scalar(
                    self.call_spine("__rock_col_sort_leaves", vec![vec, k, paths])?,
                )))
            }
            ("__rock_vec_to_lua_array", [recv]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else { return Ok(None) };
                if elem.is_nobox() {
                    return Err(CompilerError::new_with_label(
                        self.body.expr_spans[*recv],
                        format!(
                            "cannot convert column-stored nobox value `{}` to a Lua array: \
                             the bridge requires implicit element materialization",
                            elem.name
                        ),
                    ));
                }
                let vec = self.lower_expr(*recv)?;
                Ok(Some(ValueRepr::Scalar(
                    self.call_spine("__rock_vec_column_elements", vec![vec])?,
                )))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn make_column_header(&mut self, elem: ColumnElem, capacity: Idx<Inst>) -> Idx<Inst> {
        self.emit_cur(Inst::MakeVecColumnHeader {
            elem: elem.name,
            representation: elem.representation(),
            nobox: elem.is_nobox(),
            capacity,
        })
    }

    pub(crate) fn column_header(&mut self, vec: Idx<Inst>) -> Idx<Inst> {
        self.emit_cur(Inst::Field {
            base: vec,
            field: Ustr::from(VEC_COLUMN_HEADER_FIELD),
        })
    }

    pub(crate) fn empty_column_vec(
        &mut self,
        elem: ColumnElem,
        capacity: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let shape = self.column_shape(elem)?;
        let columns = column_leaf_paths(&shape).into_iter().map(|p| (p, Vec::new())).collect();
        Ok(self.emit_cur(Inst::MakeVecColumns {
            elem: elem.name,
            representation: elem.representation(),
            nobox: elem.is_nobox(),
            columns,
            len: 0,
            capacity,
        }))
    }

    /// A fresh column-stored `Map<K, V>`: a hash spine of slot indices over an
    /// empty column Vec, plus the free list and the live count. Arena's
    /// bookkeeping-SoA field names deliberately -- the two are one structure
    /// (`docs/unfuck-backlog.md`).
    ///
    /// The `data` columns must exist before the first store, which is why this
    /// mints a zero-length `MakeVecColumns` rather than an empty table.
    pub(crate) fn empty_column_map(&mut self, elem: ColumnElem) -> Result<Idx<Inst>, CompilerError> {
        let zero_cap = self.emit_cur(Inst::Const(Lit::Int(0)));
        let data = self.empty_column_vec(elem, zero_cap)?;
        // Through the helper, not a bare `MakeStruct`: the spine's key space is
        // user data and must not inherit host object-model names, which an
        // object literal does on JS. See `__map_col_new_spine`.
        let spine = self.call_spine("__map_col_new_spine", Vec::new())?;
        let free = self.emit_cur(Inst::MakeVec(Vec::new()));
        let count = self.emit_cur(Inst::Const(Lit::Int(0)));
        Ok(self.emit_cur(Inst::MakeStruct {
            ty: None,
            fields: vec![
                (Ustr::from("spine"), spine),
                (Ustr::from("data"), data),
                (Ustr::from("free_indices"), free),
                (Ustr::from("occupied_count"), count),
            ],
        }))
    }

    /// Column-aware lowering of the Map storage boundary when the receiver's
    /// VALUE type is concretely column-stored. `Ok(None)` = not one of these, or
    /// a boxed value, in which case the stdlib body stands.
    ///
    /// Resolves through the active `SpecSubst` for the same reason
    /// [`Self::column_data_primitive`] does: the shared snapshot of a generic
    /// body keeps the bare-table representation, the snapshot specialized at a
    /// column-eligible value lowers to spine operations. Static per
    /// instantiation, never a runtime test for whether a spine exists.
    pub(crate) fn map_column_primitive(
        &mut self,
        name: &str,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<Option<ValueRepr>, CompilerError> {
        match (name, arg_idxs) {
            ("get", [recv, key]) => {
                let Some(elem) = self.map_column_value_of(*recv)? else { return Ok(None) };
                let m = self.lower_expr(*recv)?;
                let k = self.lower_expr(*key)?;
                Ok(Some(self.map_column_get_repr(m, k, elem)?))
            }
            ("table_unset", [recv, key]) => {
                let Some(_) = self.map_column_value_of(*recv)? else { return Ok(None) };
                let m = self.lower_expr(*recv)?;
                let k = self.lower_expr(*key)?;
                self.call_spine("__map_col_free", vec![m, k])?;
                Ok(Some(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)))))
            }
            ("len" | "map_len", [recv]) => {
                let Some(_) = self.map_column_value_of(*recv)? else { return Ok(None) };
                let m = self.lower_expr(*recv)?;
                Ok(Some(ValueRepr::Scalar(self.call_spine("__map_col_len", vec![m])?)))
            }
            ("contains", [recv, key]) => {
                let Some(_) = self.map_column_value_of(*recv)? else { return Ok(None) };
                let m = self.lower_expr(*recv)?;
                let k = self.lower_expr(*key)?;
                Ok(Some(ValueRepr::Scalar(
                    self.call_spine("__map_col_contains", vec![m, k])?,
                )))
            }
            ("clear", [recv]) => {
                let Some(_) = self.map_column_value_of(*recv)? else { return Ok(None) };
                let m = self.lower_expr(*recv)?;
                self.call_spine("__map_col_clear", vec![m])?;
                Ok(Some(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)))))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn column_vec_from_raw_array(
        &mut self,
        raw: Idx<Inst>,
        len: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<Idx<Inst>, CompilerError> {
        let vec = self.empty_column_vec(elem, len)?;
        let i_cell = self.f.new_cell(None);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store { cell: i_cell, value: zero });

        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let cmp = self.emit_cur(Inst::IntLt(i, len));
        self.f.set_term(self.cur, Term::Branch { cond: cmp, then_bb: body, else_bb: exit });

        self.cur = body;
        let i_now = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let one_based = self.emit_cur(Inst::IntAdd(i_now, one));
        let value = self.emit_cur(Inst::Index {
            base: raw,
            index: one_based,
            kind: ContainerKind::VecColumn,
        });
        let bundle = ValueBundle::new(elem.representation(), ValueRepr::Scalar(value));
        self.column_append_bundle(vec, &bundle, elem)?;
        let next = self.emit_cur(Inst::IntAdd(i_now, one));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: next,
        });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(vec)
    }

    /// `xs.push(v)` onto a column-stored Vec: bump the shared `len` once, then
    /// store leaf `k` into column `k` at the new slot. The element is consumed
    /// as leaves (`repr_to_leaves`), so a literal argument never builds a box.
    pub(crate) fn column_push(
        &mut self,
        recv: Idx<Expr>,
        item: Idx<Expr>,
        elem: ColumnElem,
    ) -> Result<ValueRepr, CompilerError> {
        let shape = self.column_shape(elem)?;
        let vec = self.lower_expr(recv)?;
        let repr = self.lower_expr_repr(item)?;
        self.column_append_bundle_with_shape(vec, &repr, elem, &shape)?;
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))))
    }

    pub(crate) fn column_append_bundle(
        &mut self,
        vec: Idx<Inst>,
        bundle: &ValueBundle,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        let shape = self.column_shape(elem)?;
        let leaves = self.repr_to_storage_leaves(bundle, &shape, "column append")?;
        self.column_append_leaves(vec, leaves, elem, &shape)
    }

    pub(crate) fn column_append_bundle_with_shape(
        &mut self,
        vec: Idx<Inst>,
        bundle: &ValueBundle,
        elem: ColumnElem,
        shape: &ValueShape,
    ) -> Result<(), CompilerError> {
        let leaves = self.repr_to_storage_leaves(bundle, shape, "column append")?;
        self.column_append_leaves(vec, leaves, elem, shape)
    }

    fn column_append_leaves(
        &mut self,
        vec: Idx<Inst>,
        leaves: Vec<Idx<Inst>>,
        elem: ColumnElem,
        shape: &ValueShape,
    ) -> Result<(), CompilerError> {
        let paths = column_leaf_paths(shape);
        if leaves.len() != paths.len() {
            return Err(gap(format!(
                "column push arity mismatch for [{}]: {} leaves vs {} columns",
                elem.name,
                leaves.len(),
                paths.len()
            )));
        }
        // `len` is shared by every column, so it advances ONCE. Bumping per
        // column would desync them.
        let len = self.emit_cur(Inst::Field { base: vec, field: Ustr::from("len") });
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let next = self.emit_cur(Inst::IntAdd(len, one));
        // A column indexes the way its backend's `_data` does -- 1-based on
        // luajit, 0-based on JS -- so the appended slot is the NEW length on one
        // and the OLD length on the other. The read side gets this from
        // `__vec_checked_index`, whose two implementations differ by the same
        // one; this is the write side of that single decision.
        let slot = if self.column_base_is_one() { next } else { len };
        self.emit_cur(Inst::StoreField {
            base: vec,
            field: Ustr::from("len"),
            value: next,
        });
        for (path, leaf) in paths.into_iter().zip(leaves) {
            let col = self.emit_cur(Inst::Field { base: vec, field: path });
            self.emit_cur(Inst::StoreIndex {
                base: col,
                index: slot,
                value: leaf,
                kind: ContainerKind::VecColumn,
            });
        }
        Ok(())
    }

    /// `Some((vec, slot, elem))` iff `e` is `xs[i]` over a
    /// column-stored Vec — the shape that has no element slot to serve as a
    /// place base.
    pub(crate) fn column_index_place(
        &mut self,
        e: Idx<Expr>,
    ) -> Result<Option<(Idx<Inst>, Idx<Inst>, ColumnElem)>, CompilerError> {
        // Reached from a PLACE path, which never lowers `e` as a value, so the
        // cursor names whatever encloses the assignment. The walk keys this
        // obligation on `e` itself.
        self.with_site_expr(e, |s| s.column_index_place_at(e))
    }

    fn column_index_place_at(
        &mut self,
        e: Idx<Expr>,
    ) -> Result<Option<(Idx<Inst>, Idx<Inst>, ColumnElem)>, CompilerError> {
        let Expr::Index { base, index } = self.body.expr_arena[e] else {
            return Ok(None);
        };
        // A column-stored Map resolves to the SAME place one indirection out:
        // the spine turns the key into a slot, and the slot indexes `data`.
        // Without this a field write through `m[k]` takes a whole-element place
        // base, which the wrapper has no slot for.
        if let Some(elem) = self.map_column_value_of(base)? {
            let m = self.lower_expr(base)?;
            let k = self.lower_expr(index)?;
            let slot = self.call_spine("__map_col_slot_checked", vec![m, k])?;
            let data = self.map_col_data(m);
            let at = self.checked_column_index(data, slot);
            return Ok(Some((data, at, elem)));
        }
        let Some(elem) = self.vec_column_elem_of(base)? else {
            return Ok(None);
        };
        let vec = self.lower_expr(base)?;
        let i = self.lower_expr(index)?;
        let slot = self.checked_column_index(vec, i);
        Ok(Some((vec, slot, elem)))
    }

    /// `xs[i].<field> = v` on a column-stored Vec: store into the column that
    /// holds that leaf. `field` must name a PRIMITIVE leaf — a nested value
    /// field (`xs[i].inner = ..`, `xs[i].inner.a = ..`) spans several columns
    /// and is rejected rather than silently writing one.
    pub(crate) fn column_leaf_store(
        &mut self,
        vec: Idx<Inst>,
        slot: Idx<Inst>,
        elem: ColumnElem,
        field: Ustr,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        // A NULLABLE element has no field place at all: bone rejects `.field` on
        // a `T?` before this, and writing one leaf of a slot whose presence says
        // "absent" would be a silent half-write. Reject rather than trust that.
        if elem.nullable {
            return Err(gap(format!(
                "cannot write `{field}` through an element of `[{}?]`; refine the \
                 element to non-null and store it whole",
                elem.name
            )));
        }
        let shape = self.column_shape(elem)?;
        if !value_leaf_paths(&shape).iter().any(|p| *p == field) {
            return Err(gap(format!(
                "`{field}` is not a scalar leaf of column-stored `[{}]`; a \
                 nested value field spans several columns and needs a whole-element store",
                elem.name
            )));
        }
        let v = self.lower_expr(value)?;
        let col = self.emit_cur(Inst::Field { base: vec, field });
        self.emit_cur(Inst::StoreIndex {
            base: col,
            index: slot,
            value: v,
            kind: ContainerKind::VecColumn,
        });
        Ok(())
    }

    /// `xs[i] = v` into a column-stored Vec: bounds-check once, then store leaf
    /// `k` into column `k`. The element never materializes.
    pub(crate) fn column_element_store(
        &mut self,
        vec: Idx<Inst>,
        index: Idx<Inst>,
        value: Idx<Expr>,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        let repr = self.lower_expr_repr(value)?;
        self.column_element_store_bundle(vec, index, &repr, elem)
    }


    /// A synthesized call to a named runtime entry (`docs/boundary-contracts.md`
    /// S 1.4): the column-storage spine helpers and the `NativeBuffer` bounds
    /// check. Non-generic by necessity -- `Inst::Call` carries no generic
    /// arguments, so a generic Rock helper could never specialize here.
    ///
    /// ONE entry point for every synthesized runtime call. `name` is
    /// `&'static str` because it IS the site's operation identity
    /// (`SiteOp::Synthesized`): the call belongs to the enclosing expression but
    /// is not that expression's own callee, and stamping it `DirectCall` makes
    /// it collide with the expression's real callee -- which the join then
    /// reports as a callee mismatch. Every caller already passes a literal.
    pub(crate) fn call_spine(
        &mut self,
        name: &'static str,
        args: Vec<Idx<Inst>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let func = self.stdlib_funcref(name)?;
        Ok(self.with_site_op(crate::site::SiteOp::Synthesized(name), |b| {
            b.emit_cur(Inst::Call { func, args })
        }))
    }

    /// A call synthesized on a CONDITIONAL dispatch arm -- an interpolation
    /// hole's render or structural equality (`skull_mir::lower::dispatch`).
    ///
    /// Separate from [`Self::call_spine`] because the site identity differs:
    /// these live at an expression that names no callee, so several of them can
    /// share one expression AND one name (a nobox value renders per leaf field)
    /// and the `path` is what keeps them apart.
    pub(crate) fn dispatched_call(
        &mut self,
        name: &'static str,
        path: Vec<u32>,
        args: Vec<Idx<Inst>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let func = super::dispatch::dispatched_callee_decl(self.ws, name)
            .ok_or_else(|| gap(format!("dispatch helper `{name}` is missing or ambiguous")))?;
        let expr = self
            .cur_expr
            .ok_or_else(|| gap(format!("dispatch helper `{name}` has no source expression")))?;
        self.with_site_op(crate::site::SiteOp::Dispatched { name, path }, |b| {
            let direct = b.plan_direct_call(expr, func)?;
            let call = b.emit_cur(Inst::Call { func, args });
            b.record_call(call, func, &direct)?;
            Ok(call)
        })
    }

    /// `m[k] = v` into a column-stored Map: take (or allocate) the key's slot,
    /// then store leaf-wise into `data`'s columns. The value never materializes.
    ///
    /// A NULLABLE value collapses to key removal when absent. Rock's Map already
    /// cannot distinguish an absent key from a key holding null -- a nil store
    /// removes the key -- so nothing writes a top-level presence tag, and the
    /// `[P?]` presence-column gap is not on this path.
    pub(crate) fn map_column_store(
        &mut self,
        m: Idx<Inst>,
        key: Idx<Inst>,
        value: Idx<Expr>,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        let bundle = self.lower_expr_repr(value)?;
        let repr = bundle.value();
        // A SCALAR here is a nullable value that never became a Group -- a literal
        // `null`, or a boxed source. Its presence is a runtime nil test, and the
        // leaves come from field reads off the box, which is why `boxed` defers
        // the rebuild to the PRESENT arm below: reading `s.x` unconditionally
        // faults on the literal-null store.
        let (present, boxed) = match &repr {
            ValueRepr::Group { present, .. } => (*present, None),
            ValueRepr::Scalar(s) => {
                let s = *s;
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                (Some(self.emit_cur(Inst::NotEq(s, nil))), Some(s))
            }
            _ => (None, None),
        };
        let Some(present) = present else {
            let slot = self.call_spine("__map_col_slot", vec![m, key])?;
            let data = self.map_col_data(m);
            return self.column_element_store_bundle(data, slot, &bundle, elem);
        };
        let set_bb = self.f.new_block();
        let del_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch { cond: present, then_bb: set_bb, else_bb: del_bb },
        );
        self.cur = set_bb;
        let stored = match boxed {
            Some(s) => ValueBundle::new(
                elem.representation(),
                self.boxed_value_copy_repr(s, elem.name)?,
            ),
            None => bundle.clone(),
        };
        let slot = self.call_spine("__map_col_slot", vec![m, key])?;
        let data = self.map_col_data(m);
        self.column_element_store_bundle(data, slot, &stored, elem)?;
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = del_bb;
        self.call_spine("__map_col_free", vec![m, key])?;
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        Ok(())
    }

    /// `m[k]` on a column-stored Map -- errors on a miss, like the boxed
    /// `__map_get_checked` it replaces.
    pub(crate) fn map_column_index_repr(
        &mut self,
        m: Idx<Inst>,
        key: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<ValueRepr, CompilerError> {
        let slot = self.call_spine("__map_col_slot_checked", vec![m, key])?;
        let data = self.map_col_data(m);
        Ok(self.column_element_repr(data, slot, elem)?.into_value())
    }

    /// `m.get(k)` on a column-stored Map: a `V?` whose presence is the SPINE.
    ///
    /// The columns are read only in the present arm -- at a missing key the slot
    /// is -1 and indexing `data` would fault, and a freed slot still holds stale
    /// leaves that must not be observable.
    ///
    /// Which repr the miss arm has to produce is decided by how `V?` is
    /// represented OUTSIDE this expression, and that is `is_nobox_struct` -- the
    /// one predicate every representation decision reaches. Column ELIGIBILITY
    /// is a storage rule (`vec_column_elem`) and says nothing about it: an
    /// unmarked `value` struct is stored in columns here and is a boxed
    /// nil-or-cdata everywhere else. So an unmarked V builds its ctor INSIDE the
    /// present arm and misses as a plain `nil`; handing its boxed consumers the
    /// tagged Group instead makes them pack a ctor after the join, over the miss
    /// arm's nil leaves.
    pub(crate) fn map_column_get_repr(
        &mut self,
        m: Idx<Inst>,
        key: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<ValueRepr, CompilerError> {
        let shape = self.column_shape(elem)?;
        let unboxed = elem.is_nobox();
        let slot = self.call_spine("__map_col_slot_of", vec![m, key])?;
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        let present = self.emit_cur(Inst::IntGeq(slot, zero));
        let cell_count = if unboxed { value_leaf_paths(&shape).len() } else { 1 };
        let cells: Vec<Idx<Cell>> =
            (0..cell_count).map(|_| self.f.new_cell(None)).collect();
        let hit = self.f.new_block();
        let miss = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch { cond: present, then_bb: hit, else_bb: miss },
        );
        self.cur = hit;
        let data = self.map_col_data(m);
        let found = self.column_element_repr(data, slot, elem)?;
        let stored: Vec<Idx<Inst>> = if unboxed {
            self.repr_to_leaves_bundle(&found)?
        } else {
            vec![self.pack(found.into_value())?]
        };
        for (cell, value) in cells.iter().zip(stored) {
            self.emit_cur(Inst::Store { cell: *cell, value });
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = miss;
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        for cell in &cells {
            self.emit_cur(Inst::Store { cell: *cell, value: nil });
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        let loaded: Vec<Idx<Inst>> =
            cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        if !unboxed {
            return Ok(ValueRepr::Scalar(loaded[0]));
        }
        let mut idx = 0;
        let inner = self.rebuild_from_leaves(&shape, &loaded, &mut idx);
        Ok(match inner {
            ValueRepr::Group { name, parts, .. } => ValueRepr::Group {
                name,
                parts,
                present: Some(present),
            },
            other => other,
        })
    }

    /// [`Self::column_element_store`] from an already-lowered repr, for a caller
    /// that had to inspect the value's presence before choosing a block to store
    /// in (the column Map's nullable arm).
    pub(crate) fn column_element_store_bundle(
        &mut self,
        vec: Idx<Inst>,
        index: Idx<Inst>,
        bundle: &ValueBundle,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        let shape = self.column_shape(elem)?;
        let leaves = self.repr_to_storage_leaves(bundle, &shape, "column store")?;
        let paths = column_leaf_paths(&shape);
        if leaves.len() != paths.len() {
            return Err(gap(format!(
                "column store arity mismatch for [{}]: {} leaves vs {} columns",
                elem.name,
                leaves.len(),
                paths.len()
            )));
        }
        let slot = self.checked_column_index(vec, index);
        for (path, leaf) in paths.into_iter().zip(leaves) {
            let col = self.emit_cur(Inst::Field { base: vec, field: path });
            self.emit_cur(Inst::StoreIndex {
                base: col,
                index: slot,
                value: leaf,
                kind: ContainerKind::VecColumn,
            });
        }
        Ok(())
    }

    /// `__vec_checked_index(vec, i)` — the backing slot index, bounds-checked
    /// against `vec.len`. One call per ELEMENT access, not per column.
    pub(crate) fn checked_column_index(&mut self, vec: Idx<Inst>, index: Idx<Inst>) -> Idx<Inst> {
        self.emit_cur(Inst::VecCheckedIndex { vec, index })
    }

    /// Whether a leaf column is 1-based on the target. It follows that backend's
    /// `_data`: luajit's is 1-based, JS's is 0-based. JS cannot simply adopt the
    /// 1-based form to share one rule -- leaving index 0 empty makes every column
    /// a HOLEY array in V8, which forfeits the packed-double fast path the whole
    /// layout is measured against (`docs/js-vec-columns-plan.md`).
    pub(crate) fn column_base_is_one(&self) -> bool {
        self.scx.target != bone_semantic::hir::FnBackend::Js
    }
}
