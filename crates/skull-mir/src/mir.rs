//! skull MIR: HIR, flattened into a cell-model CFG.
//!
//! Faithful to `docs/bone-mir-design.md`:
//!   - A function is a graph of [`Block`]s; control flow lives ONLY in a block's
//!     [`Term`]inator.
//!   - An instruction's `Idx<Inst>` **IS its value** (exactly like `Idx<Expr>`
//!     in HIR). There is no separate `Value`/`Op` wrapper. A value-producing
//!     `Inst` is referenced by its id; an effectful one ([`Inst::Store`]) has an
//!     id nobody references.
//!   - **Variables are mutable [`Cell`]s** (a cell = a Lua local): read with
//!     [`Inst::Load`], written with [`Inst::Store`]. Loop-carried updates and
//!     reassignment ride on cells with no SSA construction.
//!   - **Expression results are single-use values** born and consumed within one
//!     block. Anything that must cross a block boundary (an `if`-as-value result,
//!     a loop-carried var) is a cell — so merges go through cells, **not phi**.
//!     [`Block`] therefore has **no params**; SSA + block params arrive later as
//!     an additive `mem2reg` pass.
//!   - Ops are **typed at build time** (decision 8): `int` vs `float` vs `string`
//!     arithmetic are distinct `Inst`s, so the backend never looks up a type.
//!   - Calls carry a [`FuncRef`] (module + function index), **not a mangled
//!     string** (decision: mangling is the codegen's job).
//!
//! The taxonomy grows as lowering phases land (constructors, container ops,
//! cast, stringify/format, inline-lua) — see the plan's phase checklist.

use bone_semantic::hir::TyId;
use bone_semantic::prelude::Span;
use comfy_la_arena::{Arena, Idx};
use std::sync::Arc;
use std::time::Instant;
use ustr::Ustr;

pub const VEC_COLUMN_HEADER_FIELD: &str = "@vec";

/// Physical constructor spelling selected by lowering.  This is part of the
/// plan identity; backends must not infer it from field names or operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueCtorForm {
    Declared,
    Leaves,
}
pub const VEC_COLUMN_REPR: &str = "columns";
pub const VEC_STRIDED_REPR: &str = "strided";
/// The PRESENCE column of a `[E?]` whose `E` is `nobox`: one bool per slot,
/// leading the leaf columns exactly as the presence leaf leads a nullable
/// shape's leaves. `@` cannot appear in a Rock identifier, so the key can never
/// collide with a leaf path, and it shares that property with `@vec`.
pub const VEC_COLUMN_PRESENCE_FIELD: &str = "@p";

/// Every element type `mf` builds a column-stored `Vec` of, nested fns included,
/// together with the planner's no-box guarantee. The fact is carried by both
/// column-producing instructions, including a header rebuilt from an input Vec.
pub fn column_elem_plans(mf: &MirFunc, out: &mut std::collections::BTreeMap<Ustr, bool>) {
    for inst in mf.insts.values() {
        match inst {
            Inst::MakeVecColumns { elem, nobox, .. }
            | Inst::MakeVecColumnHeader { elem, nobox, .. } => {
                out.insert(*elem, *nobox);
            }
            _ => {}
        }
    }
    for nf in &mf.nested_fns {
        column_elem_plans(nf, out);
    }
}

/// The same census with the build-owned representation identity retained for
/// consumers that emit or inspect the physical columns.
pub fn column_elem_rep_plans(
    mf: &MirFunc,
    out: &mut std::collections::BTreeMap<Ustr, (bool, crate::lower::RepId)>,
) {
    for inst in mf.insts.values() {
        match inst {
            Inst::MakeVecColumns { elem, nobox, representation, .. }
            | Inst::MakeVecColumnHeader { elem, nobox, representation, .. } => {
                out.insert(*elem, (*nobox, *representation));
            }
            _ => {}
        }
    }
    for nf in &mf.nested_fns {
        column_elem_rep_plans(nf, out);
    }
}

/// Every element type `mf` builds a column-stored `Vec`, nested fns included.
/// This is the name-only view used by inspect type definitions; representation
/// decisions come from [`column_elem_plans`].
pub fn column_elem_types(mf: &MirFunc, out: &mut std::collections::BTreeSet<Ustr>) {
    let mut plans = std::collections::BTreeMap::new();
    column_elem_plans(mf, &mut plans);
    out.extend(plans.into_keys());
}

/// A resolved call target: which function in which module. Mangling (rock's
/// overload scheme) is deferred to codegen, which also reads `is_extern` /
/// `lua_body` off this ref to pick the emission shape. (bone-mir-design line
/// "MIR `Call` carries `FuncRef`, NOT a string".)
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FuncRef {
    pub module: bone_semantic::prelude::ModuleId,
    pub func: Idx<bone_semantic::hir::Function>,
}

/// A literal value.
#[derive(Clone, Debug, PartialEq)]
pub enum Lit {
    Int(i64),
    Uint(u32),
    Float(f64),
    Bool(bool),
    Str(Ustr),
    Nil,
}

/// How a collection is represented in the target, so index/length ops emit the
/// right shape (decision 9: the IR is 0-based; the Lua printer adds `+1` on
/// sequence access only, never on `Map` keys or arithmetic). Determined from the
/// base's inferred type at lower time, so the backend never guesses.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ContainerKind {
    /// `[T]` — stdlib `{ data = {..1-indexed..}, len = N }`.
    Vec,
    /// `NativeBuffer<T>` — `{ ptr, len }`; `.ptr` is a 0-based C array.
    NativeBuffer,
    /// `NativeBuffer<E>` where `E` is an FFI-clean value STRUCT (`elem` = its
    /// name). Same `{ ptr, len }` shape; the Lua emitter renders it EXACTLY like
    /// [`ContainerKind::NativeBuffer`] (element access is a cdata ref). Carried
    /// so the JS emitter can lower element access into flat-slot arithmetic over
    /// the single `Float32Array(n * k)` backing (Arc-2 P3 representation), which
    /// has no per-element cdata view.
    NativeBufferValue(Ustr),
    /// `[E]` where `E` is an all-scalar `value` struct (`elem` = its name):
    /// element storage is FLAT — one ordinary 1-indexed Lua array per leaf,
    /// `{ <leaf0> = {..}, .., len = n }` — so no per-element table exists.
    /// Replaces the single `_data` backing of [`ContainerKind::Vec`].
    /// Eligibility + layout rationale: `docs/vec-value-columns-plan.md`.
    VecValueColumns(ColumnElem),
    /// `[E]` where `E` is a wide flat enum with non-false scalar payload leaves.
    /// Elements occupy fixed-width rows in one dense backing array; inactive
    /// enum lanes store `false`, never `nil`.
    VecValueStrided(ColumnElem),
    /// One leaf column OF a [`ContainerKind::VecValueColumns`] Vec — a bare
    /// 1-indexed Lua array. The index is ALREADY 1-based and already
    /// bounds-checked (`__vec_checked_index` against the owning Vec's `len`),
    /// so this indexes raw: no `+1`, no check. Never a user-facing type.
    VecColumn,
    /// One raw slot in a strided Vec backing. The index is already translated
    /// to the backend's physical array base and bounds were checked by the
    /// owning logical element access.
    VecStrideSlot,
    /// One typed LEAF VIEW over a `NativeBuffer`'s storage — a raw C pointer of
    /// a primitive type ([`Inst::FfiViewCast`]), indexed in units of that type.
    /// The index is absolute (`i * stride + leaf_off`, both compile-time
    /// constants from the element's C layout) and already bounds-checked once
    /// per ELEMENT access, so this indexes raw: no `+1`, no check.
    ///
    /// This is what makes an element access allocate nothing: indexing a
    /// primitive pointer yields a number, where indexing a struct-typed pointer
    /// yields a reference cdata. Never a user-facing type.
    /// See `docs/native-buffer-leaf-addressing-plan.md`.
    NativeBufferLeaf,
    /// `Map<K,V>` — keyed directly by K, no `+1`.
    Map,
}

/// The element of a column-stored `[E]` / `[E?]`: the value struct's name, and
/// whether the ELEMENT type is nullable. Nullability travels with the element
/// because it decides the column SET (a nullable element leads with
/// [`VEC_COLUMN_PRESENCE_FIELD`]) — reading it back off an expression's type at
/// a use site would be the phantom-presence bug.
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
        ColumnElem { name, nullable, representation, nobox }
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

/// The capture transport selected for one closure value. The transform owns the
/// scope analysis; MIR retains the resulting plan so later closure, inline, and
/// backend stages do not rediscover which cells cross the closure boundary.
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
    /// Either NativeBuffer flavor — the `{ ptr, len }` C-array shape.
    pub fn is_native_buffer(self) -> bool {
        matches!(self, ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_))
    }

    /// Either Vec flavor — the `{ .., len }` growable shape, 1-indexed.
    pub fn is_vec(self) -> bool {
        matches!(
            self,
            ContainerKind::Vec
                | ContainerKind::VecValueColumns(_)
                | ContainerKind::VecValueStrided(_)
        )
    }
}

/// Which `co fn` lifecycle hook a [`Inst::CoroHook`] registers. All three
/// relocate to the constructor as `s.__on_<kind> = function() .. end`; they
/// differ only in WHEN the runtime fires them — `on_resume` at the top of each
/// `resume()`, `on_killed` from `kill()`, `on_hot_reload` from
/// `fire_hot_reload()`. A hook is NEVER fired by a plain resume except
/// `on_resume`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CoroHookKind {
    OnResume,
    OnKilled,
    OnHotReload,
}

impl CoroHookKind {
    /// The state-table slot this hook lands in (`__on_killed`, ...).
    pub fn slot(self) -> &'static str {
        match self {
            CoroHookKind::OnResume => "__on_resume",
            CoroHookKind::OnKilled => "__on_killed",
            CoroHookKind::OnHotReload => "__on_hot_reload",
        }
    }
}

/// One instruction. Fixed-arity operands are `Idx<Inst>` (the producing
/// instruction); variadic operands (call args) are an [`IdxRange<Inst>`],
/// mirroring HIR's `IdxRange<Expr>`. Control flow is NOT here — it lives in
/// [`Term`].
#[derive(Clone, Debug, PartialEq)]
pub enum Inst {
    Const(Lit),
    /// Read a global (module-level binding) by name.
    Global(Ustr),
    /// Read a variable cell.
    Load(Idx<Cell>),

    // --- typed arithmetic (decision 8: built-time int/float/string split) ---
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
    /// Lua `..` — synthesized for `string + string` and interpolation folding.
    StringConcat(Idx<Inst>, Idx<Inst>),
    /// `tostring(v)` — the default `{x}` stringify for a scalar (int/bool/string)
    /// interpolation hole. Float and composite holes lower to a [`Inst::Call`] of
    /// the stdlib `__rock_format_float` / `inspect` helper instead (so their
    /// rock-faithful formatting is reused), not to this op.
    ToString(Idx<Inst>),
    /// `string.format("<fmt>", value)` — a static interpolation format spec
    /// (`{x:.2f}` etc.). The printf conversion string is baked at build time
    /// (D8: the format dispatch is decided HERE, not at the backend).
    Format { value: Idx<Inst>, fmt: Ustr },
    /// `string.format(<fmt>, value)` — a format spec with a runtime-assembled
    /// conversion string (dynamic width/precision `{x:.{p}f}`); `fmt` is the
    /// already-built format-string value.
    FormatDyn { value: Idx<Inst>, fmt: Idx<Inst> },

    // --- typed comparisons (yield a real bool) ---
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
    /// `==` / `!=`. Scalar reps compare with the target's native equality;
    /// value-type contents-equality routes through a helper (added with value
    /// types) rather than reaching the backend as a raw `==`.
    Eq(Idx<Inst>, Idx<Inst>),
    NotEq(Idx<Inst>, Idx<Inst>),

    // --- casts (`as`) ---
    /// `float as int` — truncates toward -inf (Lua numbers are all doubles), so
    /// it floors. Every other `as` is identity at runtime and lowers to no op
    /// (the inner value is returned directly).
    FloatToInt(Idx<Inst>),
    /// int->float subsumption (`fn f(i: int) -> float { return i }`,
    /// `takes_float(int_expr)`, an int literal in a float slot). Inserted at
    /// lowering by the `coerced` funnel at every sink where bone typed the value
    /// int and the flow position float. Identity on luajit (all doubles), so
    /// codegen-lua renders it as the operand verbatim; non-Lua backends
    /// materialize the conversion (GLSL: fold an int Const to float text, else
    /// wrap `float(...)`). See `docs/shaders.md`.
    IntToFloat(Idx<Inst>),
    UintToFloat(Idx<Inst>),
    /// Round a float to the nearest `f32` and back: the write into an `f32`
    /// field, so its value is the same in every storage.
    FloatToF32(Idx<Inst>),
    IntToUint(Idx<Inst>),
    UintToInt(Idx<Inst>),
    FloatToUint(Idx<Inst>),
    UintBitcastInt(Idx<Inst>),
    IntBitcastUint(Idx<Inst>),

    // --- unary ---
    Neg(Idx<Inst>),
    Not(Idx<Inst>),
    BitNot(Idx<Inst>),
    UintBitNot(Idx<Inst>),

    // --- bitwise (LuaJIT `bit.*`) ---
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

    // --- calls ---
    // NOTE: args are a `Vec<Idx<Inst>>`, not the `IdxRange<Inst>` bone-mir-design
    // suggests for variadic operands. Post-lowering an arg's *value* is the last
    // inst of its (possibly multi-inst) computation, so arg values are NOT
    // contiguous in the arena and an `IdxRange` can't address them. (A finding
    // for the design: `IdxRange<Inst>` only holds for operands allocated together,
    // which lowered call args are not.)
    /// Direct call to a statically-resolved function.
    Call { func: FuncRef, args: Vec<Idx<Inst>> },
    /// Direct call whose result is a SCALARIZED value struct: the callee returns
    /// N leaf scalars (the scalar ABI), bound positionally into `dests` (`d0, d1,
    /// ... = callee(args)`). An effect (like [`Inst::MapNext`]) — the id is never
    /// referenced; the leaves are read back via [`Inst::Load`] of each dest cell.
    /// Modeled as a multi-assign so Lua doesn't truncate the multi-return.
    CallMulti {
        dests: Vec<Idx<Cell>>,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
    },
    /// Call a fn-typed value (a local cell read, global, or expression).
    CallValue {
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    },
    /// Indirect call whose result is a tuple returned via the multi-return ABI:
    /// the fn-value returns N values bound positionally into `dests`
    /// (`d0, d1, ... = callee(args)`). The `CallValue` analogue of [`Inst::CallMulti`]
    /// -- a multi-assign so Lua doesn't truncate the multi-return. Leaves are read
    /// back via [`Inst::Load`] of each dest cell.
    CallValueMulti {
        dests: Vec<Idx<Cell>>,
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    },
    /// PLACEHOLDER `next(x)` on an `Iterator`-bounded generic (`generic_idx`
    /// indexes the enclosing fn's generics). Has no callee: the concrete
    /// `next` is only known per instantiation -- the inliner substitutes it
    /// (via the site's `BoundTree`) into a [`Inst::CallMulti`] when this fn's
    /// body is spliced at a concrete root. Emitting a body that still
    /// contains one is a compile error (bounded fns exist only inlined).
    /// Bound conformance fixes the SLOTS at `(bool, E)`, but `E`'s shape is the
    /// enclosing body's specialization to answer, so `dests` is 2 only when `E`
    /// stays opaque; a value element is split into its leaves here
    /// ([`MirFunc::bound_next_split`]) rather than rebuilt at the splice.
    CallBoundMulti {
        dests: Vec<Idx<Cell>>,
        generic_idx: u32,
        args: Vec<Idx<Inst>>,
    },

    /// A closure value (`x => expr` / `(a, b) => { .. }`). Its body is a sub-CFG
    /// rooted at `entry`, allocated in the SAME blocks/insts/cells arenas as the
    /// enclosing function (so a captured outer variable is the same [`Cell`] and
    /// becomes a Lua upvalue for free — no `Upvalue` backing tag, bone-mir-design
    /// decision realized). `params` are the closure's parameter cells (its Lua
    /// `function(...)` arg list); only cells cross the boundary, so the closure's
    /// *values* are independent of the enclosing scope and are colored separately
    /// at emit. The sub-CFG is unreachable from the enclosing entry, so the
    /// enclosing emit never walks into it.
    Lambda {
        params: Vec<Idx<Cell>>,
        entry: Idx<Block>,
    },

    // --- aggregate constructors ---
    /// A tuple `{v0, v1, ...}` (1-indexed positional Lua table).
    MakeTuple(Vec<Idx<Inst>>),
    /// A `Vec`/`[T]` literal -> `{ data = {v0, ...}, len = N }` (the stdlib rep).
    MakeVec(Vec<Idx<Inst>>),
    /// A `[T]` literal in COLUMN storage — `{ <leaf0> = {..}, .., len = N }`.
    /// `columns` is one (leaf path, per-element values) pair per leaf, all of
    /// length `len`, in the element's declaration order.
    MakeVecColumns {
        elem: Ustr,
        representation: crate::lower::RepId,
        nobox: bool,
        columns: Vec<(Ustr, Vec<Idx<Inst>>)>,
        len: usize,
        capacity: Idx<Inst>,
    },
    /// A column Vec header rebuilt from an existing Vec. It carries the same
    /// planner fact as [`Inst::MakeVecColumns`] without changing the runtime
    /// header shape.
    MakeVecColumnHeader {
        elem: Ustr,
        representation: crate::lower::RepId,
        nobox: bool,
        capacity: Idx<Inst>,
    },
    MakeVecStrided {
        elem: Ustr,
        representation: crate::lower::RepId,
        nobox: bool,
        stride: usize,
        values: Vec<Idx<Inst>>,
        len: usize,
        capacity: Idx<Inst>,
    },
    MakeVecStridedHeader {
        elem: Ustr,
        representation: crate::lower::RepId,
        nobox: bool,
        stride: usize,
        capacity: Idx<Inst>,
    },
    /// Physical strided rows cannot contain nil without punching a Lua array
    /// hole. The selected element population has no active bool leaves, so
    /// false is an unambiguous inactive-lane sentinel.
    VecStrideNilToFalse(Idx<Inst>),
    VecStrideFalseToNil(Idx<Inst>),
    /// A struct value. `ty` is the nominal name for the `__ty` reflective field
    /// (`None` for an unknown/foreign nominal).
    MakeStruct {
        ty: Option<Ustr>,
        fields: Vec<(Ustr, Idx<Inst>)>,
    },
    /// A `value` struct -> a C-layout cdata via its `<name>_ct(...)` constructor
    /// (declared field order, positional). The cdef + ctor are emitted by the
    /// workspace scaffolding. cdata gives value (copy) semantics — required for
    /// `NativeBuffer<value>` byte-copy and field-wise `==`/`tostring` (metatype).
    /// The `value`-vs-table dispatch is decided HERE (D8), so codegen needs no
    /// type lookup.
    MakeValueStruct {
        name: Ustr,
        /// The build-owned representation that selected this constructor's
        /// physical form. Backends must realize this plan, never replan from
        /// `name` or the operand spelling.
        representation: crate::lower::RepId,
        form: ValueCtorForm,
        /// `(declared field name, value)` in declared field order (the `<name>_ct`
        /// ctor is positional, so order is load-bearing; the names let MIR passes
        /// forward a `Field` read to its operand -- SROA). Nested value-struct
        /// fields normally hold the nested `MakeValueStruct`, not its flattened
        /// leaves.
        ///
        /// ONE EXCEPTION, the LEAF FORM: names may instead be DOTTED paths
        /// naming every primitive leaf (`("pos.x", ..), ("pos.y", ..)`), which is
        /// what keeps a `nobox` field from materializing when its parent boxes
        /// (`flatten_nobox_nested_ctors`, `docs/nobox-nested-ctor-plan.md`). It
        /// exists only between the end of `cleanup` and emit, so no MIR pass sees
        /// it; luajit emits a generated `<name>_lct` and JS rebuilds the nested
        /// objects. Partial flattening is invalid: when any name is dotted the
        /// sequence must equal the owned representation plan's leaf paths exactly, which the
        /// Lua emitter asserts.
        fields: Vec<(Ustr, Idx<Inst>)>,
    },
    /// A first-class nullable value whose payload has already been built by
    /// MakeValueStruct. The explicit presence operand is required when a
    /// flattened Group crosses an opaque function-value parameter boundary.
    MakeNullableValue {
        present: Idx<Inst>,
        value: Idx<Inst>,
    },
    /// A value-struct box that is AUTHORIZED rather than implicit: requested by
    /// `materialize(value)`, or minted by the compiler at a `@gpu_only` call
    /// boundary (`authorize_gpu_boundary_boxes`), where the callee's boxed param
    /// is a Lua-side artifact of an arg that is a native `vec2`/`vec4` on the
    /// GPU. Emission is identical to `MakeValueStruct`; the difference is that
    /// the `nobox` verifier ignores this variant.
    MaterializeValueStruct {
        name: Ustr,
        /// The same physical plan as the implicit constructor, retained even
        /// though this node records an authorized materialization boundary.
        representation: crate::lower::RepId,
        form: ValueCtorForm,
        fields: Vec<(Ustr, Idx<Inst>)>,
    },
    /// An enum-variant value as a tagged table `{ __variant, __ty, <payload> }`.
    /// Tuple-payload variants fill `positional`; record-payload variants fill
    /// `named`; unit variants leave both empty.
    MakeEnum {
        tag: Ustr,
        ty: Ustr,
        positional: Vec<Idx<Inst>>,
        named: Vec<(Ustr, Idx<Inst>)>,
    },
    /// Re-materialize a boxed enum from a DYNAMIC tag + positional payload slots --
    /// the inverse of field flattening. Used at a
    /// whole-value-read boundary of a flattened enum field, where the variant is not
    /// statically known (it lives in a runtime `f__tag` slot). Emits
    /// `{__variant = <tag>, __ty = "<ty>", <positional...>, <named...>}`. Nil slots
    /// (a variant narrower than the union, or the inactive tuple/record half) are
    /// harmless: Lua omits them, so the box matches whichever variant is live.
    MakeEnumDyn {
        tag: Idx<Inst>,
        ty: Ustr,
        positional: Vec<Idx<Inst>>,
        named: Vec<(Ustr, Idx<Inst>)>,
    },

    /// A closure environment record (defunctionalized closure) or a box.
    /// Produced by `skull_transform::convert_closures` (the shared
    /// closure-conversion pass), NEVER by lowering. Two shapes:
    ///   - `fn_ref = Some(name)` is a callable RECORD -- a capturing lambda's
    ///     value: `{ __slot = <CodeRef>, c<A> = .. }`, invoked through the target's
    ///     `__call` convention (Lua `setmetatable({..}, __rock_fn_mt)`, JS the
    ///     `__rock_call` dispatch helper). A capture-FREE lambda is a bare
    ///     [`Inst::Global`] instead (no record, no alloc).
    ///   - `fn_ref = None` is a fn-less 1-field env used to BOX a mutable shared
    ///     capture (`{ v = .. }`): the box TABLE is the cell's value, the variable
    ///     is `.v` on it. An empty `fields` is the `{}` init of a fresh box.
    /// `fields` carry captured cell values by field name (`c<raw>`); env
    /// reads/writes reuse [`Inst::Field`]/[`Inst::StoreField`] (no dedicated
    /// EnvGet/EnvSet -- they express it cleanly).
    MakeEnv {
        fn_ref: Option<Ustr>,
        fields: Vec<(Ustr, Idx<Inst>)>,
    },

    /// `ffi.new("<ctype>[?]", count)` -- a contiguous flat ffi array of `count`
    /// elements of C type `ctype` (an FFI-clean value struct's cdef name, or
    /// `int`/`float`). Emitted only by the `native_buffer_alloc<T>` intrinsic,
    /// which resolves `ctype` from the concrete element type at the call site.
    FfiNewArray {
        ctype: Ustr,
        count: Idx<Inst>,
        /// Aggregate element allocations carry the plan used to validate
        /// their C layout. Primitive arrays have no nominal plan.
        representation: Option<crate::lower::RepId>,
    },

    /// `ffi.cast("<cty>*", <ptr>)` -- a typed LEAF VIEW over a NativeBuffer's
    /// storage, one per distinct primitive C type among the element's leaves.
    /// Made ONCE per buffer at construction and stored on the buffer table, not
    /// per access: the cast itself allocates a pointer cdata, so doing it per
    /// access would reintroduce exactly the allocation leaf addressing removes.
    ///
    /// `cty` is a primitive C type (`int` / `float` / `bool`). The view aliases
    /// the SAME bytes as `.ptr`, so leaf-addressed and slot-addressed accesses
    /// to one buffer stay coherent -- which is what lets the two coexist.
    FfiViewCast { cty: Ustr, ptr: Idx<Inst> },

    // --- projections ---
    /// `base.field` (struct field, or an enum record-payload field).
    Field { base: Idx<Inst>, field: Ustr },
    /// `base.N` on a tuple/newtype. 0-based (Rock); emit adds `+1` for Lua.
    TupleIndex { base: Idx<Inst>, index: usize },

    // --- container reads (0-based IR; emit adds `+1` on sequence access) ---
    /// `base[index]`, shape per `kind`: `Vec` -> `base.data[index+1]`,
    /// `NativeBuffer` -> `base.ptr[index]`, `Map` -> `base[index]`.
    Index {
        base: Idx<Inst>,
        index: Idx<Inst>,
        kind: ContainerKind,
    },
    /// `__vec_checked_index(vec, index)` — bounds-check `index` against
    /// `vec.len` and yield the 1-based backing index. Emitted only for
    /// [`ContainerKind::VecValueColumns`] access, where the K column reads that
    /// follow share ONE check; each then indexes raw via
    /// [`ContainerKind::VecColumn`].
    VecCheckedIndex { vec: Idx<Inst>, index: Idx<Inst> },
    /// Element count: `Vec` -> `base.len`, `NativeBuffer` -> `base.len`.
    Len {
        base: Idx<Inst>,
        kind: ContainerKind,
    },

    // --- effects (no value; the id is never referenced) ---
    /// Write a variable cell.
    Store { cell: Idx<Cell>, value: Idx<Inst> },
    /// `base.field = value`.
    StoreField {
        base: Idx<Inst>,
        field: Ustr,
        value: Idx<Inst>,
    },
    /// `base[index] = value` (shape per `kind`, see [`Inst::Index`]).
    StoreIndex {
        base: Idx<Inst>,
        index: Idx<Inst>,
        value: Idx<Inst>,
        kind: ContainerKind,
    },
    /// `NAME = value` — assign a module-level global (a Lua global).
    StoreGlobal { name: Ustr, value: Idx<Inst> },
    /// Append `value` to the array under construction `arr` (spread literals
    /// `[..a, b]`). `Vec`: `arr.len = arr.len + 1; arr.data[arr.len] = value`.
    /// An effect.
    ArrayPush {
        arr: Idx<Inst>,
        value: Idx<Inst>,
        kind: ContainerKind,
    },
    /// `await` bookkeeping: register/clear the inner coroutine being delegated
    /// to (so `kill()` on the outer chains into it). Appears ONLY in a `co fn`
    /// pre-transform; `skull-transform`'s coroutine pass rewrites it to
    /// `s.__inner = value`. A `nil` value clears it.
    CoroSetInner(Idx<Inst>),
    /// `on_killed`/`on_resume`/`on_hot_reload` lifecycle-hook registration
    /// inside a `co fn`. `lambda` is the hook body's [`Inst::Lambda`]. Appears
    /// ONLY pre-transform; the coroutine pass relocates it to the constructor
    /// (rendered as `s.__on_<kind> = function() .. end`) so a kill before the
    /// first resume still fires the hook (the body would otherwise register it
    /// only on first resume). An effect — the id is never referenced.
    CoroHook { kind: CoroHookKind, lambda: Idx<Inst> },
    /// `<key_cell>, <val_cell> = next(<table>, <key_cell>)` — one step of Lua
    /// stateless table iteration (for-over-`Map`). The key cell is both the
    /// previous key (cursor) and the new key; a nil key signals exhaustion.
    MapNext {
        key_cell: Idx<Cell>,
        val_cell: Idx<Cell>,
        table: Idx<Inst>,
    },
}

impl Inst {
    /// Call `push` for every variable cell this instruction reads or writes.
    pub fn cells_each(&self, push: &mut impl FnMut(Idx<Cell>)) {
        match self {
            Inst::Load(cell) | Inst::Store { cell, .. } => push(*cell),
            Inst::CallMulti { dests, .. }
            | Inst::CallValueMulti { dests, .. }
            | Inst::CallBoundMulti { dests, .. } => {
                dests.iter().for_each(|cell| push(*cell));
            }
            Inst::Lambda { params, .. } => params.iter().for_each(|cell| push(*cell)),
            Inst::MapNext { key_cell, val_cell, .. } => {
                push(*key_cell);
                push(*val_cell);
            }
            _ => {}
        }
    }

    /// Call `push` for every value operand this instruction reads.
    pub fn uses_each(&self, push: &mut impl FnMut(Idx<Inst>)) {
        match self {
            Inst::Const(_) | Inst::Global(_) | Inst::Load(_) => {}
            Inst::FfiViewCast { ptr, .. } => push(*ptr),
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
                push(*a);
                push(*b);
            }
            Inst::Format { value, .. } => push(*value),
            Inst::FormatDyn { value, fmt } => {
                push(*value);
                push(*fmt);
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
            | Inst::VecStrideFalseToNil(value) => push(*value),
            Inst::Call { args, .. }
            | Inst::CallMulti { args, .. }
            | Inst::CallBoundMulti { args, .. } => args.iter().for_each(|arg| push(*arg)),
            Inst::CallValue { callee, args } | Inst::CallValueMulti { callee, args, .. } => {
                push(*callee);
                args.iter().for_each(|arg| push(*arg));
            }
            Inst::Lambda { .. } => {}
            Inst::MakeVecColumns { columns, capacity, .. } => {
                push(*capacity);
                columns.iter().flat_map(|(_, values)| values).for_each(|&value| push(value));
            }
            Inst::MakeVecColumnHeader { capacity, .. } => push(*capacity),
            Inst::MakeVecStrided { values, capacity, .. } => {
                push(*capacity);
                values.iter().for_each(|&value| push(value));
            }
            Inst::MakeVecStridedHeader { capacity, .. } => push(*capacity),
            Inst::MakeTuple(values) | Inst::MakeVec(values) => {
                values.iter().for_each(|value| push(*value));
            }
            Inst::MakeStruct { fields, .. }
            | Inst::MakeValueStruct { fields, .. }
            | Inst::MaterializeValueStruct { fields, .. }
            | Inst::MakeEnv { fields, .. } => {
                fields.iter().for_each(|(_, value)| push(*value));
            }
            Inst::MakeNullableValue { present, value } => {
                push(*present);
                push(*value);
            }
            Inst::MakeEnum { positional, named, .. } => {
                positional.iter().for_each(|value| push(*value));
                named.iter().for_each(|(_, value)| push(*value));
            }
            Inst::MakeEnumDyn { tag, positional, named, .. } => {
                push(*tag);
                positional.iter().for_each(|value| push(*value));
                named.iter().for_each(|(_, value)| push(*value));
            }
            Inst::FfiNewArray { count, .. } => push(*count),
            Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => push(*base),
            Inst::Index { base, index, .. } | Inst::VecCheckedIndex { vec: base, index } => {
                push(*base);
                push(*index);
            }
            Inst::Len { base, .. } => push(*base),
            Inst::Store { value, .. } | Inst::StoreGlobal { value, .. } => push(*value),
            Inst::StoreField { base, value, .. } => {
                push(*base);
                push(*value);
            }
            Inst::StoreIndex { base, index, value, .. } => {
                push(*base);
                push(*index);
                push(*value);
            }
            Inst::ArrayPush { arr, value, .. } => {
                push(*arr);
                push(*value);
            }
            Inst::MapNext { table, .. } => push(*table),
            Inst::CoroSetInner(value) => push(*value),
            Inst::CoroHook { .. } => {}
        }
    }

    /// Whether this instruction binds a value (gets a `vN` in the dump and a
    /// target local). Effects like [`Inst::Store`] do not.
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

/// A variable slot — a Lua local. Read with [`Inst::Load`], written with
/// [`Inst::Store`]. Carries the source name for readable codegen. There is no
/// backing tag: an ordinary cell is a plain local (a closure-captured one is
/// shared for free by the target's closures), and a cell live across a coroutine
/// suspend becomes a frame field — but that is *computed by the coroutine
/// transform*, not stored here.
///
/// `loop_binding` is DESUGAR PROVENANCE set by the `for` lowerings (`lower_for_*`
/// counter cells + `MapNext` key/val dests): a direct capture of such a cell is
/// per-iteration BY VALUE, so the ownership analysis excludes it from boxing.
/// Living on the cell (not a `MirFunc` side-table) means every transform that
/// clones a cell carries the tag for free.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub name: Option<Ustr>,
    pub capture_identity: Option<crate::code_identity::CaptureIdentity>,
    pub loop_binding: bool,
    /// bone's type for this variable, when known at the binding site (params,
    /// `let`s). Non-Lua backends declare typed locals from it (the GLSL emitter
    /// reads it before falling back to first-store inference), fixing the
    /// `let y: float = 0` corner where the first store's rep is int. `None` for a
    /// compiler-minted temp (if-result, loop counter, call dest), whose type the
    /// backend infers from its first store. codegen-lua ignores it (untyped Lua).
    pub ty: Option<TyId>,
    /// Set ONLY at lowering, ONLY for a source binding (local / loop variable)
    /// whose bone type is a NON-nullable value struct. It licenses SROA to
    /// rematerialize an escaping whole-value load of this cell at the use site
    /// (immutable value + statically non-null == rebuild is observationally
    /// identical, the same guarantee that makes `v.field` legal). Any doubt about
    /// what a cell represents leaves it unset -- unset is always today's boxed
    /// emission; only a wrongly-set flag can miscompile, so all conservatism
    /// lands here (`docs/value-two-tier-plan.md` item 8).
    pub nonnull_value_struct: bool,
    /// A source parameter has a compiler-generated default-fill store. Closure
    /// ownership treats that store as initialization, not mutation.
    pub default_param: bool,
    /// Set ONLY at lowering, on the root cell of a place that a `ref` borrows.
    /// A write through that alias can reach a `value` object in this cell, and
    /// the transforms cannot see it, so the object must stay the one this cell
    /// holds: SROA does not split the cell (nor rematerialize it at a use), and
    /// copy-prop does not forward its loads (the `Field`-of-ctor fold assumes
    /// value immutability).
    pub borrowed: bool,
    /// Build-owned concrete type for a scalarized value leaf when its HIR
    /// `TyId` cannot name the physical leaf. Native backends consume this
    /// alongside the shared mono interner; source-level backends ignore it.
    pub native_ty: Option<crate::monoty::MonoTyId>,
    /// Compiler-owned flat-enum slot identity for a scalarized local. The
    /// physical slot may carry several logical members, so a single
    /// `MonoTyId` is not enough to describe its native representation.
    pub native_flat_slot: Option<NativeFlatSlot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeFlatSlot {
    pub owner: crate::monoty::MonoTyId,
    pub field: ustr::Ustr,
}

/// How a block ends. Control flow lives ONLY here.
#[derive(Clone, Debug, PartialEq)]
pub enum Term {
    /// `return` — zero values (bare `return`), one (`return v`), or N (a
    /// scalarized value struct's leaf fields: `return v0, v1, ...`).
    Return(Vec<Idx<Inst>>),
    /// Placeholder for a block still being built, or a provably-dead block
    /// (code after a `return`). Lowers to a bare `return`.
    Unreachable,
    /// Unconditional branch. (No args — merges go through cells, not block
    /// params.)
    Goto(Idx<Block>),
    /// Two-way branch on a bool value.
    Branch {
        cond: Idx<Inst>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
    },
    /// Multi-way branch (match). PROVISIONAL — the case-key representation is
    /// refined when match lowering lands (today: match a literal discriminant).
    Switch {
        discr: Idx<Inst>,
        cases: Vec<(Lit, Idx<Block>)>,
        default: Idx<Block>,
    },
    /// `yield <value>`, resuming at `resume`. `values` is the yield value's leaves
    /// (empty = bare yield, 1 = a scalar, N = a flat enum's tag+payload leaves via
    /// the frame-slot channel). **Pre-rewrite only** — the coroutine transform (in
    /// `skull-transform`) removes every `Yield`, turning the function into an
    /// ordinary state-machine CFG (writing the leaves to reused frame slots).
    Yield {
        values: Vec<Idx<Inst>>,
        resume: Idx<Block>,
    },
}

impl Term {
    /// Call `push` for every value operand this terminator reads.
    pub fn uses_each(&self, push: &mut impl FnMut(Idx<Inst>)) {
        match self {
            Term::Return(values) | Term::Yield { values, .. } => {
                values.iter().for_each(|value| push(*value));
            }
            Term::Branch { cond, .. } => push(*cond),
            Term::Switch { discr, .. } => push(*discr),
            Term::Unreachable | Term::Goto(_) => {}
        }
    }
}

/// A basic block: its instructions in order, then one terminator. **No params**
/// — the cell model routes every cross-block value through a cell, so the phi
/// mechanism is unused here (it would be introduced by a future `mem2reg`).
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

/// A `static` local's synthesized storage: a private global cell + a done-flag,
/// discovered during lowering and surfaced to `skull::mir_emit` for the
/// compiler-owned StateSchema and per-Session initializer reset. The
/// in-body first-reach guard is emitted inline (see `Builder::lower_static`).
#[derive(Clone, Debug, PartialEq)]
pub struct StaticCell {
    pub cell: Ustr,
    pub done_flag: Ustr,
    /// Declared/inferred source type, resolved through the owning concrete
    /// body's type table by typed native backends.
    pub ty: Option<bone_semantic::prelude::TyId>,
    /// Source binding name and declaration ordinal are the logical identity
    /// within the owning callable; synthesized cell names are realization only.
    pub logical_name: Ustr,
    pub ordinal: u32,
    /// Rendered type, included in the logical static layout fingerprint.
    pub type_str: String,
    /// Content hash of the initializer expression, so editing the initializer
    /// resets + re-inits the cell on the next reload.
    /// Stable across edits ELSEWHERE in the body (hashes names / literals, not
    /// arena indices).
    pub init_hash: u64,
    /// The compiler-owned callable identity that owns this static. This is
    /// metadata for StateSchema; the emitted cell name is only a realization.
    pub owner: Option<crate::code_identity::CodeIdentity>,
    /// Source site of the static declaration. It is stable under body edits and
    /// independent of lowering traversal order.
    pub site: Option<crate::code_identity::LambdaSiteId>,
}

/// Emission identities for a dense arena whose source had unreachable entries
/// removed. Physical indices remain dense for MIR and backend side tables; these
/// identities preserve the old arena order at the output boundary.
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

/// One comptime input a lowered body baked in (docs/comptime-plan.md tier 1):
/// which input, and the hash of the value that went into the emitted code.
/// bone hashes only source text, so incremental invalidation must compare
/// these against the CURRENT inputs itself -- a body with a stale read is a
/// body edit.
#[derive(Clone, Debug, PartialEq)]
pub struct ComptimeRead {
    pub input: ComptimeInputId,
    pub value_hash: u64,
    /// Source identity lets body witnesses distinguish the expanded literal
    /// from an ordinary string with the same bytes.
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ComptimeInputId {
    /// `build_var("NAME")` -- the declared var's name.
    Var(Ustr),
    /// `embed_string("path")` -- the path literal, project-root-relative.
    File(Ustr),
}

/// The one hash both the recorder and the staleness check use, so they cannot
/// drift (std `DefaultHasher`, the `StaticCell.init_hash` precedent).
pub fn comptime_value_hash(value: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}
/// A function — a CFG over cells and values. `params` are the entry block's
/// parameter *cells* in order (params are variables, hence cells); they
/// materialize as the Lua function parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct MirFunc {
    pub name: Ustr,
    /// Compiler-owned identity of the callable body, when this MIR is a
    /// concrete Rock function or a closure/nested-function sibling.
    pub code_identity: Option<crate::code_identity::CodeIdentity>,
    /// Canonical ABI witness used to build `code_compatibility` after closure
    /// conversion. It contains no backend symbol or build-local RepId.
    pub code_abi_fingerprint: Option<String>,
    /// Function-value ABI witnesses keyed by the pre-conversion lambda inst.
    pub lambda_abi_fingerprints:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, String>,
    pub lambda_abi_representations:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, crate::lower::FunctionRepPlan>,
    /// Concrete logical target for each named function-value mint.
    pub code_value_identities:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, crate::code_identity::CodeIdentity>,
    /// Identities of source-named nested functions keyed by their hoisted
    /// backend spelling. The spelling is only the lookup key during lowering;
    /// the stored value remains compiler-owned.
    pub nested_fn_identities:
        bone_semantic::prelude::FxHashMap<Ustr, crate::code_identity::CodeIdentity>,
    pub code_compatibility: Option<crate::code_identity::CodeCompatibility>,
    /// The build-owned representation catalog used by every plan-tagged value
    /// constructor in this function and its nested functions.
    pub rep_arena: Option<Arc<crate::lower::RepArena>>,
    pub params: Vec<Idx<Cell>>,
    /// Callee-entry comparisons that ask whether a declared parameter was
    /// omitted. Backends with typed parameters cannot use the Lua/JS nil
    /// sentinel and consume this provenance through their own call ABI.
    pub param_default_guards:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, usize>,
    /// Source parameter indices whose scalar literal defaults are supplied by
    /// direct calls instead of entering the default-fill branch.
    pub param_default_materialized: std::collections::HashSet<usize>,
    /// Default expression roots keyed by source parameter index. Placement
    /// callers need the callee-owned value when a wire slot is omitted.
    pub param_default_values:
        bone_semantic::prelude::FxHashMap<usize, Vec<Idx<Inst>>>,
    /// Opt-in timing for arena growth attribution.
    pub lower_timing_enabled: bool,
    pub lower_attribution: crate::lower::LowerAttribution,
    /// The concrete function representation consumed by entry binding and
    /// return lowering. Calls retain the same plan in `call_abi_plans`.
    pub abi_plan: Option<crate::lower::BackendFnAbiPlanHandle>,
    /// The owned function representation for bodies without an `Instance`,
    /// notably hoisted nested functions. Such a body still has a concrete ABI;
    /// it just has no graph-owned top-level instance to wrap in
    /// `BackendFnAbiPlan`.
    pub abi_representation: Option<crate::lower::FunctionRepPlan>,
    /// The logical function type paired with `abi_representation` for a
    /// hoisted nested function, which has no declaration `FuncRef` to recover
    /// it from later.
    pub abi_logical_type: Option<crate::monoty::MonoTy>,
    /// The owned result representation of an always-emitted global initializer.
    pub global_plan: Option<crate::lower::GlobalPlan>,
    /// `static` cells declared in this fn's body (in source order).
    pub statics: Vec<StaticCell>,
    /// Comptime inputs this body's emitted code baked in (build_var /
    /// embed_string expansions), for incremental staleness detection.
    pub comptime_reads: Vec<ComptimeRead>,
    /// Nominal structs whose declared logical shape is observed by runtime
    /// reflection even when no Rock constructor realizes the value.
    pub struct_registry_roots: std::collections::BTreeSet<Ustr>,
    pub insts: Arena<Inst>,
    pub cells: Arena<Cell>,
    pub blocks: Arena<Block>,
    pub emit_ids: Option<EmitIds>,
    /// Cell liveness observed by cleanup in source-only, unreachable blocks of
    /// an inline template. These roots replace copying those dead block loads.
    pub inline_cleanup_load_cells: Vec<Idx<Cell>>,
    pub entry: Idx<Block>,
    /// `Some` for a `co fn`: the cell `yield e` evaluates to (the next
    /// `resume(arg)` value). The coroutine transform makes it the `__step`'s
    /// second parameter. `None` for an ordinary function.
    pub is_coroutine: bool,
    pub resume_arg: Option<Idx<Cell>>,
    /// A co fn's terminal-cleanup defers, in registration-site order:
    /// (flag cell, detached guard-block entry). Each entry block is
    /// UNREACHABLE from `entry` (like closure bodies) and compiles to a
    /// standalone `<name>__defer<k>(s)` fn: `if flag { flag = false; <body> }`.
    /// The flag is set where the `defer` statement executes and cleared by the
    /// normal scope-exit splice, so terminal cleanup (kill/error) runs exactly
    /// the still-pending defers.
    pub coro_defers: Vec<(Idx<Cell>, Idx<Block>)>,
    /// Loop-head blocks emitted by `lower_for_user_iter` (the desugared
    /// `for x in <user iter>` back-edge/continue target). The FORL-recovery
    /// pass (`skull_transform::recover_counted_loops`) is gated on these tags:
    /// only a desugared iterator loop may have its bound snapshot-hoisted
    /// (iterator invalidation is recorded UB), never a user-written `while`.
    /// General splicing maps each cloned header through its block map; the
    /// caller's original call block keeps its own id.
    pub for_iter_headers: Vec<Idx<Block>>,
    /// Per call inst of an `Iterator`-bounded callee: the resolution tree that
    /// substitutes the placeholder [`Inst::CallBoundMulti`]s inside that callee
    /// when the inliner splices it here. Keys are remapped on splice; entries
    /// whose tree leaves are `CallerGeneric` stay placeholders until THIS fn is
    /// itself spliced at a concrete root.
    pub bound_trees:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, bone_semantic::prelude::BoundTree>,
    /// The same fact for a call a STANDALONE substitution produced: the `sub` of
    /// the resolved leaf that retargeted it, so the demand this body files for
    /// that callee carries the callee's own resolution.
    ///
    /// Separate from [`Self::bound_trees`] because it is already RESOLVED. A raw
    /// `BoundTree`'s `next_generics` are `TyId`s in the arena that recorded it,
    /// so a tree arriving from another instantiation cannot be stated as one --
    /// which is the whole reason a demand carries a `ResolvedBoundTree`. The
    /// splice path imports the resolved sub-tree beside the raw graft so a
    /// later standalone demand or nested splice does not re-derive another
    /// body's fact.
    pub resolved_bound_trees:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, crate::bound::ResolvedBoundTree>,
    /// Per call inst that the caller lowered with a scalarized delivery ABI:
    /// **the concrete callee body it targets**. The inliner reads it to splice
    /// the matching concrete snapshot; a surviving (un-spliced) entry is a
    /// hard site-aware rejection because an opaque delivery cannot receive its
    /// splatted args.
    ///
    /// An [`Instance`](crate::instance::Instance) rather than an instantiation
    /// KEY, so the symbol a call renderer spells and the symbol the definition
    /// was emitted under come from one object rather than from two derivations
    /// of one encoder.
    pub spec_bindings: bone_semantic::prelude::FxHashMap<Idx<Inst>, crate::instance::Instance>,
    /// Per call inst: the CONCRETE INSTANCE it targets.
    ///
    /// Separate from [`Self::spec_bindings`], and deliberately so: that map is
    /// the DELIVERY record (which calls the lowerer chose to serve with a
    /// specialized snapshot) and is therefore a policy artifact. This is
    /// IDENTITY, recorded for every complete instantiation regardless of what
    /// the policy decided, so a call that keeps the shared body still states
    /// which instance it means. Reading the delivery record as identity is what
    /// made a declined specialization indistinguishable from an unknown callee.
    pub callee_instances:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, crate::instance::Instance>,
    /// The ABI plan the call lowering used for each direct call. This is kept
    /// beside identity because reconstructing it from a later, possibly mutated
    /// owner would make an ABI corruption look self-consistent.
    pub call_abi_plans:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, crate::lower::BackendFnAbiPlanHandle>,
    /// The owned function-type representation used by an indirect call. A
    /// function value has no declaration `Instance`, so its physical signature
    /// is carried directly beside the `CallValue`/`CallValueMulti` instruction.
    pub call_rep_plans:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, crate::lower::FunctionRepPlan>,
    /// The owned capture transport for each closure creation instruction. The
    /// closure transform consumes this instead of re-deriving captures from the
    /// rewritten CFG.
    pub closure_env_plans:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, ClosureEnvPlan>,
    /// Placeholder [`Inst::CallBoundMulti`]s whose element the LOWERING already
    /// split into leaf cells: `dests` is sized to the real return arity and the
    /// loop pattern is bound to the leaves, so the splice must leave both alone.
    ///
    /// Membership is not derivable from `dests.len()`: a ONE-leaf element is two
    /// dests either way, which is exactly the shape a repack must still correct
    /// when lowering could NOT resolve it. So the fact is recorded rather than
    /// re-inferred.
    pub bound_next_split: bone_semantic::prelude::FxHashSet<Idx<Inst>>,
    /// Nested `fn`s declared in this fn's body, each lowered to its own MirFunc
    /// with a private `__local_*` name. Emitted as sibling top-level Lua fns by
    /// `mir_emit`; a call/value-ref resolves to the name (zero closure alloc).
    pub nested_fns: Vec<MirFunc>,
    /// Per-instruction source span: what an emit-time rejection points AT, rather
    /// than at the enclosing definition. Stamped for every instruction lowering
    /// emits, because a transform may REWRITE an instruction in place into the
    /// offending one (SROA turns an escaping `Load` into a `MakeValueStruct`) --
    /// stamping only the constructions lowering itself emits would miss exactly
    /// those. Keys are remapped on splice, like [`Self::spec_bindings`];
    /// transform-synthesized instructions simply have no entry.
    pub inst_spans: bone_semantic::prelude::ArenaMap<Idx<Inst>, bone_semantic::prelude::Span>,
    /// Concrete logical types for source-produced value constructors.
    /// Representation plans intentionally erase nominal generic arguments, so
    /// native backends consume this build-owned identity instead of guessing
    /// them from a constructor's field shape.
    pub inst_logical_types:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, crate::monoty::MonoTyId>,
    /// **Which callee obligation of the source body each call instruction is**
    /// (plan S 9, Stage 3). The join key between a lowered call and the callee
    /// `skull::concrete_body::ConcreteBody` already resolved for that site, so
    /// instance-driven emission can consume the graph's answer instead of
    /// resolving one again.
    ///
    /// A span cannot serve as this key and never could: a `for` head's `iter`
    /// and `next` live at one expression and share one span, so the OPERATION
    /// is part of the identity (`crate::site::SiteOp`).
    ///
    /// Stamped at the single choke point [`crate::lower::Builder::emit_cur`]
    /// for every call form, so a missing entry means the call was emitted
    /// outside any expression rather than that a recording site was forgotten.
    /// Keys are remapped on splice, like [`Self::spec_bindings`].
    pub call_sites: bone_semantic::prelude::FxHashMap<Idx<Inst>, crate::site::CallSite>,
    /// Instructions that MINT a workspace function's emitted SYMBOL as a value
    /// (`let f = adder`, an `adder` argument, a `func: adder` field), and which
    /// function each names.
    ///
    /// Recorded because an [`Inst::Global`] is not self-describing: the same
    /// form reads a module global, a `static` cell and a hoisted nested fn, and
    /// only this one carries a callee declaration. An indirect call through a
    /// value spells nothing, so the only way a wrong function can be NAMED is
    /// through one of these -- which makes it the population that has to join,
    /// rather than the `CallValue` that consumes it.
    pub fn_value_symbols: bone_semantic::prelude::FxHashMap<Idx<Inst>, FuncRef>,
    /// The emitted names of nested fns THIS body hoists to top level. Their
    /// symbols are the owning instance's own output rather than a callee
    /// declaration's, which is what separates them from
    /// [`Self::fn_value_symbols`].
    pub hoisted_fn_globals: Vec<Ustr>,
    /// Set on a hoisted flat-element `ref` variant: the declaration whose body
    /// it re-lowers with leaf-addressed `ref` parameters. The enclosing chunk's
    /// text then contains that body, so it is a dependency of the chunk.
    pub flat_variant_source: Option<FuncRef>,
    /// The owner instances [`Self::call_sites`] refers to, interned. Index 0 is
    /// this body's own owner when it has one; inlining appends the spliced
    /// callee's owner and remaps its entries, so a body holds one entry per
    /// distinct instance whose calls it now contains.
    pub site_owners: Vec<crate::instance::Instance>,
}

/// Lua runtime entry points of ordinary-fn `defer` unwinding (defined in the
/// program prelude, `skull::PRELUDE_TEMPLATE`). Registering a `defer` pushes
/// its detached cleanup closure; every scope-exit splice pops it. A body that
/// pushes is emitted with a prologue that re-enters it under `DEFER_CALL`,
/// which runs the still-registered cleanups when an error escapes and
/// re-raises the error.
pub const DEFER_PUSH: &str = "__rock_defer_push";
pub const DEFER_POP: &str = "__rock_defer_pop";
pub const DEFER_CALL: &str = "__rock_defer_call";

impl MirFunc {
    /// Whether any of `blocks` registers an ordinary-fn `defer` cleanup, i.e.
    /// whether this body must run under the unwind trampoline. Read off the
    /// final MIR, so an inlined callee's registrations protect its caller.
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
            code_identity: None,
            code_abi_fingerprint: None,
            lambda_abi_fingerprints: Default::default(),
            lambda_abi_representations: Default::default(),
            code_value_identities: Default::default(),
            nested_fn_identities: Default::default(),
            code_compatibility: None,
            rep_arena: None,
            params: Vec::new(),
            param_default_guards: Default::default(),
            param_default_materialized: Default::default(),
            param_default_values: Default::default(),
            lower_timing_enabled: crate::lower::lower_attribution_timing_enabled(),
            lower_attribution: Default::default(),
            abi_plan: None,
            abi_representation: None,
            abi_logical_type: None,
            global_plan: None,
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
            nested_fns: Vec::new(),
            inst_spans: Default::default(),
            inst_logical_types: Default::default(),
            call_sites: Default::default(),
            callee_instances: Default::default(),
            call_abi_plans: Default::default(),
            call_rep_plans: Default::default(),
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

    /// Allocate a fresh variable cell (untyped -- the backend infers from the
    /// first store). Use [`Self::new_cell_typed`] at a binding site with a
    /// declared/inferred type.
    pub fn new_cell(&mut self, name: Option<Ustr>) -> Idx<Cell> {
        let capacity = self.cells.capacity();
        let start = self.lower_timing_enabled.then(Instant::now);
        let id = self.cells.alloc(Cell {
            name,
            capture_identity: None,
            loop_binding: false,
            ty: None,
            nonnull_value_struct: false,
            default_param: false,
            borrowed: false,
            native_ty: None,
            native_flat_slot: None,
        });
        self.record_new_cell();
        if self.cells.capacity() != capacity {
            self.lower_attribution.mir_cell_reallocations += 1;
            self.lower_attribution.mir_cell_reallocation_ns +=
                start.map(|start| start.elapsed().as_nanos() as u64).unwrap_or_default();
        }
        id
    }

    /// Allocate a fresh variable cell carrying bone's type for the binding.
    pub fn new_cell_typed(&mut self, name: Option<Ustr>, ty: Option<TyId>) -> Idx<Cell> {
        let capacity = self.cells.capacity();
        let start = self.lower_timing_enabled.then(Instant::now);
        let id = self.cells.alloc(Cell {
            name,
            capture_identity: None,
            loop_binding: false,
            ty,
            nonnull_value_struct: false,
            default_param: false,
            borrowed: false,
            native_ty: None,
            native_flat_slot: None,
        });
        self.record_new_cell();
        if self.cells.capacity() != capacity {
            self.lower_attribution.mir_cell_reallocations += 1;
            self.lower_attribution.mir_cell_reallocation_ns +=
                start.map(|start| start.elapsed().as_nanos() as u64).unwrap_or_default();
        }
        id
    }

    /// Allocate a cell whose physical native type is known but whose source
    /// type arena has no concrete leaf node to reference.
    pub fn new_cell_native_typed(
        &mut self,
        name: Option<Ustr>,
        ty: Option<TyId>,
        native_ty: Option<crate::monoty::MonoTyId>,
    ) -> Idx<Cell> {
        let id = self.new_cell_typed(name, ty);
        self.cells[id].native_ty = native_ty;
        id
    }

    pub fn new_cell_native_flat_slot(
        &mut self,
        name: Option<Ustr>,
        owner: crate::monoty::MonoTyId,
        field: Ustr,
    ) -> Idx<Cell> {
        let id = self.new_cell(name);
        self.cells[id].native_flat_slot = Some(NativeFlatSlot { owner, field });
        id
    }

    /// Tag a cell as a `for`-desugaring loop binding (DESUGAR PROVENANCE; see
    /// [`Cell::loop_binding`]).
    pub fn mark_loop_binding(&mut self, cell: Idx<Cell>) {
        self.cells[cell].loop_binding = true;
    }

    /// Mark a parameter cell whose default expression is filled in the callee
    /// prologue, so closure capture does not mistake that initialization for a
    /// later mutation.
    pub fn mark_default_param(&mut self, cell: Idx<Cell>) {
        self.cells[cell].default_param = true;
    }

    /// Tag a source-binding cell as holding a non-nullable value struct (licenses
    /// SROA escape-rematerialization; see [`Cell::nonnull_value_struct`]).
    pub fn mark_nonnull_value_struct(&mut self, cell: Idx<Cell>) {
        self.cells[cell].nonnull_value_struct = true;
    }

    /// Tag the root cell of a `ref`-borrowed place (see [`Cell::borrowed`]).
    pub fn mark_borrowed(&mut self, cell: Idx<Cell>) {
        self.cells[cell].borrowed = true;
    }

    /// Allocate a fresh (empty, `Unreachable`-terminated) block.
    pub fn new_block(&mut self) -> Idx<Block> {
        let capacity = self.blocks.capacity();
        let start = self.lower_timing_enabled.then(Instant::now);
        let id = self.blocks.alloc(Block::new());
        self.record_new_block();
        if self.blocks.capacity() != capacity {
            self.lower_attribution.mir_block_reallocations += 1;
            self.lower_attribution.mir_block_reallocation_ns +=
                start.map(|start| start.elapsed().as_nanos() as u64).unwrap_or_default();
        }
        id
    }

    /// Allocate an instruction and append it to `block`; return its id (=value).
    pub fn emit(&mut self, block: Idx<Block>, inst: Inst) -> Idx<Inst> {
        let capacity = self.insts.capacity();
        let start = self.lower_timing_enabled.then(Instant::now);
        let id = self.insts.alloc(inst);
        self.record_new_inst();
        if self.insts.capacity() != capacity {
            self.lower_attribution.mir_instruction_reallocations += 1;
            self.lower_attribution.mir_instruction_reallocation_ns +=
                start.map(|start| start.elapsed().as_nanos() as u64).unwrap_or_default();
        }
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
            .map_or(u32::from(id.into_raw()), |ids| ids.insts[u32::from(id.into_raw()) as usize])
    }

    pub fn emit_cell_id(&self, id: Idx<Cell>) -> u32 {
        self.emit_ids
            .as_ref()
            .map_or(u32::from(id.into_raw()), |ids| ids.cells[u32::from(id.into_raw()) as usize])
    }

    pub fn emit_block_id(&self, id: Idx<Block>) -> u32 {
        self.emit_ids
            .as_ref()
            .map_or(u32::from(id.into_raw()), |ids| ids.blocks[u32::from(id.into_raw()) as usize])
    }

    pub fn emit_inst_span(&self) -> u32 {
        self.emit_ids.as_ref().map_or(self.insts.len() as u32, |ids| ids.next_insts)
    }

    pub fn emit_cell_span(&self) -> u32 {
        self.emit_ids.as_ref().map_or(self.cells.len() as u32, |ids| ids.next_cells)
    }

    pub fn emit_block_span(&self) -> u32 {
        self.emit_ids.as_ref().map_or(self.blocks.len() as u32, |ids| ids.next_blocks)
    }

    pub fn emit_nexts(&self) -> Option<(u32, u32, u32)> {
        self.emit_ids.as_ref().map(|ids| (ids.next_insts, ids.next_cells, ids.next_blocks))
    }

    pub fn set_emit_nexts(&mut self, nexts: (u32, u32, u32)) {
        let ids = self.emit_ids.as_mut().expect("emit identities must be enabled");
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
