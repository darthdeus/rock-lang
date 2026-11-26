use bone_core::hir::TyId;
use bone_core::prelude::Span;
use comfy_la_arena::{Arena, Idx};
use std::sync::Arc;
use ustr::Ustr;

pub const VALUE_INTEGER_CHECK: &str = "__rock_value_integer_check";

pub const VEC_LENGTH_SLOT: &str = "@vec_len";

pub const VEC_COLUMN_HEADER_FIELD: &str = "@vec";

/// Part of the plan identity: backends must not infer it from field names
/// or operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueCtorForm {
    Declared,
    Leaves,
}
pub const VEC_COLUMN_REPR: &str = "columns";
pub const VEC_STRIDED_REPR: &str = "strided";
/// One bool per slot, leading the leaf columns. `@` cannot appear in a Rock
/// identifier, so the key never collides with a leaf path.
pub(crate) const VEC_COLUMN_PRESENCE_FIELD: &str = "@p";

pub fn column_elem_rep_plans(
    mf: &MirFunc,
    out: &mut std::collections::BTreeMap<Ustr, (bool, crate::lower::RepId)>,
) {
    for inst in mf.insts.values() {
        if let Inst::MakeFlatVec(vec) = inst
            && vec.contents.stride().is_none()
        {
            out.insert(
                vec.elem.name,
                (vec.elem.is_nobox(), vec.elem.representation()),
            );
        }
    }
    for nf in &mf.nested_fns {
        column_elem_rep_plans(nf, out);
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FuncRef {
    pub module: bone_core::prelude::ModuleId,
    pub func: Idx<bone_core::hir::Function>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Lit {
    Int(i64),
    Uint(u32),
    Float(f64),
    Bool(bool),
    Str(Ustr),
    Nil,
    /// Passed for an omitted `T?` parameter with a non-null default: the callee
    /// fills the default only on this marker, so an explicit `null` stays null.
    Omitted,
}

/// The IR is 0-based; the Lua printer adds `+1` on sequence access only, never
/// on `Map` keys or arithmetic.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ContainerKind {
    /// A slot may be absent while in bounds (`[T?]`), so access may enter a
    /// backing table metamethod.
    Vec,
    /// In-bounds access cannot reach a backing table metamethod.
    VecNonNull,
    /// Index proven in `[0, len)`; a metamethod can still run for an absent slot.
    /// Only on `Index` and `StoreIndex`.
    VecInBounds,
    VecNonNullInBounds,
    /// `{ ptr, len }`; `.ptr` is a 0-based C array.
    NativeBuffer,
    NativeBufferInBounds,
    /// Lua renders this exactly like `NativeBuffer`; JS lowers element access to
    /// slot arithmetic over one flat `Float32Array(n * k)`.
    NativeBufferValue(Ustr),
    /// One 1-indexed Lua array per leaf, `{ <leaf> = {..}, len = n }`; no
    /// per-element table exists.
    VecValueColumns(ColumnElem),
    /// Fixed-width rows in one dense array; inactive enum lanes store `false`,
    /// never `nil`.
    VecValueStrided(ColumnElem),
    /// The index is already 1-based and bounds-checked.
    VecColumn,
    /// The index is already in the backend's physical array base and was
    /// bounds-checked by the owning element access.
    VecStrideSlot,
    /// Indexing a primitive pointer yields a number; indexing a struct pointer
    /// would allocate a reference cdata.
    /// The index is absolute (`i * stride + leaf_off`) and already bounds-checked
    /// per element: no `+1`, no check.
    NativeBufferLeaf,
    /// Raw `base[index]` with the receiver's nominal type when known.
    Record(Option<Ustr>),
    /// Keyed directly by K, no `+1`.
    Map,
}

/// Nullability travels with the element because it decides the column set;
/// reading it off a use site's type would add a phantom presence column.
#[derive(Copy, Clone, Debug)]
pub struct ColumnElem {
    pub name: Ustr,
    pub nullable: bool,
    representation: crate::lower::RepId,
    nobox: bool,
}

impl PartialEq for ColumnElem {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.nullable == other.nullable && self.nobox == other.nobox
    }
}

impl Eq for ColumnElem {}

impl std::hash::Hash for ColumnElem {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.name, self.nullable, self.nobox).hash(state);
    }
}

impl ColumnElem {
    pub(crate) fn planned(
        name: Ustr,
        nullable: bool,
        representation: crate::lower::RepId,
        nobox: bool,
    ) -> Self {
        ColumnElem {
            name,
            nullable,
            representation,
            nobox,
        }
    }

    pub fn representation(&self) -> crate::lower::RepId {
        self.representation
    }

    pub fn is_nobox(&self) -> bool {
        self.nobox
    }
}

impl std::fmt::Display for ColumnElem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.name, if self.nullable { "?" } else { "" })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosureEnvPlan {
    captures: Vec<Idx<Cell>>,
    env_cell: Option<Idx<Cell>>,
}

impl ClosureEnvPlan {
    pub fn new(captures: Vec<Idx<Cell>>, env_cell: Option<Idx<Cell>>) -> Self {
        Self { captures, env_cell }
    }

    pub fn captures(&self) -> &[Idx<Cell>] {
        &self.captures
    }

    pub fn env_cell(&self) -> Option<Idx<Cell>> {
        self.env_cell
    }

    pub fn captures_cell(&self, cell: Idx<Cell>) -> bool {
        self.captures.contains(&cell)
    }
}

impl ContainerKind {
    pub fn is_native_buffer(self) -> bool {
        matches!(
            self,
            ContainerKind::NativeBuffer
                | ContainerKind::NativeBufferInBounds
                | ContainerKind::NativeBufferValue(_)
        )
    }
}

/// A plain resume fires only `on_resume`; `on_killed` fires from `kill()` and
/// `on_hot_reload` from `fire_hot_reload()`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CoroHookKind {
    OnResume,
    OnKilled,
    OnHotReload,
    OnOwnedHotReload,
}

impl CoroHookKind {
    pub fn slot(self) -> &'static str {
        match self {
            CoroHookKind::OnResume => "__on_resume",
            CoroHookKind::OnKilled => "__on_killed",
            CoroHookKind::OnHotReload => "__on_hot_reload",
            CoroHookKind::OnOwnedHotReload => "__on_owned_hot_reload",
        }
    }
}

#[derive(Clone, Debug)]
pub struct FlatVecCtor {
    pub elem: ColumnElem,
    pub capacity: Idx<Inst>,
    pub contents: FlatVecContents,
}

impl PartialEq for FlatVecCtor {
    fn eq(&self, other: &Self) -> bool {
        self.elem.name == other.elem.name
            && self.elem.representation() == other.elem.representation()
            && self.elem.is_nobox() == other.elem.is_nobox()
            && self.capacity == other.capacity
            && self.contents == other.contents
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum FlatVecContents {
    Header(Option<usize>),
    Columns {
        columns: Vec<(Ustr, Vec<Idx<Inst>>)>,
        len: usize,
    },
    Rows {
        stride: usize,
        values: Vec<Idx<Inst>>,
        len: usize,
    },
}

impl FlatVecContents {
    pub fn stride(&self) -> Option<usize> {
        match self {
            Self::Header(stride) => *stride,
            Self::Columns { .. } => None,
            Self::Rows { stride, .. } => Some(*stride),
        }
    }

    pub fn repr_name(&self) -> &'static str {
        if self.stride().is_some() {
            VEC_STRIDED_REPR
        } else {
            VEC_COLUMN_REPR
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Inst {
    Const(Lit),
    Global(Ustr),
    Load(Idx<Cell>),

    IntAdd(Idx<Inst>, Idx<Inst>),
    IntSub(Idx<Inst>, Idx<Inst>),
    IntMul(Idx<Inst>, Idx<Inst>),
    IntDiv(Idx<Inst>, Idx<Inst>),
    IntMod(Idx<Inst>, Idx<Inst>),
    UintAdd(Idx<Inst>, Idx<Inst>),
    UintSub(Idx<Inst>, Idx<Inst>),
    UintMul(Idx<Inst>, Idx<Inst>),
    UintDiv(Idx<Inst>, Idx<Inst>),
    UintMod(Idx<Inst>, Idx<Inst>),
    FloatAdd(Idx<Inst>, Idx<Inst>),
    FloatSub(Idx<Inst>, Idx<Inst>),
    FloatMul(Idx<Inst>, Idx<Inst>),
    FloatDiv(Idx<Inst>, Idx<Inst>),
    FloatMod(Idx<Inst>, Idx<Inst>),
    StringConcat(Idx<Inst>, Idx<Inst>),
    /// Scalar holes only; float and composite holes call `__rock_format_float` or
    /// `inspect` instead.
    ToString(Idx<Inst>),
    Format {
        value: Idx<Inst>,
        fmt: Ustr,
    },
    FormatDyn {
        value: Idx<Inst>,
        fmt: Idx<Inst>,
    },

    IntLt(Idx<Inst>, Idx<Inst>),
    IntLeq(Idx<Inst>, Idx<Inst>),
    IntGt(Idx<Inst>, Idx<Inst>),
    IntGeq(Idx<Inst>, Idx<Inst>),
    UintLt(Idx<Inst>, Idx<Inst>),
    UintLeq(Idx<Inst>, Idx<Inst>),
    UintGt(Idx<Inst>, Idx<Inst>),
    UintGeq(Idx<Inst>, Idx<Inst>),
    FloatLt(Idx<Inst>, Idx<Inst>),
    FloatLeq(Idx<Inst>, Idx<Inst>),
    FloatGt(Idx<Inst>, Idx<Inst>),
    FloatGeq(Idx<Inst>, Idx<Inst>),
    /// Value-type contents equality goes through a helper, never a raw `==`.
    Eq(Idx<Inst>, Idx<Inst>),
    NotEq(Idx<Inst>, Idx<Inst>),

    /// Floors, since Lua numbers are doubles. Every other `as` is identity and
    /// emits no op.
    FloatToInt(Idx<Inst>),
    /// Identity on LuaJIT; non-Lua backends materialize the conversion.
    IntToFloat(Idx<Inst>),
    UintToFloat(Idx<Inst>),
    /// Rounds to the nearest `f32` so an `f32` field reads the same in every
    /// storage.
    FloatToF32(Idx<Inst>),
    IntToUint(Idx<Inst>),
    UintToInt(Idx<Inst>),
    FloatToUint(Idx<Inst>),
    UintBitcastInt(Idx<Inst>),
    IntBitcastUint(Idx<Inst>),

    Neg(Idx<Inst>),
    Not(Idx<Inst>),
    BitNot(Idx<Inst>),
    UintBitNot(Idx<Inst>),

    BitAnd(Idx<Inst>, Idx<Inst>),
    BitOr(Idx<Inst>, Idx<Inst>),
    BitXor(Idx<Inst>, Idx<Inst>),
    Shl(Idx<Inst>, Idx<Inst>),
    Shr(Idx<Inst>, Idx<Inst>),
    UintBitAnd(Idx<Inst>, Idx<Inst>),
    UintBitOr(Idx<Inst>, Idx<Inst>),
    UintBitXor(Idx<Inst>, Idx<Inst>),
    UintShl(Idx<Inst>, Idx<Inst>),
    UintShr(Idx<Inst>, Idx<Inst>),

    // Args are a Vec, not an IdxRange: lowered arg values are not contiguous in
    // the arena.
    Call {
        func: FuncRef,
        args: Vec<Idx<Inst>>,
    },
    /// A multi-assign so Lua does not truncate the multi-return.
    CallMulti {
        dests: Vec<Idx<Cell>>,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
    },
    CallValue {
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    },
    /// A multi-assign so Lua does not truncate the multi-return.
    CallValueMulti {
        dests: Vec<Idx<Cell>>,
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    },
    /// Placeholder `next` on an `Iterator`-bounded generic; the inliner substitutes
    /// the concrete callee. Emitting a body that still contains one is an error.
    CallBoundMulti {
        dests: Vec<Idx<Cell>>,
        generic_idx: u32,
        args: Vec<Idx<Inst>>,
    },

    /// The body shares the enclosing fn's arenas, so a captured variable is the
    /// same `Cell`. Its sub-CFG is unreachable from the enclosing entry.
    Lambda {
        params: Vec<Idx<Cell>>,
        entry: Idx<Block>,
    },

    MakeTuple(Vec<Idx<Inst>>),
    /// `{ data = {v0, ...}, len = N }`.
    MakeVec(Vec<Idx<Inst>>),
    MakeFlatVec(Box<FlatVecCtor>),
    /// Physical strided rows cannot contain nil without punching a Lua array
    /// hole, so false is the inactive-lane sentinel.
    VecStrideNilToFalse(Idx<Inst>),
    VecStrideFalseToNil(Idx<Inst>),
    MakeStruct {
        ty: Option<Ustr>,
        fields: Vec<(Ustr, Idx<Inst>)>,
    },
    MakeValueStruct {
        name: Ustr,
        /// Backends must realize this plan, never replan from `name` or the
        /// operand spelling.
        representation: crate::lower::RepId,
        form: ValueCtorForm,
        /// In declared field order: the `<name>_ct` ctor is positional.
        /// In the leaf form (only between cleanup and emit) the names are dotted
        /// leaf paths that must equal the plan's leaf paths exactly.
        fields: Vec<(Ustr, Idx<Inst>)>,
    },
    MakeNullableValue {
        present: Idx<Inst>,
        value: Idx<Inst>,
    },
    /// An authorized box (`materialize`, or a `@gpu_only` call boundary). Emits like
    /// `MakeValueStruct`; the `nobox` verifier ignores it.
    MaterializeValueStruct {
        name: Ustr,
        representation: crate::lower::RepId,
        form: ValueCtorForm,
        fields: Vec<(Ustr, Idx<Inst>)>,
    },
    /// A tagged table `{ __variant, __ty, <payload> }`.
    MakeEnum {
        tag: Ustr,
        ty: Ustr,
        positional: Vec<Idx<Inst>>,
        named: Vec<(Ustr, Idx<Inst>)>,
    },
    /// Nil slots are harmless: Lua omits them, so the box matches the live variant.
    MakeEnumDyn {
        tag: Idx<Inst>,
        ty: Ustr,
        positional: Vec<Idx<Inst>>,
        named: Vec<(Ustr, Idx<Inst>)>,
    },

    /// Produced only by closure conversion, never by lowering. `fn_ref = None`
    /// boxes a mutable shared capture as `{ v = .. }`.
    MakeEnv {
        fn_ref: Option<Ustr>,
        fields: Vec<(Ustr, Idx<Inst>)>,
    },

    FfiNewArray {
        ctype: Ustr,
        count: Idx<Inst>,
        representation: Option<crate::lower::RepId>,
    },

    /// Made once per buffer, not per access: `ffi.cast` allocates a pointer
    /// cdata.
    FfiViewCast {
        /// Aliases the same bytes as `.ptr`, so leaf and slot accesses stay coherent.
        cty: Ustr,
        ptr: Idx<Inst>,
    },

    Field {
        /// The base is non-nil and its fields have no getters, so an unused read may
        /// be removed.
        nonthrowing: bool,
        base: Idx<Inst>,
        field: Ustr,
    },
    /// 0-based; the Lua emitter adds `+1`.
    TupleIndex {
        base: Idx<Inst>,
        index: usize,
    },

    Index {
        base: Idx<Inst>,
        index: Idx<Inst>,
        kind: ContainerKind,
    },
    /// Yields the 1-based backing index; the column reads that follow share this
    /// one check.
    VecCheckedIndex {
        vec: Idx<Inst>,
        index: Idx<Inst>,
    },
    /// Each backend renders its own column base (`index + 1` on Lua, `index` on
    /// JS). Pure and never raises.
    VecIndexSlot(Idx<Inst>),
    /// Pure and never raises.
    NbIndexSlot(Idx<Inst>),
    Len {
        base: Idx<Inst>,
        kind: ContainerKind,
    },

    Store {
        cell: Idx<Cell>,
        value: Idx<Inst>,
    },
    StoreField {
        base: Idx<Inst>,
        field: Ustr,
        value: Idx<Inst>,
        /// The logical leaf name when `field` aliases nested cdata storage.
        native_leaf: Option<Ustr>,
    },
    StoreIndex {
        base: Idx<Inst>,
        index: Idx<Inst>,
        value: Idx<Inst>,
        kind: ContainerKind,
    },
    StoreGlobal {
        name: Ustr,
        value: Idx<Inst>,
    },
    ArrayPush {
        arr: Idx<Inst>,
        value: Idx<Inst>,
        kind: ContainerKind,
    },
    CoroSetInner(Idx<Inst>),
    /// Relocated to the constructor so a kill before the first resume still fires
    /// the hook.
    CoroHook {
        kind: CoroHookKind,
        lambda: Idx<Inst>,
    },
    /// The key cell is both cursor and new key; a nil key signals exhaustion.
    MapNext {
        key_cell: Idx<Cell>,
        val_cell: Idx<Cell>,
        table: Idx<Inst>,
    },
}

macro_rules! each_value_operand {
    ($inst:expr, $iter:ident, $visit:ident, $($mut:tt)?) => {
        match $inst {
            Inst::Const(_) | Inst::Global(_) | Inst::Load(_) => {}
            Inst::FfiViewCast { ptr, .. } => $visit(ptr),
            Inst::IntAdd(a, b)
            | Inst::IntSub(a, b)
            | Inst::IntMul(a, b)
            | Inst::IntDiv(a, b)
            | Inst::IntMod(a, b)
            | Inst::UintAdd(a, b)
            | Inst::UintSub(a, b)
            | Inst::UintMul(a, b)
            | Inst::UintDiv(a, b)
            | Inst::UintMod(a, b)
            | Inst::FloatAdd(a, b)
            | Inst::FloatSub(a, b)
            | Inst::FloatMul(a, b)
            | Inst::FloatDiv(a, b)
            | Inst::FloatMod(a, b)
            | Inst::StringConcat(a, b)
            | Inst::IntLt(a, b)
            | Inst::IntLeq(a, b)
            | Inst::IntGt(a, b)
            | Inst::IntGeq(a, b)
            | Inst::UintLt(a, b)
            | Inst::UintLeq(a, b)
            | Inst::UintGt(a, b)
            | Inst::UintGeq(a, b)
            | Inst::FloatLt(a, b)
            | Inst::FloatLeq(a, b)
            | Inst::FloatGt(a, b)
            | Inst::FloatGeq(a, b)
            | Inst::Eq(a, b)
            | Inst::NotEq(a, b)
            | Inst::BitAnd(a, b)
            | Inst::BitOr(a, b)
            | Inst::BitXor(a, b)
            | Inst::Shl(a, b)
            | Inst::Shr(a, b)
            | Inst::UintBitAnd(a, b)
            | Inst::UintBitOr(a, b)
            | Inst::UintBitXor(a, b)
            | Inst::UintShl(a, b)
            | Inst::UintShr(a, b) => {
                $visit(a);
                $visit(b);
            }
            Inst::Format { value, .. } => $visit(value),
            Inst::FormatDyn { value, fmt } => {
                $visit(value);
                $visit(fmt);
            }
            Inst::Neg(value)
            | Inst::Not(value)
            | Inst::BitNot(value)
            | Inst::UintBitNot(value)
            | Inst::ToString(value)
            | Inst::FloatToInt(value)
            | Inst::IntToFloat(value)
            | Inst::UintToFloat(value)
            | Inst::FloatToF32(value)
            | Inst::IntToUint(value)
            | Inst::UintToInt(value)
            | Inst::FloatToUint(value)
            | Inst::UintBitcastInt(value)
            | Inst::IntBitcastUint(value)
            | Inst::VecStrideNilToFalse(value)
            | Inst::VecStrideFalseToNil(value)
            | Inst::VecIndexSlot(value)
            | Inst::NbIndexSlot(value) => $visit(value),
            Inst::Call { args, .. }
            | Inst::CallMulti { args, .. }
            | Inst::CallBoundMulti { args, .. } => args.$iter().for_each(&mut $visit),
            Inst::CallValue { callee, args } | Inst::CallValueMulti { callee, args, .. } => {
                $visit(callee);
                args.$iter().for_each(&mut $visit);
            }
            Inst::Lambda { .. } => {}
            Inst::MakeFlatVec(vec) => {
                $visit(& $($mut)? vec.capacity);
                match & $($mut)? vec.contents {
                    FlatVecContents::Columns { columns, .. } => columns.$iter()
                        .flat_map(|(_, values)| values.$iter())
                        .for_each(&mut $visit),
                    FlatVecContents::Rows { values, .. } => values.$iter().for_each(&mut $visit),
                    FlatVecContents::Header(_) => {}
                }
            }
            Inst::MakeTuple(values) | Inst::MakeVec(values) => {
                values.$iter().for_each(&mut $visit);
            }
            Inst::MakeStruct { fields, .. }
            | Inst::MakeValueStruct { fields, .. }
            | Inst::MaterializeValueStruct { fields, .. }
            | Inst::MakeEnv { fields, .. } => {
                fields.$iter().for_each(|(_, value)| $visit(value));
            }
            Inst::MakeNullableValue { present, value } => {
                $visit(present);
                $visit(value);
            }
            Inst::MakeEnum {
                positional, named, ..
            } => {
                positional.$iter().for_each(&mut $visit);
                named.$iter().for_each(|(_, value)| $visit(value));
            }
            Inst::MakeEnumDyn {
                tag,
                positional,
                named,
                ..
            } => {
                $visit(tag);
                positional.$iter().for_each(&mut $visit);
                named.$iter().for_each(|(_, value)| $visit(value));
            }
            Inst::FfiNewArray { count, .. } => $visit(count),
            Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => $visit(base),
            Inst::Index { base, index, .. } | Inst::VecCheckedIndex { vec: base, index } => {
                $visit(base);
                $visit(index);
            }
            Inst::Len { base, .. } => $visit(base),
            Inst::Store { value, .. } | Inst::StoreGlobal { value, .. } => $visit(value),
            Inst::StoreField { base, value, .. } => {
                $visit(base);
                $visit(value);
            }
            Inst::StoreIndex {
                base, index, value, ..
            } => {
                $visit(base);
                $visit(index);
                $visit(value);
            }
            Inst::ArrayPush { arr, value, .. } => {
                $visit(arr);
                $visit(value);
            }
            Inst::MapNext { table, .. } => $visit(table),
            Inst::CoroSetInner(value) => $visit(value),
            Inst::CoroHook { .. } => {}
        }
    };
}

impl Inst {
    pub fn cells_each(&self, push: &mut impl FnMut(Idx<Cell>)) {
        match self {
            Inst::Load(cell) | Inst::Store { cell, .. } => push(*cell),
            Inst::CallMulti { dests, .. }
            | Inst::CallValueMulti { dests, .. }
            | Inst::CallBoundMulti { dests, .. } => {
                dests.iter().for_each(|cell| push(*cell));
            }
            Inst::Lambda { params, .. } => params.iter().for_each(|cell| push(*cell)),
            Inst::MapNext {
                key_cell, val_cell, ..
            } => {
                push(*key_cell);
                push(*val_cell);
            }
            _ => {}
        }
    }

    /// `MapNext` reads its key cell (the cursor) before writing it.
    pub fn cell_reads_each(&self, push: &mut impl FnMut(Idx<Cell>)) {
        match self {
            Inst::Load(c) => push(*c),
            Inst::MapNext { key_cell, .. } => push(*key_cell),
            _ => {}
        }
    }

    pub fn cell_writes_each(&self, push: &mut impl FnMut(Idx<Cell>)) {
        match self {
            Inst::Store { cell, .. } => push(*cell),
            Inst::CallMulti { dests, .. }
            | Inst::CallValueMulti { dests, .. }
            | Inst::CallBoundMulti { dests, .. } => {
                dests.iter().for_each(|cell| push(*cell));
            }
            Inst::MapNext {
                key_cell, val_cell, ..
            } => {
                push(*key_cell);
                push(*val_cell);
            }
            _ => {}
        }
    }

    pub fn uses_each(&self, push: &mut impl FnMut(Idx<Inst>)) {
        let mut visit = |value: &Idx<Inst>| push(*value);
        each_value_operand!(self, iter, visit,)
    }

    pub fn remap_uses(&mut self, map: &impl Fn(Idx<Inst>) -> Idx<Inst>) {
        let mut visit = |value: &mut Idx<Inst>| *value = map(*value);
        each_value_operand!(self, iter_mut, visit, mut)
    }

    pub fn produces_value(&self) -> bool {
        !matches!(
            self,
            Inst::Store { .. }
                | Inst::StoreField { .. }
                | Inst::StoreIndex { .. }
                | Inst::StoreGlobal { .. }
                | Inst::MapNext { .. }
                | Inst::CallMulti { .. }
                | Inst::CallBoundMulti { .. }
                | Inst::CallValueMulti { .. }
                | Inst::CoroSetInner { .. }
                | Inst::CoroHook { .. }
                | Inst::ArrayPush { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub name: Option<Ustr>,
    pub capture_identity: Option<crate::code_identity::CaptureIdentity>,
    /// A direct capture of a loop binding is per-iteration by value, so ownership
    /// analysis excludes it from boxing.
    pub loop_binding: bool,
    /// `None` for compiler temps, whose type typed backends infer from the first
    /// store. The Lua backend ignores it.
    pub ty: Option<TyId>,
    /// Licenses SROA to rematerialize an escaping load at the use site. Only a
    /// wrongly-set flag can miscompile, so leave it unset when in doubt.
    pub nonnull_value_struct: bool,
    /// Closure ownership treats the default-fill store as initialization, not
    /// mutation.
    pub default_param: bool,
    /// Drops the nil-write guard on field stores. Leave it unset when in doubt: a
    /// wrongly-set flag lets a nil store insert an absent key.
    pub never_nil: bool,
    /// A write through the `ref` alias is invisible to transforms, so SROA must not
    /// split this cell and copy-prop must not forward its loads.
    pub borrowed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Term {
    Return(Vec<Idx<Inst>>),
    /// Lowers to a bare `return`.
    Unreachable,
    /// No args: merges go through cells, not block params.
    Goto(Idx<Block>),
    Branch {
        cond: Idx<Inst>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
    },
    /// Pre-rewrite only: the coroutine transform removes every `Yield`.
    Yield {
        values: Vec<Idx<Inst>>,
        resume: Idx<Block>,
    },
}

impl Term {
    pub fn uses_each(&self, push: &mut impl FnMut(Idx<Inst>)) {
        match self {
            Term::Return(values) | Term::Yield { values, .. } => {
                values.iter().for_each(|value| push(*value));
            }
            Term::Branch { cond, .. } => push(*cond),
            Term::Unreachable | Term::Goto(_) => {}
        }
    }

    pub fn remap_uses(&mut self, map: &impl Fn(Idx<Inst>) -> Idx<Inst>) {
        match self {
            Term::Return(values) | Term::Yield { values, .. } => {
                values.iter_mut().for_each(|value| *value = map(*value));
            }
            Term::Branch { cond: value, .. } => {
                *value = map(*value);
            }
            Term::Unreachable | Term::Goto(_) => {}
        }
    }

    pub fn remap_targets(&mut self, map: &impl Fn(Idx<Block>) -> Idx<Block>) {
        match self {
            Term::Goto(target) | Term::Yield { resume: target, .. } => *target = map(*target),
            Term::Branch {
                then_bb, else_bb, ..
            } => {
                *then_bb = map(*then_bb);
                *else_bb = map(*else_bb);
            }
            Term::Return(_) | Term::Unreachable => {}
        }
    }
}

/// No params: every cross-block value goes through a cell.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub insts: Vec<Idx<Inst>>,
    pub term: Term,
}

impl Block {
    pub fn new() -> Self {
        Block {
            insts: Vec::new(),
            term: Term::Unreachable,
        }
    }
}

impl Default for Block {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StaticCell {
    pub cell: Ustr,
    pub done_flag: Ustr,
    pub ty: Option<bone_core::prelude::TyId>,
    /// Name and ordinal are the logical identity; synthesized cell names are
    /// realization only.
    pub logical_name: Ustr,
    pub ordinal: u32,
    pub type_str: String,
    /// Hashes names and literals, not arena indices, so it is stable across edits
    /// elsewhere in the body.
    pub init_hash: u64,
    pub owner: Option<crate::code_identity::CodeIdentity>,
}

/// Physical indices stay dense for side tables; these preserve source arena
/// order at the output boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmitIds {
    pub insts: Vec<u32>,
    pub cells: Vec<u32>,
    pub blocks: Vec<u32>,
    pub forced_cells: Vec<u32>,
    pub forced_cell_blocks: Vec<u32>,
    pub next_insts: u32,
    pub next_cells: u32,
    pub next_blocks: u32,
}

/// bone hashes only source text, so incremental invalidation must compare these
/// against the current inputs.
#[derive(Clone, Debug, PartialEq)]
pub struct ComptimeRead {
    pub input: ComptimeInputId,
    pub value_hash: u64,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ComptimeInputId {
    Var(Ustr),
    File(Ustr),
}

/// The one hash both the recorder and the staleness check use.
pub fn comptime_value_hash(value: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}
#[derive(Clone, Debug, PartialEq)]
pub struct LambdaAbi {
    pub fingerprint: String,
    pub scalar_entry: Option<crate::lower::ScalarEntryPlan>,
    pub representation: crate::lower::FunctionRepPlan,
    /// Copies of one HIR lambda share this origin. Macro siblings may share a span.
    pub source: Option<Idx<bone_core::hir::Expr>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MirFunc {
    pub name: Ustr,
    /// A private nested body reached only by compiler-generated direct calls.
    pub direct_only: bool,
    pub code_identity: Option<crate::code_identity::CodeIdentity>,
    /// Contains no backend symbol or build-local `RepId`.
    pub code_abi_fingerprint: Option<String>,
    pub scalar_entry: Option<crate::lower::ScalarEntryPlan>,
    pub lambda_abis: bone_core::prelude::FxHashMap<Idx<Inst>, LambdaAbi>,
    /// Stores that begin a lexical binding's lifetime.
    pub binding_initializers: bone_core::prelude::FxHashSet<Idx<Inst>>,
    pub code_value_identities:
        bone_core::prelude::FxHashMap<Idx<Inst>, crate::code_identity::CodeIdentity>,
    pub nested_fn_identities:
        bone_core::prelude::FxHashMap<Ustr, crate::code_identity::CodeIdentity>,
    pub code_compatibility: Option<crate::code_identity::CodeCompatibility>,
    pub rep_arena: Option<Arc<crate::lower::RepArena>>,
    pub params: Vec<Idx<Cell>>,
    /// Backends with typed parameters cannot use the Lua/JS nil sentinel.
    pub materialized_default_guards: bone_core::prelude::FxHashSet<Idx<Inst>>,
    pub abi_plan: Option<crate::lower::BackendFnAbiPlanHandle>,
    pub abi_representation: Option<crate::lower::FunctionRepPlan>,
    pub statics: Vec<StaticCell>,
    pub comptime_reads: Vec<ComptimeRead>,
    pub struct_registry_roots: std::collections::BTreeSet<Ustr>,
    pub insts: Arena<Inst>,
    pub cells: Arena<Cell>,
    pub blocks: Arena<Block>,
    pub emit_ids: Option<EmitIds>,
    pub inline_cleanup_load_cells: Vec<Idx<Cell>>,
    pub entry: Idx<Block>,
    pub is_coroutine: bool,
    pub resume_arg: Option<Idx<Cell>>,
    /// Each entry block is unreachable from `entry`. The flag is set where the
    /// `defer` runs and cleared at scope exit, so terminal cleanup runs only pending defers.
    pub coro_defers: Vec<(Idx<Cell>, Idx<Block>)>,
    /// Only a desugared iterator loop may have its bound snapshot-hoisted, never a
    /// user-written `while`.
    pub for_iter_headers: Vec<Idx<Block>>,
    pub bound_trees: bone_core::prelude::FxHashMap<Idx<Inst>, bone_core::prelude::BoundTree>,
    /// Already resolved: a raw `BoundTree`'s `TyId`s belong to the arena that
    /// recorded it.
    pub resolved_bound_trees:
        bone_core::prelude::FxHashMap<Idx<Inst>, crate::bound::ResolvedBoundTree>,
    /// A surviving un-spliced entry is a hard rejection: an opaque delivery cannot
    /// receive splatted args.
    pub spec_bindings: bone_core::prelude::FxHashMap<Idx<Inst>, crate::instance::Instance>,
    /// Identity, recorded regardless of delivery policy; `spec_bindings` is a
    /// policy artifact and must not be read as identity.
    pub callee_instances: bone_core::prelude::FxHashMap<Idx<Inst>, crate::instance::Instance>,
    /// Kept because rebuilding from a later, mutated owner would make an ABI
    /// corruption look self-consistent.
    pub call_abi_plans:
        bone_core::prelude::FxHashMap<Idx<Inst>, crate::lower::BackendFnAbiPlanHandle>,
    pub call_rep_plans: bone_core::prelude::FxHashMap<Idx<Inst>, crate::lower::FunctionRepPlan>,
    /// Result width for a call whose physical lane count differs from its source ABI.
    pub flat_call_lanes: bone_core::prelude::FxHashMap<Idx<Inst>, usize>,
    pub closure_env_plans: bone_core::prelude::FxHashMap<Idx<Inst>, ClosureEnvPlan>,
    /// Not derivable from `dests.len()`: a one-leaf element is two dests either way.
    pub bound_next_split: bone_core::prelude::FxHashSet<Idx<Inst>>,
    /// Unused ones are dropped without keeping one to raise a nil error the source
    /// could not have asked for.
    pub synthetic_projections: bone_core::prelude::FxHashSet<Idx<Inst>>,
    pub flat_box_widths: bone_core::prelude::FxHashMap<Idx<Inst>, usize>,
    pub nested_fns: Vec<MirFunc>,
    /// Stamped for every lowered instruction because transforms rewrite
    /// instructions in place.
    pub inst_spans: bone_core::prelude::ArenaMap<Idx<Inst>, bone_core::prelude::Span>,
    /// A span cannot key this: a `for` head's `iter` and `next` share one span.
    pub call_sites: bone_core::prelude::FxHashMap<Idx<Inst>, crate::site::CallSite>,
    pub fn_value_symbols: bone_core::prelude::FxHashMap<Idx<Inst>, FuncRef>,
    pub hoisted_fn_globals: Vec<Ustr>,
    pub flat_variant_source: Option<FuncRef>,
    /// Index 0 is this body's own owner when it has one.
    pub site_owners: Vec<crate::instance::Instance>,
}

/// A body that pushes runs under `DEFER_CALL`, which runs the still-registered
/// cleanups when an error escapes and re-raises it.
pub const DEFER_PUSH: &str = "__rock_defer_push";
pub const DEFER_POP: &str = "__rock_defer_pop";
pub const DEFER_CALL: &str = "__rock_defer_call";
pub const CORO_KILL: &str = "__rock_coroutine_kill";
pub const CORO_HOT_RELOAD: &str = "__rock_coroutine_fire_hot_reload";

impl MirFunc {
    /// Does not establish purity or authorize removing the original's effects.
    pub fn field_projection(&self, id: Idx<Inst>) -> Option<(Idx<Inst>, Ustr)> {
        match self.insts[id] {
            Inst::Field { base, field, .. } => Some((base, field)),
            Inst::Index {
                base,
                index,
                kind: ContainerKind::Record(_),
            } => match self.insts[index] {
                Inst::Const(Lit::Str(field)) => Some((base, field)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Read off the final MIR, so an inlined callee's registrations protect its
    /// caller.
    pub fn registers_defer_cleanup(&self, blocks: &[Idx<Block>]) -> bool {
        blocks.iter().any(|&b| {
            self.blocks[b].insts.iter().any(|&i| match &self.insts[i] {
                Inst::CallValue { callee, .. } => {
                    matches!(&self.insts[*callee], Inst::Global(n) if n.as_str() == DEFER_PUSH)
                }
                _ => false,
            })
        })
    }

    pub fn new(name: Ustr) -> Self {
        let mut blocks = Arena::default();
        let entry = blocks.alloc(Block::new());
        Self {
            name,
            direct_only: false,
            code_identity: None,
            code_abi_fingerprint: None,
            scalar_entry: None,
            lambda_abis: Default::default(),
            binding_initializers: Default::default(),
            code_value_identities: Default::default(),
            nested_fn_identities: Default::default(),
            code_compatibility: None,
            rep_arena: None,
            params: Vec::new(),
            materialized_default_guards: Default::default(),
            abi_plan: None,
            abi_representation: None,
            statics: Vec::new(),
            comptime_reads: Vec::new(),
            struct_registry_roots: Default::default(),
            insts: Arena::default(),
            cells: Arena::default(),
            blocks,
            emit_ids: None,
            inline_cleanup_load_cells: Vec::new(),
            entry,
            is_coroutine: false,
            resume_arg: None,
            coro_defers: Vec::new(),
            for_iter_headers: Vec::new(),
            bound_trees: Default::default(),
            resolved_bound_trees: Default::default(),
            spec_bindings: Default::default(),
            bound_next_split: Default::default(),
            synthetic_projections: Default::default(),
            flat_box_widths: Default::default(),
            nested_fns: Vec::new(),
            inst_spans: Default::default(),
            call_sites: Default::default(),
            callee_instances: Default::default(),
            call_abi_plans: Default::default(),
            call_rep_plans: Default::default(),
            flat_call_lanes: Default::default(),
            closure_env_plans: Default::default(),
            fn_value_symbols: Default::default(),
            hoisted_fn_globals: Vec::new(),
            flat_variant_source: None,
            site_owners: Vec::new(),
        }
    }

    pub fn entry(&self) -> Idx<Block> {
        self.entry
    }

    pub fn new_cell(&mut self, name: Option<Ustr>) -> Idx<Cell> {
        self.new_cell_typed(name, None)
    }

    pub(crate) fn new_cell_typed(&mut self, name: Option<Ustr>, ty: Option<TyId>) -> Idx<Cell> {
        let id = self.cells.alloc(Cell {
            name,
            capture_identity: None,
            loop_binding: false,
            ty,
            nonnull_value_struct: false,
            default_param: false,
            never_nil: false,
            borrowed: false,
        });
        self.record_new_cell();
        id
    }

    pub fn mark_loop_binding(&mut self, cell: Idx<Cell>) {
        self.cells[cell].loop_binding = true;
    }

    pub(crate) fn mark_default_param(&mut self, cell: Idx<Cell>) {
        self.cells[cell].default_param = true;
    }

    pub fn mark_nonnull_value_struct(&mut self, cell: Idx<Cell>) {
        self.cells[cell].nonnull_value_struct = true;
    }

    pub(crate) fn mark_borrowed(&mut self, cell: Idx<Cell>) {
        self.cells[cell].borrowed = true;
    }

    pub fn new_block(&mut self) -> Idx<Block> {
        let id = self.blocks.alloc(Block::new());
        self.record_new_block();
        id
    }

    pub fn emit(&mut self, block: Idx<Block>, inst: Inst) -> Idx<Inst> {
        let id = self.insts.alloc(inst);
        self.record_new_inst();
        self.blocks[block].insts.push(id);
        id
    }

    pub fn alloc_inst(&mut self, inst: Inst) -> Idx<Inst> {
        let id = self.insts.alloc(inst);
        self.record_new_inst();
        id
    }

    pub fn set_term(&mut self, block: Idx<Block>, term: Term) {
        self.blocks[block].term = term;
    }

    pub fn ensure_emit_ids(&mut self) {
        if self.emit_ids.is_none() {
            self.emit_ids = Some(EmitIds {
                insts: (0..self.insts.len()).map(|id| id as u32).collect(),
                cells: (0..self.cells.len()).map(|id| id as u32).collect(),
                blocks: (0..self.blocks.len()).map(|id| id as u32).collect(),
                forced_cells: Vec::new(),
                forced_cell_blocks: Vec::new(),
                next_insts: self.insts.len() as u32,
                next_cells: self.cells.len() as u32,
                next_blocks: self.blocks.len() as u32,
            });
        }
    }

    pub fn emit_inst_id(&self, id: Idx<Inst>) -> u32 {
        self.emit_ids
            .as_ref()
            .map_or(u32::from(id.into_raw()), |ids| {
                ids.insts[u32::from(id.into_raw()) as usize]
            })
    }

    pub fn emit_cell_id(&self, id: Idx<Cell>) -> u32 {
        self.emit_ids
            .as_ref()
            .map_or(u32::from(id.into_raw()), |ids| {
                ids.cells[u32::from(id.into_raw()) as usize]
            })
    }

    pub fn emit_block_id(&self, id: Idx<Block>) -> u32 {
        self.emit_ids
            .as_ref()
            .map_or(u32::from(id.into_raw()), |ids| {
                ids.blocks[u32::from(id.into_raw()) as usize]
            })
    }

    pub fn emit_inst_span(&self) -> u32 {
        self.emit_ids
            .as_ref()
            .map_or(self.insts.len() as u32, |ids| ids.next_insts)
    }

    pub fn emit_cell_span(&self) -> u32 {
        self.emit_ids
            .as_ref()
            .map_or(self.cells.len() as u32, |ids| ids.next_cells)
    }

    pub fn emit_block_span(&self) -> u32 {
        self.emit_ids
            .as_ref()
            .map_or(self.blocks.len() as u32, |ids| ids.next_blocks)
    }

    pub fn emit_nexts(&self) -> Option<(u32, u32, u32)> {
        self.emit_ids
            .as_ref()
            .map(|ids| (ids.next_insts, ids.next_cells, ids.next_blocks))
    }

    pub fn set_emit_nexts(&mut self, nexts: (u32, u32, u32)) {
        let ids = self
            .emit_ids
            .as_mut()
            .expect("emit identities must be enabled");
        ids.next_insts = nexts.0;
        ids.next_cells = nexts.1;
        ids.next_blocks = nexts.2;
    }

    pub fn alloc_cell_with_emit_id(&mut self, cell: Cell, emit_id: u32) -> Idx<Cell> {
        let id = self.cells.alloc(cell);
        self.emit_ids
            .as_mut()
            .expect("emit identities must be enabled")
            .cells
            .push(emit_id);
        id
    }

    pub fn alloc_inst_with_emit_id(&mut self, inst: Inst, emit_id: u32) -> Idx<Inst> {
        let id = self.insts.alloc(inst);
        self.emit_ids
            .as_mut()
            .expect("emit identities must be enabled")
            .insts
            .push(emit_id);
        id
    }

    pub fn alloc_block_with_emit_id(&mut self, emit_id: u32) -> Idx<Block> {
        let id = self.blocks.alloc(Block::new());
        self.emit_ids
            .as_mut()
            .expect("emit identities must be enabled")
            .blocks
            .push(emit_id);
        id
    }

    fn record_new_inst(&mut self) {
        if let Some(ids) = &mut self.emit_ids {
            ids.insts.push(ids.next_insts);
            ids.next_insts += 1;
        }
    }

    fn record_new_cell(&mut self) {
        if let Some(ids) = &mut self.emit_ids {
            ids.cells.push(ids.next_cells);
            ids.next_cells += 1;
        }
    }

    fn record_new_block(&mut self) {
        if let Some(ids) = &mut self.emit_ids {
            ids.blocks.push(ids.next_blocks);
            ids.next_blocks += 1;
        }
    }
}
