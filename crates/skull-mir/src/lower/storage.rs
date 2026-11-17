//! Element STORAGE lowering: native-buffer leaf addressing, and the column form
//! of a vec / map whose element is a `nobox` value (header, push, append, index,
//! per-leaf store, and the presence column beside the leaf columns).
//!
//! Column eligibility itself is decided in `shape` -- this is the code that
//! builds and reads the layout that module describes.
use super::*;

/// Every stdlib entry [`Builder::call_spine`] may call. Builds root them
/// instead of predicting which sites lowering will synthesize, so a call
/// resolves by name wherever lowering decides to emit it.
pub const SYNTHESIZED_ENTRIES: &[&str] = &[
    "__arena_store_or_empty",
    "__descriptor_with",
    "__map_col_clear",
    "__map_col_contains",
    "__map_col_free",
    "__map_col_len",
    "__map_col_new_spine",
    "__map_col_slot",
    "__map_col_slot_checked",
    "__map_col_slot_of",
    "__nb_checked_index",
    "__rock_col_capacity",
    "__rock_col_copy",
    "__rock_col_insert",
    "__rock_col_sort",
    "__rock_col_sort_leaves",
    "__rock_stride_capacity",
    "__rock_stride_copy",
    "__rock_stride_insert",
    "__rock_stride_reserve",
    "__rock_stride_sort",
    "__rock_stride_sort_leaves",
    "__rock_vec_column_elements",
    "__rock_vec_strided_elements",
    "table_len",
];

impl<'a> Builder<'a> {
    pub(crate) fn clone_column_data(
        &mut self,
        vec: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<ValueRepr, CompilerError> {
        let shape = self.column_shape(elem)?;
        let paths = column_leaf_paths(&shape);

        // Built as a `Vec`-tagged struct: `write_table_key` brackets a
        // dotted leaf path, so the emitted table is the same shape
        // `MakeVecColumns` writes for a literal.
        let mut fields = Vec::with_capacity(paths.len() + 2);
        let len = self.read_field(vec, crate::names::ustr("len"));
        for path in paths {
            let col = self.read_field(vec, path);
            let copied = self.call_spine("__rock_col_copy", vec![col, len])?;
            fields.push((path, copied));
        }
        fields.push((crate::names::ustr("len"), len));
        let old_header = self.column_header(vec);
        let capacity = self.read_field(old_header, crate::names::ustr("cap"));
        let header = self.make_column_header(elem, capacity);
        fields.push((crate::names::ustr(VEC_COLUMN_HEADER_FIELD), header));
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::MakeStruct {
            ty: Some(crate::names::ustr("Vec")),
            fields,
        })))
    }

    pub(crate) fn clone_strided_data(
        &mut self,
        vec: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<ValueRepr, CompilerError> {
        let stride = self.column_shape(elem)?.leaf_count();
        let data = self.read_field(vec, crate::names::ustr("_data"));
        let len = self.read_field(vec, crate::names::ustr("len"));
        let width = self.emit_cur(Inst::Const(Lit::Int(stride as i64)));
        let copied = self.call_spine("__rock_stride_copy", vec![data, len, width])?;
        let old_header = self.column_header(vec);
        let capacity = self.read_field(old_header, crate::names::ustr("cap"));
        let header = self.make_strided_header(elem, stride, capacity);
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::MakeStruct {
            ty: Some(crate::names::ustr("Vec")),
            fields: vec![
                (crate::names::ustr("_data"), copied),
                (crate::names::ustr("len"), len),
                (crate::names::ustr(VEC_COLUMN_HEADER_FIELD), header),
            ],
        })))
    }

    pub(crate) fn strided_element_repr(
        &mut self,
        vec: Idx<Inst>,
        index: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<ValueBundle, CompilerError> {
        let shape = self.column_shape(elem)?;
        let leaves = self.strided_element_leaves(vec, index, elem, &shape, 0..shape.leaf_count());
        let mut idx = 0;
        Ok(ValueBundle::new(
            elem.representation(),
            self.rebuild_from_leaves(&shape, &leaves, &mut idx),
        ))
    }

    pub(crate) fn strided_element_leaves(
        &mut self,
        vec: Idx<Inst>,
        index: Idx<Inst>,
        elem: ColumnElem,
        shape: &ValueShape,
        range: std::ops::Range<usize>,
    ) -> Vec<Idx<Inst>> {
        let stride = shape.leaf_count();
        let raw = self.strided_stores_raw(elem);
        let slot = self.checked_column_index(vec, index);
        let data = self.read_field(vec, crate::names::ustr("_data"));
        range
            .map(|offset| {
                let at = self.strided_slot_index(slot, stride, offset);
                let stored = self.read_index(data, at, ContainerKind::VecStrideSlot);
                if raw {
                    stored
                } else {
                    self.emit_cur(Inst::VecStrideFalseToNil(stored))
                }
            })
            .collect()
    }

    pub(crate) fn strided_element_store(
        &mut self,
        vec: Idx<Inst>,
        index: Idx<Inst>,
        value: Idx<Expr>,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        let bundle = self.lower_expr_repr(value)?;
        self.strided_element_store_bundle(vec, index, &bundle, elem)
    }

    pub(crate) fn strided_element_store_bundle(
        &mut self,
        vec: Idx<Inst>,
        index: Idx<Inst>,
        bundle: &ValueBundle,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        let shape = self.column_shape(elem)?;
        let stride = shape.leaf_count();
        let leaves = self.element_storage_leaves(bundle, &shape, stride, elem, "strided store")?;
        let slot = self.checked_column_index(vec, index);
        let row = self.strided_row_of(slot);
        self.strided_store_leaves_at(vec, row, leaves, stride, elem);
        Ok(())
    }

    /// Fold small rows' offsets into their stores, capped at 12: each distinct
    /// offset can cost a KNUM plus a KINT (three of LuaJIT's 500 constant slots)
    /// on x86. Larger callers already spend most slots on keys and callees.
    /// Wider rows advance one index so their leaves add no offset constants.
    fn strided_store_leaves_at(
        &mut self,
        vec: Idx<Inst>,
        row: Idx<Inst>,
        leaves: Vec<Idx<Inst>>,
        stride: usize,
        elem: ColumnElem,
    ) {
        let raw = self.strided_stores_raw(elem);
        let data = self.read_field(vec, crate::names::ustr("_data"));
        let compact = stride <= 12;
        let width = self.emit_cur(Inst::Const(Lit::Int(stride as i64)));
        let base = self.emit_cur(Inst::IntMul(row, width));
        let mut at = if !compact && self.column_base_is_one() {
            self.int_plus_one(base)
        } else {
            base
        };
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        for (offset, leaf) in leaves.into_iter().enumerate() {
            if compact {
                let physical = offset + usize::from(self.column_base_is_one());
                at = if physical == 0 {
                    base
                } else {
                    let offset = self.emit_cur(Inst::Const(Lit::Int(physical as i64)));
                    self.emit_cur(Inst::IntAdd(base, offset))
                };
            } else if offset > 0 {
                at = self.emit_cur(Inst::IntAdd(at, one));
            }
            let value = if raw {
                leaf
            } else {
                self.emit_cur(Inst::VecStrideNilToFalse(leaf))
            };
            self.write_index(data, at, value, ContainerKind::VecStrideSlot);
        }
    }

    /// A wide `nobox` struct row keeps every leaf as stored: its leaves may be
    /// `false`, so the flat-enum tier's nil-to-false densifying would corrupt
    /// them. Enum rows are numeric by eligibility and keep the sentinel.
    pub(crate) fn strided_stores_raw(&self, elem: ColumnElem) -> bool {
        self.ws.struct_by_name.contains_key(&elem.name)
    }

    /// The 0-based row a checked (backend-based) element index names.
    fn strided_row_of(&mut self, checked: Idx<Inst>) -> Idx<Inst> {
        if self.column_base_is_one() {
            let one = self.emit_cur(Inst::Const(Lit::Int(1)));
            self.emit_cur(Inst::IntSub(checked, one))
        } else {
            checked
        }
    }

    pub(crate) fn strided_slot_index(
        &mut self,
        checked: Idx<Inst>,
        stride: usize,
        offset: usize,
    ) -> Idx<Inst> {
        let row = self.strided_row_of(checked);
        self.strided_row_slot(row, stride, offset)
    }

    /// The backing slot of leaf `offset` in 0-based `row`.
    fn strided_row_slot(&mut self, row: Idx<Inst>, stride: usize, offset: usize) -> Idx<Inst> {
        let width = self.emit_cur(Inst::Const(Lit::Int(stride as i64)));
        let base = self.emit_cur(Inst::IntMul(row, width));
        let physical = offset + usize::from(self.column_base_is_one());
        if physical == 0 {
            base
        } else {
            let offset = self.emit_cur(Inst::Const(Lit::Int(physical as i64)));
            self.emit_cur(Inst::IntAdd(base, offset))
        }
    }

    pub(crate) fn strided_push(
        &mut self,
        recv: Idx<Expr>,
        item: Idx<Expr>,
        elem: ColumnElem,
    ) -> Result<ValueRepr, CompilerError> {
        let vec = self.lower_expr(recv)?;
        let bundle = self.lower_expr_repr(item)?;
        self.strided_append_bundle(vec, &bundle, elem)?;
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))))
    }

    pub(crate) fn strided_append_bundle(
        &mut self,
        vec: Idx<Inst>,
        bundle: &ValueBundle,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        let shape = self.column_shape(elem)?;
        let stride = shape.leaf_count();
        let leaves = self.element_storage_leaves(bundle, &shape, stride, elem, "strided append")?;
        let len = self.read_field(vec, crate::names::ustr("len"));
        if self.scx.target() == bone_core::hir::FnBackend::Lua {
            self.call_spine("__rock_stride_reserve", vec![vec, len])?;
        }
        // The appended row is the old length, counted from 0.
        self.strided_store_leaves_at(vec, len, leaves, stride, elem);
        let next = self.int_plus_one(len);
        self.write_field(vec, crate::names::ustr("len"), next);
        Ok(())
    }

    pub(crate) fn empty_strided_vec(
        &mut self,
        elem: ColumnElem,
        capacity: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let stride = self.column_shape(elem)?.leaf_count();
        Ok(self.emit_cur(Inst::MakeVecStrided {
            elem: elem.name,
            representation: elem.representation(),
            nobox: elem.is_nobox(),
            stride,
            values: Vec::new(),
            len: 0,
            capacity,
        }))
    }

    fn make_strided_header(
        &mut self,
        elem: ColumnElem,
        stride: usize,
        capacity: Idx<Inst>,
    ) -> Idx<Inst> {
        self.emit_cur(Inst::MakeVecStridedHeader {
            elem: elem.name,
            representation: elem.representation(),
            nobox: elem.is_nobox(),
            stride,
            capacity,
        })
    }

    pub(crate) fn strided_data_primitive(
        &mut self,
        e: Idx<Expr>,
        name: &str,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<Option<ValueBundle>, CompilerError> {
        match (name, arg_idxs) {
            ("__vec_data_get", [recv, index]) => {
                let Some(elem) = self.vec_strided_elem_of(*recv)? else {
                    return Ok(None);
                };
                let vec = self.lower_expr(*recv)?;
                let index = self.lower_expr(*index)?;
                Ok(Some(self.strided_element_repr(vec, index, elem)?))
            }
            ("__vec_data_set", [recv, index, value]) => {
                let Some(elem) = self.vec_strided_elem_of(*recv)? else {
                    return Ok(None);
                };
                let vec = self.lower_expr(*recv)?;
                let index = self.lower_expr(*index)?;
                self.strided_element_store(vec, index, *value, elem)?;
                let value = ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)));
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("__vec_data_unset", [recv, index]) => {
                let Some(elem) = self.vec_strided_elem_of(*recv)? else {
                    return Ok(None);
                };
                let stride = self.column_shape(elem)?.leaf_count();
                // A raw row only has something to release when it can hold a
                // reference; stale scalars past `len` are never read.
                let raw = self.strided_stores_raw(elem);
                if raw && !self.column_contains_references(elem) {
                    let value = ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)));
                    return Ok(Some(self.value_bundle_for_expr(e, value)?));
                }
                let vec = self.lower_expr(*recv)?;
                let index = self.lower_expr(*index)?;
                let slot = self.checked_column_index(vec, index);
                let row = self.strided_row_of(slot);
                let vacated = if raw { Lit::Nil } else { Lit::Bool(false) };
                let leaves = (0..stride)
                    .map(|_| self.emit_cur(Inst::Const(vacated.clone())))
                    .collect();
                self.strided_store_leaves_at(vec, row, leaves, stride, elem);
                let value = ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)));
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("__vec_data_capacity", [recv]) => {
                let Some(elem) = self.vec_strided_elem_of(*recv)? else {
                    return Ok(None);
                };
                let stride = self.column_shape(elem)?.leaf_count();
                let vec = self.lower_expr(*recv)?;
                let header = self.column_header(vec);
                let reserved = self.read_field(header, crate::names::ustr("cap"));
                let width = self.emit_cur(Inst::Const(Lit::Int(stride as i64)));
                // A raw row may hold nil, so `#_data` is only some border.
                let physical = if self.strided_stores_raw(elem) {
                    let len = self.read_field(vec, crate::names::ustr("len"));
                    self.emit_cur(Inst::IntMul(len, width))
                } else {
                    let data = self.read_field(vec, crate::names::ustr("_data"));
                    self.call_spine("table_len", vec![data])?
                };
                let value = ValueRepr::Scalar(
                    self.call_spine("__rock_stride_capacity", vec![reserved, physical, width])?,
                );
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("__vec_data_insert", [recv, index, value]) => {
                let Some(elem) = self.vec_strided_elem_of(*recv)? else {
                    return Ok(None);
                };
                let shape = self.column_shape(elem)?;
                let stride = shape.leaf_count();
                let vec = self.lower_expr(*recv)?;
                let index = self.lower_expr(*index)?;
                let bundle = self.lower_expr_repr(*value)?;
                let leaves = self.repr_to_storage_leaves(&bundle, &shape, "strided insert")?;
                let data = self.read_field(vec, crate::names::ustr("_data"));
                let len = self.read_field(vec, crate::names::ustr("len"));
                let width = self.emit_cur(Inst::Const(Lit::Int(stride as i64)));
                self.call_spine("__rock_stride_insert", vec![data, index, len, width])?;
                // `index` is the Lua-style (1-based) position on both backends.
                let one = self.emit_cur(Inst::Const(Lit::Int(1)));
                let row = self.emit_cur(Inst::IntSub(index, one));
                self.strided_store_leaves_at(vec, row, leaves, stride, elem);
                let value = ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)));
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("__vec_data_clone", [recv]) => {
                let Some(elem) = self.vec_strided_elem_of(*recv)? else {
                    return Ok(None);
                };
                let vec = self.lower_expr(*recv)?;
                let value = self.clone_strided_data(vec, elem)?;
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("__vec_data_sort", [recv, key_fn]) => {
                let Some(elem) = self.vec_strided_elem_of(*recv)? else {
                    return Ok(None);
                };
                let stride = self.column_shape(elem)?.leaf_count();
                let vec = self.lower_expr(*recv)?;
                let key = self.lower_expr(*key_fn)?;
                let width = self.emit_cur(Inst::Const(Lit::Int(stride as i64)));
                // A fn value taking a `nobox` element takes its leaves, which
                // are the row, so the key applies without an element existing.
                let helper = if elem.is_nobox() && self.strided_stores_raw(elem) {
                    "__rock_stride_sort_leaves"
                } else {
                    "__rock_stride_sort"
                };
                let value = ValueRepr::Scalar(self.call_spine(helper, vec![vec, key, width])?);
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("__rock_vec_to_lua_array", [recv]) => {
                let Some(elem) = self.vec_strided_elem_of(*recv)? else {
                    return Ok(None);
                };
                if elem.is_nobox() && self.strided_stores_raw(elem) {
                    return Err(CompilerError::new_with_label(
                        self.body.expr_spans[*recv],
                        format!(
                            "cannot convert strided nobox value `{}` to a Lua array: \
                             the bridge requires implicit element materialization",
                            elem.name
                        ),
                    ));
                }
                let vec = self.lower_expr(*recv)?;
                let value =
                    ValueRepr::Scalar(self.call_spine("__rock_vec_strided_elements", vec![vec])?);
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn strided_vec_from_raw_array(
        &mut self,
        raw: Idx<Inst>,
        len: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<Idx<Inst>, CompilerError> {
        let vec = self.empty_strided_vec(elem, len)?;
        let cell = self.f.new_cell(None);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.store_cell(cell, zero);
        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));
        self.cur = header;
        let i = self.emit_cur(Inst::Load(cell));
        let more = self.emit_cur(Inst::IntLt(i, len));
        self.branch_to(more, body, exit);
        self.cur = body;
        let i = self.emit_cur(Inst::Load(cell));
        let raw_index = if self.column_base_is_one() {
            self.int_plus_one(i)
        } else {
            i
        };
        let value = self.read_index(raw, raw_index, ContainerKind::VecStrideSlot);
        let bundle = if self.strided_stores_raw(elem) {
            ValueBundle::new(elem.representation(), ValueRepr::Scalar(value))
        } else {
            let shape = self.column_shape(elem)?;
            let leaves = self.cdata_leaves(value, &shape)?;
            let mut leaf = 0;
            let unpacked = self.rebuild_from_leaves(&shape, &leaves, &mut leaf);
            ValueBundle::new(elem.representation(), unpacked)
        };
        self.strided_append_bundle(vec, &bundle, elem)?;
        let next = self.int_plus_one(i);
        self.store_cell(cell, next);
        self.f.set_term(self.cur, Term::Goto(header));
        self.cur = exit;
        Ok(vec)
    }

    /// Resolve `base.field` inside a leaf-addressed NativeBuffer element. The
    /// returned dotted path selects its planned leaf; the index is checked once.
    pub(crate) fn nb_leaf_field_place(
        &mut self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<Option<(Idx<Inst>, Idx<Inst>, Ustr, String)>, CompilerError> {
        let (cur, mut segs) = self.field_chain(base, field);
        let Expr::Index { base: ib, index } = self.body.expr_arena[cur] else {
            return Ok(None);
        };
        let ContainerKind::NativeBufferValue(elem) = self.nb_kind(ib)? else {
            return Ok(None);
        };
        if !self.is_leaf_addressed(elem)? {
            return Ok(None);
        }
        let (b, _) = self.lower_index_base(ib)?;
        let i = self.lower_expr(index)?;
        // The bounds check belongs to the index, not the outer field access.
        let checked = self.with_site_expr(cur, |s| s.nb_checked_index(b, i))?;
        segs.reverse();
        let path = segs
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(".");
        Ok(Some((b, checked, elem, path)))
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
        let hit: Vec<LeafAddr> = plan
            .iter()
            .filter(|a| a.path.as_str() == path || a.path.as_str().starts_with(&dot))
            .cloned()
            .map(|address| container.place(address).map_err(|e| gap(e.message)))
            .collect::<Result<_, _>>()?;
        if hit.is_empty() {
            return Err(gap(format!(
                "`{path}` is not a leaf of NativeBuffer<{elem}>"
            )));
        }
        // An exact primitive leaf is the one hit whose path IS `path`. A NESTED
        // field spanning a single leaf also yields one hit, but its path is
        // `<path>.<leaf>` -- so discriminating on hit COUNT lowers that field's
        // aggregate RHS as a scalar and writes the whole cdata through a
        // primitive view. Everything that is not an exact leaf is an aggregate.
        let leaves = if hit.len() == 1 && hit[0].path.as_str() == path {
            vec![self.lower_expr(value)?]
        } else {
            let sub = match self.value_aggregate_of(value) {
                Some(n) => self.nominal_rep_shape(n, false)?,
                None => None,
            }
            .ok_or_else(|| {
                gap(format!(
                    "whole write to `{path}` needs a value-struct or cdata-enum RHS"
                ))
            })?;
            let repr = self.lower_expr_repr(value)?;
            self.repr_to_storage_leaves(&repr, &sub, "nested native buffer field store")?
        };
        if leaves.len() != hit.len() {
            return Err(gap(format!(
                "leaf store arity mismatch for `{elem}.{path}`: {} leaves vs {} slots",
                leaves.len(),
                hit.len()
            )));
        }
        let mut views = HashMap::default();
        for (a, leaf) in hit.iter().zip(leaves) {
            let at = self.leaf_index(checked, a);
            let view = self.leaf_view(buf, a.view_field, &mut views);
            self.write_index(view, at, leaf, ContainerKind::NativeBufferLeaf);
        }
        Ok(())
    }

    /// Whether `elem` has a validated C leaf layout. Otherwise the existing
    /// slot-reference path remains valid but boxier. Lua pointers and JS
    /// TypedArrays use the same planned C offsets and strides.
    fn native_buffer_plan(&self, elem: Ustr) -> Result<ContainerPlan, CompilerError> {
        let ty = MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(crate::names::ustr("NativeBuffer")),
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

    /// Resolve a single-leaf NativeBuffer read-modify-write place. JS needs this
    /// path because TypedArray elements are values; Lua's FFI element is an
    /// assignable struct reference. Aggregate paths have no scalar update.
    pub(crate) fn nb_leaf_rmw_place(
        &mut self,
        target: Idx<Expr>,
    ) -> Result<Option<(Idx<Inst>, Idx<Inst>, Ustr, LeafAddr)>, CompilerError> {
        if self.scx.target != bone_core::hir::FnBackend::Js {
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
        addr: &LeafAddr,
    ) -> Idx<Inst> {
        let at = self.leaf_index(checked, addr);
        let view = self.leaf_view(buf, addr.view_field, &mut HashMap::default());
        self.read_index(view, at, ContainerKind::NativeBufferLeaf)
    }

    /// The store half of [`Self::nb_leaf_rmw_place`].
    pub(crate) fn nb_leaf_store(
        &mut self,
        buf: Idx<Inst>,
        checked: Idx<Inst>,
        addr: &LeafAddr,
        value: Idx<Inst>,
    ) {
        let at = self.leaf_index(checked, addr);
        let view = self.leaf_view(buf, addr.view_field, &mut HashMap::default());
        self.write_index(view, at, value, ContainerKind::NativeBufferLeaf);
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
    ) -> Result<ValueBundle, CompilerError> {
        let container = self.native_buffer_plan(elem)?;
        let shape = container
            .element_shape(self.scx.rep_arena())
            .ok_or_else(|| gap("leaf-addressed element is not a value struct"))?;
        let plan: Vec<LeafAddr> = container
            .leaf_addresses()
            .ok_or_else(|| gap("leaf-addressed element has no owned leaf address plan"))?
            .iter()
            .cloned()
            .map(|address| container.place(address).map_err(|e| gap(e.message)))
            .collect::<Result<_, _>>()?;
        let checked = self.nb_checked_index(buf, index)?;
        let mut views = HashMap::default();
        let leaves: Vec<Idx<Inst>> = plan
            .iter()
            .map(|a| {
                let at = self.leaf_index(checked, a);
                let view = self.leaf_view(buf, a.view_field, &mut views);
                self.read_index(view, at, ContainerKind::NativeBufferLeaf)
            })
            .collect();
        let mut idx = 0;
        Ok(ValueBundle::new(
            container.element_rep(),
            self.rebuild_from_leaves(&shape, &leaves, &mut idx),
        ))
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
        let plan: Vec<LeafAddr> = container
            .leaf_addresses()
            .ok_or_else(|| gap("leaf-addressed element has no owned leaf address plan"))?
            .iter()
            .cloned()
            .map(|address| container.place(address).map_err(|e| gap(e.message)))
            .collect::<Result<_, _>>()?;
        let bundle = self.lower_expr_repr(value)?;
        let shape = container
            .element_shape(self.scx.rep_arena())
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
        let mut views = HashMap::default();
        for (a, leaf) in plan.iter().zip(leaves) {
            let at = self.leaf_index(checked, a);
            let view = self.leaf_view(buf, a.view_field, &mut views);
            self.write_index(view, at, leaf, ContainerKind::NativeBufferLeaf);
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
        let v = self.read_field(buf, field);
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
                let col = self.read_field(vec, path);
                self.read_index(col, slot, ContainerKind::VecColumn)
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
        let Some(decls) = self.ws.functions_by_name.get(&crate::names::ustr(name)) else {
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
            .map(|(module, func)| FuncRef {
                module: *module,
                func: *func,
            })
            .collect();
        match serving.as_slice() {
            [one] => Ok(*one),
            [] => Err(gap(format!(
                "stdlib `{name}` has no body serving the {:?} backend",
                self.scx.target
            ))),
            _ => Err(gap(format!("stdlib `{name}` is ambiguous"))),
        }
    }

    fn column_contains_references(&self, elem: ColumnElem) -> bool {
        self.scx
            .rep_arena()
            .get(elem.representation())
            .value_storage()
            == Some(ValueStorage::GcTable)
    }

    fn column_unset(
        &mut self,
        vec: Idx<Inst>,
        index: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        if !self.column_contains_references(elem) {
            return Ok(());
        }
        let shape = self.column_shape(elem)?;
        let slot = self.checked_column_index(vec, index);
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        for path in column_leaf_paths(&shape) {
            let col = self.read_field(vec, path);
            self.write_index(col, slot, nil, ContainerKind::VecColumn);
        }
        Ok(())
    }

    fn map_column_free(
        &mut self,
        map: Idx<Inst>,
        key: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        let slot = self.call_spine("__map_col_free", vec![map, key])?;
        if !self.column_contains_references(elem) {
            return Ok(());
        }
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        let present = self.emit_cur(Inst::IntGeq(slot, zero));
        let hit = self.f.new_block();
        let join = self.f.new_block();
        self.branch_to(present, hit, join);
        self.cur = hit;
        let data = self.map_col_data(map);
        self.column_unset(data, slot, elem)?;
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        Ok(())
    }

    fn column_release_all(
        &mut self,
        vec: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        if !self.column_contains_references(elem) {
            return Ok(());
        }
        let len = self.read_field(vec, crate::names::ustr("len"));
        let cell = self.f.new_cell(None);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.store_cell(cell, zero);
        let head = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(head));
        self.cur = head;
        let i = self.emit_cur(Inst::Load(cell));
        let more = self.emit_cur(Inst::IntLt(i, len));
        self.branch_to(more, body, exit);
        self.cur = body;
        self.column_unset(vec, i, elem)?;
        let next = self.int_plus_one(i);
        self.store_cell(cell, next);
        self.f.set_term(self.cur, Term::Goto(head));
        self.cur = exit;
        Ok(())
    }

    /// Column-aware lowering of `__vec_data_get/_set/_unset` when the receiver's
    /// element type is concretely column-stored. `Ok(None)` = not one of these, or
    /// a boxed element, in which case the stdlib `inline(lua)` body stands.
    pub(crate) fn column_data_primitive(
        &mut self,
        e: Idx<Expr>,
        name: &str,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<Option<ValueBundle>, CompilerError> {
        match (name, arg_idxs) {
            ("__vec_data_get", [recv, index]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else {
                    return Ok(None);
                };
                let vec = self.lower_expr(*recv)?;
                let i = self.lower_expr(*index)?;
                Ok(Some(self.column_element_repr(vec, i, elem)?))
            }
            ("__vec_data_set", [recv, index, value]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else {
                    return Ok(None);
                };
                let vec = self.lower_expr(*recv)?;
                let i = self.lower_expr(*index)?;
                self.column_element_store(vec, i, *value, elem)?;
                let value = ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)));
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("__vec_data_unset", [recv, index]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else {
                    return Ok(None);
                };
                if self.column_contains_references(elem) {
                    let vec = self.lower_expr(*recv)?;
                    let index = self.lower_expr(*index)?;
                    self.column_unset(vec, index, elem)?;
                }
                let value = ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)));
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("__vec_data_capacity", [recv]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else {
                    return Ok(None);
                };
                let shape = self.column_shape(elem)?;
                let vec = self.lower_expr(*recv)?;
                let header = self.column_header(vec);
                let reserved = self.read_field(header, crate::names::ustr("cap"));
                // Reference leaves can be nil even when the element is present.
                let physical = if self.column_contains_references(elem) {
                    self.read_field(vec, crate::names::ustr("len"))
                } else {
                    match column_leaf_paths(&shape).first() {
                        Some(path) => {
                            let col = self.read_field(vec, *path);
                            self.call_spine("table_len", vec![col])?
                        }
                        None => self.read_field(vec, crate::names::ustr("len")),
                    }
                };
                let value = ValueRepr::Scalar(
                    self.call_spine("__rock_col_capacity", vec![reserved, physical])?,
                );
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            // Shift every column at the same slot, so they stay in lockstep. The
            // element is consumed as LEAVES, so the inserted value never boxes.
            ("__vec_data_insert", [recv, index, value]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else {
                    return Ok(None);
                };
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
                let len = self.read_field(vec, crate::names::ustr("len"));
                for (path, leaf) in paths.into_iter().zip(leaves) {
                    let col = self.read_field(vec, path);
                    self.call_spine("__rock_col_insert", vec![col, i, len, leaf])?;
                }
                let value = ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)));
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            // A fresh column table whose columns are copies -- the clone must not
            // share a column with the original, or a later push would be seen by
            // both.
            ("__vec_data_clone", [recv]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else {
                    return Ok(None);
                };
                let vec = self.lower_expr(*recv)?;
                let value = self.clone_column_data(vec, elem)?;
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            // Sorting applies the key to the logical element; `to_lua_array`
            // explicitly materializes elements for a foreign API. A `nobox`
            // sort key uses its function-value ABI and reads the columns directly.
            ("__vec_data_sort", [recv, key_fn]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else {
                    return Ok(None);
                };
                let vec = self.lower_expr(*recv)?;
                let k = self.lower_expr(*key_fn)?;
                if !elem.is_nobox() {
                    let value =
                        ValueRepr::Scalar(self.call_spine("__rock_col_sort", vec![vec, k])?);
                    return Ok(Some(self.value_bundle_for_expr(e, value)?));
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
                let value = ValueRepr::Scalar(
                    self.call_spine("__rock_col_sort_leaves", vec![vec, k, paths])?,
                );
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("__rock_vec_to_lua_array", [recv]) => {
                let Some(elem) = self.vec_column_elem_of(*recv)? else {
                    return Ok(None);
                };
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
                let value =
                    ValueRepr::Scalar(self.call_spine("__rock_vec_column_elements", vec![vec])?);
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn make_column_header(
        &mut self,
        elem: ColumnElem,
        capacity: Idx<Inst>,
    ) -> Idx<Inst> {
        self.emit_cur(Inst::MakeVecColumnHeader {
            elem: elem.name,
            representation: elem.representation(),
            nobox: elem.is_nobox(),
            capacity,
        })
    }

    pub(crate) fn column_header(&mut self, vec: Idx<Inst>) -> Idx<Inst> {
        self.read_field(vec, crate::names::ustr(VEC_COLUMN_HEADER_FIELD))
    }

    pub(crate) fn empty_column_vec(
        &mut self,
        elem: ColumnElem,
        capacity: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let shape = self.column_shape(elem)?;
        let columns = column_leaf_paths(&shape)
            .into_iter()
            .map(|p| (p, Vec::new()))
            .collect();
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
    /// bookkeeping-SoA field names deliberately -- the two are one structure.
    ///
    /// The `data` columns must exist before the first store, which is why this
    /// mints a zero-length `MakeVecColumns` rather than an empty table.
    pub(crate) fn empty_column_map(
        &mut self,
        elem: ColumnElem,
    ) -> Result<Idx<Inst>, CompilerError> {
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
                (crate::names::ustr("spine"), spine),
                (crate::names::ustr("data"), data),
                (crate::names::ustr("free_indices"), free),
                (crate::names::ustr("occupied_count"), count),
            ],
        }))
    }

    /// Lower Map storage operations when the receiver's concrete value type is
    /// column-stored. Generic snapshots keep table storage; concrete
    /// instantiations resolve through `SpecSubst` and use spine operations.
    pub(crate) fn map_column_primitive(
        &mut self,
        e: Idx<Expr>,
        name: &str,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<Option<ValueBundle>, CompilerError> {
        match (name, arg_idxs) {
            ("get", [recv, key]) => {
                let Some(elem) = self.map_column_value_of(*recv)? else {
                    return Ok(None);
                };
                let m = self.lower_expr(*recv)?;
                let k = self.lower_expr(*key)?;
                Ok(Some(self.map_column_get_repr(m, k, elem)?))
            }
            ("table_unset", [recv, key]) => {
                let Some(elem) = self.map_column_value_of(*recv)? else {
                    return Ok(None);
                };
                let m = self.lower_expr(*recv)?;
                let k = self.lower_expr(*key)?;
                self.map_column_free(m, k, elem)?;
                let value = ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)));
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("len" | "map_len", [recv]) => {
                let Some(_) = self.map_column_value_of(*recv)? else {
                    return Ok(None);
                };
                let m = self.lower_expr(*recv)?;
                let value = ValueRepr::Scalar(self.call_spine("__map_col_len", vec![m])?);
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("contains", [recv, key]) => {
                let Some(_) = self.map_column_value_of(*recv)? else {
                    return Ok(None);
                };
                let m = self.lower_expr(*recv)?;
                let k = self.lower_expr(*key)?;
                let value = ValueRepr::Scalar(self.call_spine("__map_col_contains", vec![m, k])?);
                Ok(Some(self.value_bundle_for_expr(e, value)?))
            }
            ("clear", [recv]) => {
                let Some(elem) = self.map_column_value_of(*recv)? else {
                    return Ok(None);
                };
                let m = self.lower_expr(*recv)?;
                if self.column_contains_references(elem) {
                    let data = self.map_col_data(m);
                    self.column_release_all(data, elem)?;
                }
                self.call_spine("__map_col_clear", vec![m])?;
                let value = ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)));
                Ok(Some(self.value_bundle_for_expr(e, value)?))
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
        self.store_cell(i_cell, zero);

        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let cmp = self.emit_cur(Inst::IntLt(i, len));
        self.branch_to(cmp, body, exit);

        self.cur = body;
        let i_now = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let one_based = self.emit_cur(Inst::IntAdd(i_now, one));
        let value = self.read_index(raw, one_based, ContainerKind::VecColumn);
        let bundle = ValueBundle::new(elem.representation(), ValueRepr::Scalar(value));
        self.column_append_bundle(vec, &bundle, elem)?;
        let next = self.emit_cur(Inst::IntAdd(i_now, one));
        self.store_cell(i_cell, next);
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
        let vec = self.lower_expr(recv)?;
        let repr = self.lower_expr_repr(item)?;
        self.column_append_bundle(vec, &repr, elem)?;
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))))
    }

    pub(crate) fn column_append_bundle(
        &mut self,
        vec: Idx<Inst>,
        bundle: &ValueBundle,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        let shape = self.column_shape(elem)?;
        let paths = column_leaf_paths(&shape);
        let leaves =
            self.element_storage_leaves(bundle, &shape, paths.len(), elem, "column append")?;
        // `len` is shared by every column, so it advances ONCE. Bumping per
        // column would desync them.
        let len = self.read_field(vec, crate::names::ustr("len"));
        let next = self.int_plus_one(len);
        // A column indexes the way its backend's `_data` does -- 1-based on
        // luajit, 0-based on JS -- so the appended slot is the NEW length on one
        // and the OLD length on the other. The read side gets this from
        // `__vec_checked_index`, whose two implementations differ by the same
        // one; this is the write side of that single decision.
        let slot = if self.column_base_is_one() { next } else { len };
        self.write_field(vec, crate::names::ustr("len"), next);
        self.store_column_leaves(vec, slot, paths, leaves);
        Ok(())
    }

    /// `Some((xs[i], path))` iff `<base>.<field>` is a chain of field accesses
    /// rooted at an element of a column-stored or strided container and the
    /// chain names one leaf: `xs[i].at.x` -> (`xs[i]`, `"at.x"`), the key that
    /// storage addresses the leaf by. A chain that goes past a leaf (`o.hp`
    /// through a reference leaf `o`) is not one; the leaf is its place base.
    pub(crate) fn flat_leaf_path(
        &self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<Option<(Idx<Expr>, Ustr)>, CompilerError> {
        Ok(self
            .flat_field_path(base, field)?
            .and_then(|(root, path, nested)| nested.is_none().then_some((root, path))))
    }

    fn field_chain(&self, mut root: Idx<Expr>, field: Ustr) -> (Idx<Expr>, Vec<Ustr>) {
        let mut fields = vec![field];
        while let Expr::FieldAccess { base, field } = self.body.expr_arena[root] {
            fields.push(field);
            root = base;
        }
        (root, fields)
    }

    /// [`Self::flat_leaf_path`], also admitting a chain that ends at a whole
    /// nested value field (`xs[i].at`), whose shape is returned. Resolved by
    /// walking the shape rather than matching a leaf key: a nullable nested
    /// field's presence column is keyed by the field's own name, so a key match
    /// would mistake the whole field for that one presence leaf.
    pub(crate) fn flat_field_path(
        &self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<Option<(Idx<Expr>, Ustr, Option<ValueShape>)>, CompilerError> {
        let (cur, mut segs) = self.field_chain(base, field);
        let elem = if let Expr::Index {
            base: container, ..
        } = self.body.expr_arena[cur]
        {
            match self.vec_column_elem_of(container)? {
                Some(elem) => Some(elem),
                None => match self.vec_strided_elem_of(container)? {
                    Some(elem) => Some(elem),
                    None => self.map_column_value_of(container)?,
                },
            }
        } else {
            // A flat `ref` binding or call addresses the element or a nested
            // value inside it; its path leads the chain.
            self.flat_ref_root_key(cur)?.map(|key| {
                if !key.prefix.is_empty() {
                    segs.extend(key.prefix.as_str().rsplit('.').map(Ustr::from));
                }
                key.elem
            })
        };
        let Some(elem) = elem else {
            return Ok(None);
        };
        segs.reverse();
        let path = crate::names::ustr(
            &segs
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("."),
        );
        let shape = self.column_shape(elem)?;
        Ok(self
            .value_projection_range(&shape, &segs)
            .map(|(_, selected)| {
                let nested = match selected {
                    FieldShape::Prim => None,
                    FieldShape::Nested(shape) => Some(shape),
                };
                (cur, path, nested)
            }))
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

    /// `Some((vec, slot, elem))` iff `e` is `xs[i]` over a strided Vec -- the
    /// row has no element table to serve as a place base.
    pub(crate) fn strided_index_place(
        &mut self,
        e: Idx<Expr>,
    ) -> Result<Option<(Idx<Inst>, Idx<Inst>, ColumnElem)>, CompilerError> {
        self.with_site_expr(e, |s| {
            let Expr::Index { base, index } = s.body.expr_arena[e] else {
                return Ok(None);
            };
            let Some(elem) = s.vec_strided_elem_of(base)? else {
                return Ok(None);
            };
            let vec = s.lower_expr(base)?;
            let i = s.lower_expr(index)?;
            let slot = s.checked_column_index(vec, i);
            Ok(Some((vec, slot, elem)))
        })
    }

    /// The element-relative leaf keys under nested value field `field` (shape
    /// `sub`), paired with `value`'s leaves in that order, `value` lowered once.
    fn nested_field_leaves(
        &mut self,
        field: Ustr,
        sub: &ValueShape,
        value: Idx<Expr>,
    ) -> Result<(Vec<Ustr>, Vec<Idx<Inst>>), CompilerError> {
        let bundle = self.lower_expr_repr(value)?;
        let leaves = self.repr_to_storage_leaves(&bundle, sub, "nested field store")?;
        let paths: Vec<Ustr> = leaf_names(sub, &field, ".")
            .iter()
            .map(|p| crate::names::ustr(p))
            .collect();
        if leaves.len() != paths.len() {
            return Err(gap(format!(
                "nested field store arity mismatch for `{field}`: {} leaves vs {} keys",
                leaves.len(),
                paths.len()
            )));
        }
        Ok((paths, leaves))
    }

    /// `xs[i].<path> = v` on a strided Vec: store into the slots of the row
    /// under `path`. A primitive leaf (`at.x`) is one slot; a whole nested
    /// value field (`at`, given as `nested`) is its leaves' slots, from `v`
    /// lowered once.
    pub(crate) fn strided_leaf_store(
        &mut self,
        vec: Idx<Inst>,
        slot: Idx<Inst>,
        elem: ColumnElem,
        field: Ustr,
        nested: Option<&ValueShape>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let shape = self.column_shape(elem)?;
        let row = value_leaf_paths(&shape);
        let (paths, leaves) = match nested {
            Some(sub) => self.nested_field_leaves(field, sub, value)?,
            None => (vec![field], vec![self.lower_expr(value)?]),
        };
        let offsets = paths
            .iter()
            .map(|path| row.iter().position(|p| p == path))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                gap(format!(
                    "`{field}` does not name slots of strided `[{}]`",
                    elem.name
                ))
            })?;
        let raw = self.strided_stores_raw(elem);
        let data = self.read_field(vec, crate::names::ustr("_data"));
        for (offset, v) in offsets.into_iter().zip(leaves) {
            let v = if raw {
                v
            } else {
                self.emit_cur(Inst::VecStrideNilToFalse(v))
            };
            let at = self.strided_slot_index(slot, shape.leaf_count(), offset);
            self.write_index(data, at, v, ContainerKind::VecStrideSlot);
        }
        Ok(())
    }

    /// `xs[i].<path> = v` on a column-stored Vec: store into the columns under
    /// `path`. A primitive leaf (`inner.a`) is one column; a whole nested value
    /// field (`inner`, given as `nested`) is every leaf column under it, keyed
    /// as [`column_leaf_paths`] keys them, from `v` lowered once.
    pub(crate) fn column_leaf_store(
        &mut self,
        vec: Idx<Inst>,
        slot: Idx<Inst>,
        elem: ColumnElem,
        field: Ustr,
        nested: Option<&ValueShape>,
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
        let (paths, leaves) = match nested {
            Some(sub) => self.nested_field_leaves(field, sub, value)?,
            None => {
                let shape = self.column_shape(elem)?;
                if !value_leaf_paths(&shape).contains(&field) {
                    return Err(gap(format!(
                        "`{field}` is not a field of column-stored `[{}]`",
                        elem.name
                    )));
                }
                (vec![field], vec![self.lower_expr(value)?])
            }
        };
        for (path, leaf) in paths.into_iter().zip(leaves) {
            let col = self.read_field(vec, path);
            self.write_index(col, slot, leaf, ContainerKind::VecColumn);
        }
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

    /// Emit a call to a non-generic synthesized runtime entry. Its
    /// `SiteOp::Synthesized(name)` keeps it distinct from the enclosing
    /// expression's own callee in the concrete call plan.
    pub(crate) fn call_spine(
        &mut self,
        name: &'static str,
        args: Vec<Idx<Inst>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        debug_assert!(
            SYNTHESIZED_ENTRIES.contains(&name),
            "`{name}` is missing from SYNTHESIZED_ENTRIES"
        );
        let func = self.stdlib_funcref(name)?;
        Ok(
            self.with_site_op(crate::site::SiteOp::Synthesized(name), |b| {
                b.emit_cur(Inst::Call { func, args })
            }),
        )
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
        let expr = self
            .cur_expr
            .ok_or_else(|| gap(format!("dispatch helper `{name}` has no source expression")))?;
        self.with_site_op(crate::site::SiteOp::dispatched(name, path), |b| {
            let direct = b.plan_direct_call(expr)?;
            let func = super::canonical_variant(b.ws, direct.callee_inst.func);
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
        // The instance graph decides the absent arm from the value's TYPE, so
        // this does too: a null-checked binding keeps its presence lane, and a
        // boxed source is a scalar, yet neither can be null here.
        if self
            .concrete_expr_ty(value)
            .is_some_and(|ty| !ty.peel_nullable().1)
        {
            let stored = match repr {
                ValueRepr::Scalar(s) => ValueBundle::new(
                    elem.representation(),
                    self.boxed_value_copy_repr(*s, elem.name, None)?,
                ),
                _ => bundle.clone(),
            };
            let slot = self.call_spine("__map_col_slot", vec![m, key])?;
            let data = self.map_col_data(m);
            return self.column_element_store_bundle(data, slot, &stored, elem);
        }
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
        self.branch_to(present, set_bb, del_bb);
        self.cur = set_bb;
        let stored = match boxed {
            Some(s) => ValueBundle::new(
                elem.representation(),
                self.boxed_value_copy_repr(s, elem.name, None)?,
            ),
            None => bundle.clone(),
        };
        let slot = self.call_spine("__map_col_slot", vec![m, key])?;
        let data = self.map_col_data(m);
        self.column_element_store_bundle(data, slot, &stored, elem)?;
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = del_bb;
        self.map_column_free(m, key, elem)?;
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
    ) -> Result<ValueBundle, CompilerError> {
        let slot = self.call_spine("__map_col_slot_checked", vec![m, key])?;
        let data = self.map_col_data(m);
        self.column_element_repr(data, slot, elem)
    }

    /// `m.get(k)` on a column Map returns `V?` with presence determined by the
    /// spine. Read columns only in the present arm: a missing slot is `-1`, and
    /// freed slots may contain stale leaves. Unmarked values build their box in
    /// the present arm; `nobox` results remain leaf groups across the join.
    pub(crate) fn map_column_get_repr(
        &mut self,
        m: Idx<Inst>,
        key: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<ValueBundle, CompilerError> {
        let shape = self.column_shape(elem)?;
        let unboxed = elem.is_nobox();
        let slot = self.call_spine("__map_col_slot_of", vec![m, key])?;
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        let present = self.emit_cur(Inst::IntGeq(slot, zero));
        let cell_count = if unboxed {
            value_leaf_paths(&shape).len()
        } else {
            1
        };
        let cells = self.fresh_cells(cell_count);
        let hit = self.f.new_block();
        let miss = self.f.new_block();
        let join = self.f.new_block();
        self.branch_to(present, hit, miss);
        self.cur = hit;
        let data = self.map_col_data(m);
        let found = self.column_element_repr(data, slot, elem)?;
        let stored: Vec<Idx<Inst>> = if unboxed {
            self.repr_to_leaves_bundle(&found)?
        } else {
            vec![self.pack(found.into_value())?]
        };
        for (cell, value) in cells.iter().zip(stored) {
            self.store_cell(*cell, value);
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = miss;
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        for cell in &cells {
            self.store_cell(*cell, nil);
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        let loaded = self.load_cells(&cells);
        if !unboxed {
            return Ok(ValueBundle::new(
                elem.representation(),
                ValueRepr::Scalar(loaded[0]),
            ));
        }
        let mut idx = 0;
        let inner = self.rebuild_from_leaves(&shape, &loaded, &mut idx);
        let value = match inner {
            ValueRepr::Group { name, parts, .. } => ValueRepr::Group {
                name,
                parts,
                present: Some(present),
            },
            other => other,
        };
        Ok(ValueBundle::new(elem.representation(), value))
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
        let paths = column_leaf_paths(&shape);
        let leaves =
            self.element_storage_leaves(bundle, &shape, paths.len(), elem, "column store")?;
        let slot = self.checked_column_index(vec, index);
        self.store_column_leaves(vec, slot, paths, leaves);
        Ok(())
    }

    fn store_column_leaves(
        &mut self,
        vec: Idx<Inst>,
        slot: Idx<Inst>,
        paths: Vec<Ustr>,
        leaves: Vec<Idx<Inst>>,
    ) {
        for (path, leaf) in paths.into_iter().zip(leaves) {
            let col = self.read_field(vec, path);
            self.write_index(col, slot, leaf, ContainerKind::VecColumn);
        }
    }

    /// `bundle`'s storage leaves, checked to fill exactly `slots` element slots.
    pub(crate) fn element_storage_leaves(
        &mut self,
        bundle: &ValueBundle,
        shape: &ValueShape,
        slots: usize,
        elem: ColumnElem,
        reason: &str,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let leaves = self.repr_to_storage_leaves(bundle, shape, reason)?;
        if leaves.len() != slots {
            return Err(gap(format!(
                "{reason} arity mismatch for [{}]: {} leaves vs {slots} slots",
                elem.name,
                leaves.len()
            )));
        }
        Ok(leaves)
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
    /// layout is measured against.
    pub(crate) fn column_base_is_one(&self) -> bool {
        self.scx.target != bone_core::hir::FnBackend::Js
    }
}
