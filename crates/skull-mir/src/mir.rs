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
use comfy_la_arena::{Arena, Idx};
use ustr::Ustr;

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
    /// `Map<K,V>` — keyed directly by K, no `+1`.
    Map,
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

    // --- unary ---
    Neg(Idx<Inst>),
    Not(Idx<Inst>),
    BitNot(Idx<Inst>),

    // --- bitwise (LuaJIT `bit.*`) ---
    BitAnd(Idx<Inst>, Idx<Inst>),
    BitOr(Idx<Inst>, Idx<Inst>),
    BitXor(Idx<Inst>, Idx<Inst>),
    Shl(Idx<Inst>, Idx<Inst>),
    Shr(Idx<Inst>, Idx<Inst>),

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
    /// `dests` is always 2 (`ok, elem` -- bound conformance requires exactly
    /// `(bool, E)`).
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
        /// `(declared field name, value)` in declared field order (the `<name>_ct`
        /// ctor is positional, so order is load-bearing; the names let MIR passes
        /// forward a `Field` read to its operand -- SROA). Nested value-struct
        /// fields hold the nested `MakeValueStruct`, not its flattened leaves.
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
    /// the inverse of field flattening (`docs/enum-allocation-report.md`). Used at a
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
    ///     value: `{ __fn = <name>, c<A> = .. }`, invoked through the target's
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
    FfiNewArray { ctype: Ustr, count: Idx<Inst> },

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
    /// `yield*` bookkeeping: register/clear the inner coroutine being delegated
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
/// hot-reload preservation preamble + `__rock_known_globals` registration. The
/// in-body first-reach guard is emitted inline (see `Builder::lower_static`).
#[derive(Clone, Debug, PartialEq)]
pub struct StaticCell {
    pub cell: Ustr,
    pub done_flag: Ustr,
    /// Rendered type -- half the `__rock_known_globals` key (reset on mismatch).
    pub type_str: String,
    /// Content hash of the initializer expression -- the other half of the key,
    /// so editing the initializer resets + re-inits the cell on the next reload.
    /// Stable across edits ELSEWHERE in the body (hashes names / literals, not
    /// arena indices).
    pub init_hash: u64,
}

/// A function — a CFG over cells and values. `params` are the entry block's
/// parameter *cells* in order (params are variables, hence cells); they
/// materialize as the Lua function parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct MirFunc {
    pub name: Ustr,
    pub params: Vec<Idx<Cell>>,
    /// `static` cells declared in this fn's body (in source order).
    pub statics: Vec<StaticCell>,
    pub insts: Arena<Inst>,
    pub cells: Arena<Cell>,
    pub blocks: Arena<Block>,
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
    /// The id survives inlining: `splice_general` keeps the call block's id.
    pub for_iter_headers: Vec<Idx<Block>>,
    /// Per call inst of an `Iterator`-bounded callee: the resolution tree that
    /// substitutes the placeholder [`Inst::CallBoundMulti`]s inside that callee
    /// when the inliner splices it here. Keys are remapped on splice; entries
    /// whose tree leaves are `CallerGeneric` stay placeholders until THIS fn is
    /// itself spliced at a concrete root.
    pub bound_trees:
        bone_semantic::prelude::FxHashMap<Idx<Inst>, bone_semantic::prelude::BoundTree>,
    /// Per call inst that the caller lowered with the SPECIALIZED scalar ABI: the
    /// callee's per-generic instantiation key (`docs/flatvec-plan.md` call-site
    /// specialization). The inliner reads it to splice the matching specialized
    /// snapshot; a surviving (un-spliced) entry is a hard site-aware rejection
    /// (its args are splatted, so the shared generic body cannot serve it).
    pub spec_bindings: bone_semantic::prelude::FxHashMap<Idx<Inst>, crate::lower::SpecKey>,
    /// Nested `fn`s declared in this fn's body, each lowered to its own MirFunc
    /// with a private `__local_*` name. Emitted as sibling top-level Lua fns by
    /// `mir_emit`; a call/value-ref resolves to the name (zero closure alloc).
    pub nested_fns: Vec<MirFunc>,
}

impl MirFunc {
    pub fn new(name: Ustr) -> Self {
        let mut blocks = Arena::default();
        let entry = blocks.alloc(Block::new());
        Self {
            name,
            params: Vec::new(),
            statics: Vec::new(),
            insts: Arena::default(),
            cells: Arena::default(),
            blocks,
            entry,
            is_coroutine: false,
            resume_arg: None,
            coro_defers: Vec::new(),
            for_iter_headers: Vec::new(),
            bound_trees: Default::default(),
            spec_bindings: Default::default(),
            nested_fns: Vec::new(),
        }
    }

    pub fn entry(&self) -> Idx<Block> {
        self.entry
    }

    /// Allocate a fresh variable cell (untyped -- the backend infers from the
    /// first store). Use [`Self::new_cell_typed`] at a binding site with a
    /// declared/inferred type.
    pub fn new_cell(&mut self, name: Option<Ustr>) -> Idx<Cell> {
        self.cells.alloc(Cell { name, loop_binding: false, ty: None, nonnull_value_struct: false })
    }

    /// Allocate a fresh variable cell carrying bone's type for the binding.
    pub fn new_cell_typed(&mut self, name: Option<Ustr>, ty: Option<TyId>) -> Idx<Cell> {
        self.cells.alloc(Cell { name, loop_binding: false, ty, nonnull_value_struct: false })
    }

    /// Tag a cell as a `for`-desugaring loop binding (DESUGAR PROVENANCE; see
    /// [`Cell::loop_binding`]).
    pub fn mark_loop_binding(&mut self, cell: Idx<Cell>) {
        self.cells[cell].loop_binding = true;
    }

    /// Tag a source-binding cell as holding a non-nullable value struct (licenses
    /// SROA escape-rematerialization; see [`Cell::nonnull_value_struct`]).
    pub fn mark_nonnull_value_struct(&mut self, cell: Idx<Cell>) {
        self.cells[cell].nonnull_value_struct = true;
    }

    /// Allocate a fresh (empty, `Unreachable`-terminated) block.
    pub fn new_block(&mut self) -> Idx<Block> {
        self.blocks.alloc(Block::new())
    }

    /// Allocate an instruction and append it to `block`; return its id (=value).
    pub fn emit(&mut self, block: Idx<Block>, inst: Inst) -> Idx<Inst> {
        let id = self.insts.alloc(inst);
        self.blocks[block].insts.push(id);
        id
    }

    pub fn set_term(&mut self, block: Idx<Block>, term: Term) {
        self.blocks[block].term = term;
    }
}
