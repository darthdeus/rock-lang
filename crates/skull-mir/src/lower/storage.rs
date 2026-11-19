use super::flat_ref::FlatRefVal;
use super::*;

/// Builds root every entry [`Builder::call_spine`] may call instead of
/// predicting synthesized sites, so a call resolves wherever lowering emits it.
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

enum VecDataOp {
    Get(Idx<Expr>),
    Set(Idx<Expr>, Idx<Expr>),
    Unset(Idx<Expr>),
    Capacity,
    Insert(Idx<Expr>, Idx<Expr>),
    Clone,
    Sort(Idx<Expr>),
    ToLua,
}

impl<'a> Builder<'a> {
    pub(crate) fn flat_element_repr(
        &mut self,
        vec: Idx<Inst>,
        index: Idx<Inst>,
        elem: ColumnElem,
        storage: FlatStorage,
    ) -> Result<ValueBundle, CompilerError> {
        match storage {
            FlatStorage::Columns => self.column_element_repr(vec, index, elem),
            FlatStorage::Strided => self.strided_element_repr(vec, index, elem),
        }
    }

    pub(crate) fn flat_element_store(
        &mut self,
        vec: Idx<Inst>,
        index: Idx<Inst>,
        value: Idx<Expr>,
        elem: ColumnElem,
        storage: FlatStorage,
    ) -> Result<(), CompilerError> {
        let bundle = self.lower_expr_repr(value)?;
        match storage {
            FlatStorage::Columns => self.column_element_store_bundle(vec, index, &bundle, elem),
            FlatStorage::Strided => self.strided_element_store_bundle(vec, index, &bundle, elem),
        }
    }

    pub(crate) fn flat_append_bundle(
        &mut self,
        vec: Idx<Inst>,
        bundle: &ValueBundle,
        elem: ColumnElem,
        storage: FlatStorage,
    ) -> Result<(), CompilerError> {
        match storage {
            FlatStorage::Columns => self.column_append_bundle(vec, bundle, elem),
            FlatStorage::Strided => self.strided_append_bundle(vec, bundle, elem),
        }
    }

    pub(crate) fn flat_push(
        &mut self,
        recv: Idx<Expr>,
        item: Idx<Expr>,
        elem: ColumnElem,
        storage: FlatStorage,
    ) -> Result<ValueRepr, CompilerError> {
        let vec = self.lower_expr(recv)?;
        let bundle = self.lower_expr_repr(item)?;
        self.flat_append_bundle(vec, &bundle, elem, storage)?;
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))))
    }

    pub(crate) fn empty_flat_vec(
        &mut self,
        elem: ColumnElem,
        storage: FlatStorage,
        capacity: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match storage {
            FlatStorage::Columns => self.empty_column_vec(elem, capacity),
            FlatStorage::Strided => self.empty_strided_vec(elem, capacity),
        }
    }

    pub(crate) fn clone_flat_data(
        &mut self,
        vec: Idx<Inst>,
        elem: ColumnElem,
        storage: FlatStorage,
    ) -> Result<ValueRepr, CompilerError> {
        match storage {
            FlatStorage::Columns => self.clone_column_data(vec, elem),
            FlatStorage::Strided => self.clone_strided_data(vec, elem),
        }
    }

    fn clone_column_data(
        &mut self,
        vec: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<ValueRepr, CompilerError> {
        let shape = self.column_shape(elem)?;
        let paths = column_leaf_paths(&shape);

        // Built as a `Vec`-tagged struct: `write_table_key` brackets a dotted leaf
        // path, so the table has the shape `MakeVecColumns` writes for a literal.
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

    fn clone_strided_data(
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

    fn strided_element_repr(
        &mut self,
        vec: Idx<Inst>,
        index: Idx<Inst>,
        elem: ColumnElem,
    ) -> Result<ValueBundle, CompilerError> {
        let shape = self.column_shape(elem)?;
        let leaves = self.strided_element_leaves(vec, index, elem, &shape, 0..shape.leaf_count());
        Ok(ValueBundle::new(
            elem.representation(),
            Self::rebuild_from_leaves(&shape, &leaves),
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
        let slot = self.checked_column_index(vec, index);
        let data = self.read_field(vec, crate::names::ustr("_data"));
        range
            .map(|offset| {
                let at = self.strided_slot_index(slot, stride, offset);
                let stored = self.read_index(data, at, ContainerKind::VecStrideSlot);
                self.strided_decode(elem, stored)
            })
            .collect()
    }

    fn strided_element_store_bundle(
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

    /// Capped at 12: each distinct offset can cost a KNUM plus a KINT, three of
    /// LuaJIT's 500 constant slots, on x86.
    /// Wider rows advance one index so their leaves add no offset constants.
    fn strided_store_leaves_at(
        &mut self,
        vec: Idx<Inst>,
        row: Idx<Inst>,
        leaves: Vec<Idx<Inst>>,
        stride: usize,
        elem: ColumnElem,
    ) {
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
            let value = self.strided_encode(elem, leaf);
            self.write_index(data, at, value, ContainerKind::VecStrideSlot);
        }
    }

    /// A wide `nobox` struct row keeps every leaf as stored: a `false` leaf would
    /// be corrupted by the flat-enum tier's nil-to-false densifying.
    /// Enum rows are numeric by eligibility and keep the sentinel.
    pub(crate) fn strided_stores_raw(&self, elem: ColumnElem) -> bool {
        self.ws.struct_by_name.contains_key(&elem.name)
    }

    pub(crate) fn strided_encode(&mut self, elem: ColumnElem, leaf: Idx<Inst>) -> Idx<Inst> {
        if self.strided_stores_raw(elem) {
            leaf
        } else {
            self.emit_cur(Inst::VecStrideNilToFalse(leaf))
        }
    }

    pub(crate) fn strided_decode(&mut self, elem: ColumnElem, stored: Idx<Inst>) -> Idx<Inst> {
        if self.strided_stores_raw(elem) {
            stored
        } else {
            self.emit_cur(Inst::VecStrideFalseToNil(stored))
        }
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

    fn strided_append_bundle(
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

    fn empty_strided_vec(
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

    pub(crate) fn flat_vec_from_raw_array(
        &mut self,
        raw: Idx<Inst>,
        len: Idx<Inst>,
        elem: ColumnElem,
        storage: FlatStorage,
    ) -> Result<Idx<Inst>, CompilerError> {
        match storage {
            FlatStorage::Columns => self.column_vec_from_raw_array(raw, len, elem),
            FlatStorage::Strided => self.strided_vec_from_raw_array(raw, len, elem),
        }
    }

    fn strided_vec_from_raw_array(
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
            let unpacked = Self::rebuild_from_leaves(&shape, &leaves);
            ValueBundle::new(elem.representation(), unpacked)
        };
        self.strided_append_bundle(vec, &bundle, elem)?;
        let next = self.int_plus_one(i);
        self.store_cell(cell, next);
        self.f.set_term(self.cur, Term::Goto(header));
        self.cur = exit;
        Ok(vec)
    }

    fn column_vec_from_raw_array(
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

    pub(crate) fn nb_leaf_field_store(
        &mut self,
        buf: Idx<Inst>,
        checked: Idx<Inst>,
        elem: Ustr,
        path: &str,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let container = self.native_buffer_plan(elem)?;
        let dot = format!("{path}.");
        let hit: Vec<LeafAddr> = Self::nb_leaf_places(&container)?
            .iter()
            .filter(|a| a.path.as_str() == path || a.path.as_str().starts_with(&dot))
            .cloned()
            .collect();
        if hit.is_empty() {
            return Err(gap(format!(
                "`{path}` is not a leaf of NativeBuffer<{elem}>"
            )));
        }
        // Match the exact leaf path, not the hit count: a nested field spanning one
        // leaf also yields one hit, under `<path>.<leaf>`.
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
        self.nb_write_leaves(buf, checked, &hit, leaves);
        Ok(())
    }

    fn nb_leaf_places(container: &ContainerPlan) -> Result<&[LeafAddr], CompilerError> {
        container
            .leaf_places()
            .ok_or_else(|| gap("NativeBuffer element has no owned leaf address plan"))
    }

    fn nb_write_leaves(
        &mut self,
        buf: Idx<Inst>,
        checked: Idx<Inst>,
        places: &[LeafAddr],
        leaves: Vec<Idx<Inst>>,
    ) {
        let mut views = HashMap::default();
        for (a, leaf) in places.iter().zip(leaves) {
            let at = self.leaf_index(checked, a);
            let view = self.leaf_view(buf, a.view_field, &mut views);
            self.write_index(view, at, leaf, ContainerKind::NativeBufferLeaf);
        }
    }

    /// Lua pointers and JS TypedArrays use the same planned C offsets and strides.
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

    pub(crate) fn nb_leaf_address(
        &self,
        elem: Ustr,
        path: &str,
    ) -> Result<Option<LeafAddr>, CompilerError> {
        let container = self.native_buffer_plan(elem)?;
        Ok(container
            .leaf_places()
            .and_then(|plan| plan.iter().find(|a| a.path.as_str() == path))
            .cloned())
    }

    pub(crate) fn nb_leaf_load(
        &mut self,
        buf: Idx<Inst>,
        checked: Idx<Inst>,
        addr: &LeafAddr,
    ) -> Idx<Inst> {
        let (view, at) = self.nb_leaf_slot(buf, checked, addr);
        self.read_index(view, at, ContainerKind::NativeBufferLeaf)
    }

    pub(crate) fn nb_leaf_store(
        &mut self,
        buf: Idx<Inst>,
        checked: Idx<Inst>,
        addr: &LeafAddr,
        value: Idx<Inst>,
    ) {
        let (view, at) = self.nb_leaf_slot(buf, checked, addr);
        self.write_index(view, at, value, ContainerKind::NativeBufferLeaf);
    }

    fn nb_leaf_slot(
        &mut self,
        buf: Idx<Inst>,
        checked: Idx<Inst>,
        addr: &LeafAddr,
    ) -> (Idx<Inst>, Idx<Inst>) {
        let at = self.leaf_index(checked, addr);
        let view = self.leaf_view(buf, addr.view_field, &mut HashMap::default());
        (view, at)
    }

    /// Bounds-checks the element once and takes no slot reference, so the read
    /// allocates nothing; the `Group` result lets SROA peel projected bindings.
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
        let plan = Self::nb_leaf_places(&container)?;
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
        Ok(ValueBundle::new(
            container.element_rep(),
            Self::rebuild_from_leaves(&shape, &leaves),
        ))
    }

    /// The element never materializes, which is what lets a `nobox` type live in
    /// a NativeBuffer.
    pub(crate) fn nb_leaf_element_store(
        &mut self,
        buf: Idx<Inst>,
        index: Idx<Inst>,
        value: Idx<Expr>,
        elem: Ustr,
    ) -> Result<(), CompilerError> {
        let container = self.native_buffer_plan(elem)?;
        let bundle = self.lower_expr_repr(value)?;
        let shape = container
            .element_shape(self.scx.rep_arena())
            .ok_or_else(|| gap(format!("NativeBuffer<{elem}> has no value leaf plan")))?;
        let leaves = self.repr_to_storage_leaves(&bundle, &shape, "native buffer store")?;
        let plan = Self::nb_leaf_places(&container)?;
        if leaves.len() != plan.len() {
            return Err(gap(format!(
                "leaf store arity mismatch for NativeBuffer<{elem}>: {} leaves vs {} slots",
                leaves.len(),
                plan.len()
            )));
        }
        let checked = self.nb_checked_index(buf, index)?;
        self.nb_write_leaves(buf, checked, plan, leaves);
        Ok(())
    }

    /// Read once per element access and reused across that element's leaves.
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

    /// Bounds-checks without taking a slot reference; one call per element
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
        // Presence leads the reads for a nullable element, as `rebuild_from_leaves`
        // consumes it, so `present` comes from the column, not a nil compare on a box.
        let leaves: Vec<Idx<Inst>> = column_leaf_paths(&shape)
            .into_iter()
            .map(|path| {
                let col = self.read_field(vec, path);
                self.read_index(col, slot, ContainerKind::VecColumn)
            })
            .collect();
        Ok(ValueBundle::new(
            elem.representation(),
            Self::rebuild_from_leaves(&shape, &leaves),
        ))
    }

    /// For a synthesized call. A missing or ambiguous name means the stdlib and
    /// lowering drifted, so it rejects; backend siblings pick this target's decl.
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

    pub(crate) fn flat_vec_data_primitive(
        &mut self,
        e: Idx<Expr>,
        name: &str,
        args: &[Idx<Expr>],
    ) -> Result<Option<ValueBundle>, CompilerError> {
        let (recv, op) = match (name, args) {
            ("__vec_data_get", &[recv, index]) => (recv, VecDataOp::Get(index)),
            ("__vec_data_set", &[recv, index, value]) => (recv, VecDataOp::Set(index, value)),
            ("__vec_data_unset", &[recv, index]) => (recv, VecDataOp::Unset(index)),
            ("__vec_data_capacity", &[recv]) => (recv, VecDataOp::Capacity),
            ("__vec_data_insert", &[recv, index, value]) => (recv, VecDataOp::Insert(index, value)),
            ("__vec_data_clone", &[recv]) => (recv, VecDataOp::Clone),
            ("__vec_data_sort", &[recv, key]) => (recv, VecDataOp::Sort(key)),
            ("__rock_vec_to_lua_array", &[recv]) => (recv, VecDataOp::ToLua),
            _ => return Ok(None),
        };
        let Some((elem, storage)) = self.vec_flat_elem_of(recv)? else {
            return Ok(None);
        };

        let value = match op {
            VecDataOp::Get(index) => {
                let vec = self.lower_expr(recv)?;
                let index = self.lower_expr(index)?;
                return self.flat_element_repr(vec, index, elem, storage).map(Some);
            }
            VecDataOp::Set(index, value) => {
                let vec = self.lower_expr(recv)?;
                let index = self.lower_expr(index)?;
                self.flat_element_store(vec, index, value, elem, storage)?;
                None
            }
            VecDataOp::Unset(index) => {
                match storage {
                    FlatStorage::Columns => {
                        if self.column_contains_references(elem) {
                            let vec = self.lower_expr(recv)?;
                            let index = self.lower_expr(index)?;
                            self.column_unset(vec, index, elem)?;
                        }
                    }
                    FlatStorage::Strided => {
                        let stride = self.column_shape(elem)?.leaf_count();
                        let raw = self.strided_stores_raw(elem);
                        // Stale scalar slots beyond len hold nothing to release.
                        if !raw || self.column_contains_references(elem) {
                            let vec = self.lower_expr(recv)?;
                            let index = self.lower_expr(index)?;
                            let slot = self.checked_column_index(vec, index);
                            let row = self.strided_row_of(slot);
                            let vacant = if raw { Lit::Nil } else { Lit::Bool(false) };
                            let leaves = (0..stride)
                                .map(|_| self.emit_cur(Inst::Const(vacant.clone())))
                                .collect();
                            self.strided_store_leaves_at(vec, row, leaves, stride, elem);
                        }
                    }
                }
                None
            }
            VecDataOp::Capacity => {
                let shape = self.column_shape(elem)?;
                let vec = self.lower_expr(recv)?;
                let header = self.column_header(vec);
                let reserved = self.read_field(header, crate::names::ustr("cap"));
                let capacity = match storage {
                    FlatStorage::Columns => {
                        // Nil reference leaves make a column's Lua length unreliable.
                        let physical = if !self.column_contains_references(elem)
                            && let Some(path) = column_leaf_paths(&shape).first()
                        {
                            let column = self.read_field(vec, *path);
                            self.call_spine("table_len", vec![column])?
                        } else {
                            self.read_field(vec, crate::names::ustr("len"))
                        };
                        self.call_spine("__rock_col_capacity", vec![reserved, physical])?
                    }
                    FlatStorage::Strided => {
                        let width = self.emit_cur(Inst::Const(Lit::Int(shape.leaf_count() as i64)));
                        let physical = if self.strided_stores_raw(elem) {
                            let len = self.read_field(vec, crate::names::ustr("len"));
                            self.emit_cur(Inst::IntMul(len, width))
                        } else {
                            let data = self.read_field(vec, crate::names::ustr("_data"));
                            self.call_spine("table_len", vec![data])?
                        };
                        self.call_spine("__rock_stride_capacity", vec![reserved, physical, width])?
                    }
                };
                Some(ValueRepr::Scalar(capacity))
            }
            VecDataOp::Insert(index, value) => {
                let shape = self.column_shape(elem)?;
                let paths = (storage == FlatStorage::Columns).then(|| column_leaf_paths(&shape));
                let vec = self.lower_expr(recv)?;
                let index = self.lower_expr(index)?;
                let bundle = self.lower_expr_repr(value)?;
                if let Some(paths) = paths {
                    let leaves = self.repr_to_leaves_bundle(&bundle)?;
                    if leaves.len() != paths.len() {
                        return Err(gap(format!(
                            "column insert arity mismatch for [{}]",
                            elem.name
                        )));
                    }
                    let len = self.read_field(vec, crate::names::ustr("len"));
                    for (path, leaf) in paths.into_iter().zip(leaves) {
                        let col = self.read_field(vec, path);
                        self.call_spine("__rock_col_insert", vec![col, index, len, leaf])?;
                    }
                } else {
                    let stride = shape.leaf_count();
                    let leaves = self.repr_to_storage_leaves(&bundle, &shape, "strided insert")?;
                    let data = self.read_field(vec, crate::names::ustr("_data"));
                    let len = self.read_field(vec, crate::names::ustr("len"));
                    let width = self.emit_cur(Inst::Const(Lit::Int(stride as i64)));
                    self.call_spine("__rock_stride_insert", vec![data, index, len, width])?;
                    // The position argument is 1-based on both backends.
                    let one = self.emit_cur(Inst::Const(Lit::Int(1)));
                    let row = self.emit_cur(Inst::IntSub(index, one));
                    self.strided_store_leaves_at(vec, row, leaves, stride, elem);
                }
                None
            }
            VecDataOp::Clone => {
                let vec = self.lower_expr(recv)?;
                Some(self.clone_flat_data(vec, elem, storage)?)
            }
            VecDataOp::Sort(key) => {
                let stride = (storage == FlatStorage::Strided)
                    .then(|| self.column_shape(elem).map(|shape| shape.leaf_count()))
                    .transpose()?;
                let vec = self.lower_expr(recv)?;
                let key = self.lower_expr(key)?;
                let call = if let Some(stride) = stride {
                    let width = self.emit_cur(Inst::Const(Lit::Int(stride as i64)));
                    let helper = if elem.is_nobox() && self.strided_stores_raw(elem) {
                        "__rock_stride_sort_leaves"
                    } else {
                        "__rock_stride_sort"
                    };
                    self.call_spine(helper, vec![vec, key, width])?
                } else if elem.is_nobox() {
                    // Pass the argument leaves in the key function's ABI order.
                    let shape = self.column_shape(elem)?;
                    let paths = column_leaf_paths(&shape)
                        .into_iter()
                        .map(|p| self.emit_cur(Inst::Const(Lit::Str(p))))
                        .collect();
                    let paths = self.emit_cur(Inst::MakeTuple(paths));
                    self.call_spine("__rock_col_sort_leaves", vec![vec, key, paths])?
                } else {
                    self.call_spine("__rock_col_sort", vec![vec, key])?
                };
                Some(ValueRepr::Scalar(call))
            }
            VecDataOp::ToLua => {
                let (layout, helper) = match storage {
                    FlatStorage::Columns => ("column-stored", "__rock_vec_column_elements"),
                    FlatStorage::Strided => ("strided", "__rock_vec_strided_elements"),
                };
                if elem.is_nobox()
                    && (storage == FlatStorage::Columns || self.strided_stores_raw(elem))
                {
                    return Err(CompilerError::new_with_label(
                        self.body.expr_spans[recv],
                        format!(
                            "cannot convert {layout} nobox value `{}` to a Lua array: \
                             the bridge requires implicit element materialization",
                            elem.name
                        ),
                    ));
                }
                let vec = self.lower_expr(recv)?;
                Some(ValueRepr::Scalar(self.call_spine(helper, vec![vec])?))
            }
        };
        let value =
            value.unwrap_or_else(|| ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))));
        self.value_bundle_for_expr(e, value).map(Some)
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

    fn empty_column_vec(
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

    /// The `data` columns must exist before the first store.
    /// Hence a zero-length `MakeVecColumns`, not an empty table. The field names
    /// match Arena's bookkeeping SoA: the two are one structure.
    pub(crate) fn empty_column_map(
        &mut self,
        elem: ColumnElem,
    ) -> Result<Idx<Inst>, CompilerError> {
        let zero_cap = self.emit_cur(Inst::Const(Lit::Int(0)));
        let data = self.empty_column_vec(elem, zero_cap)?;
        // The spine's key space is user data; a JS object literal would inherit
        // host object-model names.
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

    /// Generic snapshots keep table storage; concrete instantiations resolve
    /// through `SpecSubst` and use spine operations.
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

    fn column_append_bundle(
        &mut self,
        vec: Idx<Inst>,
        bundle: &ValueBundle,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        let shape = self.column_shape(elem)?;
        let paths = column_leaf_paths(&shape);
        let leaves =
            self.element_storage_leaves(bundle, &shape, paths.len(), elem, "column append")?;
        // `len` is shared by every column, so it advances once; bumping per column
        // would desync them.
        let len = self.read_field(vec, crate::names::ustr("len"));
        let next = self.int_plus_one(len);
        // A column indexes like its backend's `_data` (1-based on LuaJIT, 0-based
        // on JS), so the appended slot is the new length on one and the old on the other.
        let slot = if self.column_base_is_one() { next } else { len };
        self.write_field(vec, crate::names::ustr("len"), next);
        self.store_column_leaves(vec, slot, paths, leaves);
        Ok(())
    }

    /// The chain must name one leaf: `xs[i].at.x` -> (`xs[i]`, `"at.x"`). A chain
    /// past a reference leaf is not one; the leaf is its place base.
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

    /// A nullable nested field's presence column is keyed by the field's own name.
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
            self.flat_container_elem(container)?.map(|(elem, _)| elem)
        } else {
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

    pub(crate) fn lower_flat_index_place(
        &mut self,
        e: Idx<Expr>,
        key: FlatRefKey,
    ) -> Result<FlatRefVal, CompilerError> {
        self.with_site_expr(e, |s| {
            let Expr::Index { base, index } = s.body.expr_arena[e] else {
                return Err(s.gap_here("flat place must name an indexed element"));
            };
            let map = key.storage == FlatStorage::Columns && s.map_column_value_of(base)?.is_some();
            let mut vec = s.lower_expr(base)?;
            let mut index = s.lower_expr(index)?;
            if map {
                index = s.call_spine("__map_col_slot_checked", vec![vec, index])?;
                vec = s.map_col_data(vec);
            }
            let slot = s.checked_column_index(vec, index);
            Ok(FlatRefVal { vec, slot, key })
        })
    }

    /// Paired with `value`'s leaves in order, with `value` lowered once.
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

    pub(crate) fn flat_field_store(
        &mut self,
        place: FlatRefVal,
        nested: Option<&ValueShape>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let FlatRefVal { vec, slot, key } = place;
        let FlatRefKey {
            elem,
            storage,
            prefix: field,
        } = key;
        if storage == FlatStorage::Columns && elem.nullable {
            return Err(gap(format!(
                "cannot write `{field}` through an element of `[{}?]`; refine the element to non-null and store it whole",
                elem.name
            )));
        }

        let row = match storage {
            FlatStorage::Strided => value_leaf_paths(&self.column_shape(elem)?),
            FlatStorage::Columns => {
                if nested.is_none() && !value_leaf_paths(&self.column_shape(elem)?).contains(&field)
                {
                    return Err(gap(format!(
                        "`{field}` is not a field of column-stored `[{}]`",
                        elem.name
                    )));
                }
                Vec::new()
            }
        };
        let (paths, leaves) = match nested {
            Some(sub) => self.nested_field_leaves(field, sub, value)?,
            None => (vec![field], vec![self.lower_expr(value)?]),
        };
        if storage == FlatStorage::Columns {
            self.store_column_leaves(vec, slot, paths, leaves);
            return Ok(());
        }

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
        let data = self.read_field(vec, crate::names::ustr("_data"));
        for (offset, v) in offsets.into_iter().zip(leaves) {
            let v = self.strided_encode(elem, v);
            let at = self.strided_slot_index(slot, row.len(), offset);
            self.write_index(data, at, v, ContainerKind::VecStrideSlot);
        }
        Ok(())
    }

    /// Its `SiteOp::Synthesized(name)` keeps it distinct from the enclosing
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

    /// Several such calls can share one expression and one name (a nobox value
    /// renders per leaf field); the `path` keeps their sites apart.
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

    /// The value never materializes. A nullable value collapses to key removal
    /// when absent, since a nil store already removes a Map key.
    pub(crate) fn map_column_store(
        &mut self,
        m: Idx<Inst>,
        key: Idx<Inst>,
        value: Idx<Expr>,
        elem: ColumnElem,
    ) -> Result<(), CompilerError> {
        let bundle = self.lower_expr_repr(value)?;
        let repr = bundle.value();
        // The instance graph decides the absent arm from the value's type, so this
        // does too.
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
        // Read leaves only on the present arm: `s.x` faults on a literal-null store.
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

    /// Errors on a miss, like the boxed `__map_get_checked`.
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

    /// Read columns only in the present arm: a missing slot is `-1`, and freed
    /// slots may hold stale leaves.
    /// `nobox` results remain leaf groups across the join.
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
        let inner = Self::rebuild_from_leaves(&shape, &loaded);
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

    /// One call per element access, not per column.
    pub(crate) fn checked_column_index(&mut self, vec: Idx<Inst>, index: Idx<Inst>) -> Idx<Inst> {
        self.emit_cur(Inst::VecCheckedIndex { vec, index })
    }

    /// Whether a leaf column is 1-based, following the backend's `_data`. JS stays
    /// 0-based: an empty index 0 makes a holey V8 array and loses packed doubles.
    pub(crate) fn column_base_is_one(&self) -> bool {
        self.scx.target != bone_core::hir::FnBackend::Js
    }
}
