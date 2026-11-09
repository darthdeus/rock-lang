//! skull-codegen-lua — the dumb Lua printer over skull-mir's typed cell-model CFG.
//!
//! luajit has real `goto`, so the CFG maps directly: each [`Block`] is a `::bbN::`
//! label, its instructions in order, then a terminator that `goto`s its
//! successors. Two luajit rules drive the shape:
//!   1. `goto` may not jump into the scope of a local — so EVERY value local and
//!      cell local is declared at the function top, before any label.
//!   2. `return` must be the last statement of its block — so every `return` is
//!      wrapped `do return v end`, which is legal as a final statement too.
//!
//! The printer NEVER dispatches on an operand's type: the IR is already typed
//! (`IntAdd` vs `FloatAdd` vs `StringConcat`), so `bone` is consulted ONLY to
//! resolve a `Call`'s [`FuncRef`] to its mangled name (D8/P3). A JS backend can't
//! use `goto`; it'll reloop the same MIR (Phase 8).
//!
//! Naming: a value-producing inst `Idx<Inst>` renders as `v<raw>`, a cell as
//! `c<raw>`, a block as `bb<raw>` — raw arena indices, so no renumbering pass and
//! references are a direct index format. (Regalloc to fit luajit's 200-local cap
//! is a later refinement.)

mod binding;
mod block;
mod counted_loops;
mod effect;
mod forward;
mod frame_check;
mod frame_limits;
mod function;
mod hoist;
mod inlinable;
mod inline_lua;
mod inst;
mod line_directives;
mod names;
mod preludes;
mod regalloc;
mod scc;
mod scope;
mod term;
mod trace_frames;
mod value;

pub use binding::*;
use counted_loops::*;
use forward::*;
use frame_check::*;
pub use function::*;
pub use hoist::*;
use inlinable::*;
use inline_lua::*;
pub use line_directives::*;
pub use names::*;
pub use preludes::*;
pub use scc::*;
use scope::*;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::Mutex;

use bone_core::hir::Function;
use bone_core::prelude::{CompilerError, Span};
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::cfg::{PredMap, predecessors, reachable_blocks_from, reverse_postorder, successors};
use skull_mir::mir::*;
use skull_mir::monoty::MonoInterner;

pub use regalloc::color_cells;
pub use skull_mir::mangle::mangle_fn;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EmittedAllocationKind {
    StringConcat,
    StringConversion,
    StringFormat,
    TupleConstruction,
    VecConstruction,
    VecColumnConstruction,
    VecColumnHeader,
    StructConstruction,
    ValueConstruction,
    ExplicitValueMaterialization,
    EnumConstruction,
    DynamicEnumConstruction,
    ClosureEnvironment,
    FfiArray,
    FfiView,
    ContainerGrowth,
}

impl EmittedAllocationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StringConcat => "string-concat",
            Self::StringConversion => "string-conversion",
            Self::StringFormat => "string-format",
            Self::TupleConstruction => "tuple-construction",
            Self::VecConstruction => "vec-construction",
            Self::VecColumnConstruction => "vec-column-construction",
            Self::VecColumnHeader => "vec-column-header",
            Self::StructConstruction => "struct-construction",
            Self::ValueConstruction => "value-construction",
            Self::ExplicitValueMaterialization => "explicit-value-materialization",
            Self::EnumConstruction => "enum-construction",
            Self::DynamicEnumConstruction => "dynamic-enum-construction",
            Self::ClosureEnvironment => "closure-environment",
            Self::FfiArray => "ffi-array",
            Self::FfiView => "ffi-view",
            Self::ContainerGrowth => "container-growth",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmittedAllocationSite {
    pub id: u64,
    pub kind: EmittedAllocationKind,
    pub span: Option<Span>,
    pub lua_body_line: u32,
    pub mir_inst: u32,
    lua_byte_offset: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LuaEmission {
    pub lua: String,
    pub allocation_sites: Vec<EmittedAllocationSite>,
}

fn checked_u32_constant(func: &MirFunc, x: Idx<Inst>) -> Option<f64> {
    let value = match func.insts[x] {
        Inst::Const(Lit::Int(value)) => value as f64,
        Inst::Const(Lit::Uint(value)) => value as f64,
        Inst::Const(Lit::Float(value)) => value,
        _ => return None,
    };
    (0.0..=4294967295.0).contains(&value).then(|| value.floor())
}

#[cfg(test)]
mod uint_constant_tests {
    use super::*;

    #[test]
    fn uint_constant_folding_keeps_invalid_values_checked() {
        let mut func = MirFunc::new(ustr::Ustr::from("checked"));
        for value in [
            Lit::Int(-1),
            Lit::Int(4294967296),
            Lit::Float(-0.1),
            Lit::Float(4294967295.5),
            Lit::Float(f64::NAN),
            Lit::Float(f64::INFINITY),
            Lit::Float(f64::NEG_INFINITY),
        ] {
            let x = func.alloc_inst(Inst::Const(value));
            assert_eq!(checked_u32_constant(&func, x), None);
        }
        let x = func.alloc_inst(Inst::Const(Lit::Float(-0.0)));
        assert_eq!(
            checked_u32_constant(&func, x).unwrap().to_bits(),
            (-0.0f64).to_bits()
        );
    }
}

/// The env-record parameter name of a hoisted capturing lambda body
/// (`function <name>__lambda<k>(<ENV_PARAM>, <params>)`). A reserved
/// double-underscore rock name so a user global/fn read inside the body
/// (`Inst::Global` emits the bare name) can never collide with it -- user
/// identifiers can't legally start with `__rock`. (The `e` local in
/// `FN_MT_PRELUDE`'s `__call` is a private Lua closure param, unaffected.)
const ENV_PARAM: &str = "__rock_env";

thread_local! {
    /// Cumulative time spent in register allocation (`regalloc::color_values`
    /// AND `regalloc::color_cells`) across every `render_scope`/`emit_*` call on
    /// this thread — including nested closures. Lets `emit_workspace_mir` split
    /// its "emit lua" stage into regalloc vs stringify for `backend-stats`. It's
    /// PER-THREAD: under parallel emit each unit resets it just before its own
    /// emit and reads it right after (a unit runs to completion on one rayon
    /// worker), so the per-unit value is exact and the summed total is correct
    /// regardless of feature.
    static REGALLOC_TIME: std::cell::Cell<std::time::Duration> =
        const { std::cell::Cell::new(std::time::Duration::ZERO) };
}

/// `regalloc::color_cells`, charged to the `REGALLOC_TIME` accumulator (so the
/// backend-stats regalloc/stringify split counts cell coloring as regalloc, not
/// stringify — it is the second half of register allocation).
fn color_cells_timed(
    analysis: &regalloc::CellAnalysis<'_>,
    inlinable: &[bool],
    extra_pinned: &[Idx<Cell>],
) -> Vec<u32> {
    let t = bone_core::prelude::Instant::now();
    let colors = regalloc::color_cells_with_analysis(analysis, Some(inlinable), extra_pinned);
    REGALLOC_TIME.with(|c| c.set(c.get() + t.elapsed()));
    colors
}

/// Zero the per-thread regalloc accumulator (call before an emit run).
pub fn regalloc_time_reset() {
    REGALLOC_TIME.with(|c| c.set(std::time::Duration::ZERO));
}

/// Read the per-thread regalloc accumulator (call after an emit run).
pub fn regalloc_time_elapsed() -> std::time::Duration {
    REGALLOC_TIME.with(|c| c.get())
}

fn raw<T>(idx: Idx<T>) -> u32 {
    idx.into_raw().into()
}

fn emitted_value_allocation_kind(inst: &Inst) -> Option<EmittedAllocationKind> {
    Some(match inst {
        Inst::StringConcat(..) => EmittedAllocationKind::StringConcat,
        Inst::ToString(..) => EmittedAllocationKind::StringConversion,
        Inst::Format { .. } | Inst::FormatDyn { .. } => EmittedAllocationKind::StringFormat,
        Inst::MakeTuple(..) => EmittedAllocationKind::TupleConstruction,
        Inst::MakeVec(..) => EmittedAllocationKind::VecConstruction,
        Inst::MakeVecColumns { .. } => EmittedAllocationKind::VecColumnConstruction,
        Inst::MakeVecColumnHeader { .. } => EmittedAllocationKind::VecColumnHeader,
        Inst::MakeVecStrided { .. } => EmittedAllocationKind::VecConstruction,
        Inst::MakeVecStridedHeader { .. } => EmittedAllocationKind::StructConstruction,
        Inst::MakeStruct { .. } => EmittedAllocationKind::StructConstruction,
        Inst::MakeValueStruct { .. } => EmittedAllocationKind::ValueConstruction,
        Inst::MaterializeValueStruct { .. } => EmittedAllocationKind::ExplicitValueMaterialization,
        Inst::MakeEnum { .. } => EmittedAllocationKind::EnumConstruction,
        Inst::MakeEnumDyn { .. } => EmittedAllocationKind::DynamicEnumConstruction,
        Inst::MakeEnv { .. } => EmittedAllocationKind::ClosureEnvironment,
        Inst::FfiNewArray { .. } => EmittedAllocationKind::FfiArray,
        Inst::FfiViewCast { .. } => EmittedAllocationKind::FfiView,
        _ => return None,
    })
}

fn emitted_effect_allocation_kind(inst: &Inst) -> Option<EmittedAllocationKind> {
    match inst {
        Inst::ArrayPush { .. }
        | Inst::StoreIndex {
            kind: ContainerKind::Map,
            ..
        } => Some(EmittedAllocationKind::ContainerGrowth),
        _ => None,
    }
}

fn allocation_site_id(
    body: &str,
    id: Idx<Inst>,
    kind: EmittedAllocationKind,
    span: Option<Span>,
) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    let mut write = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    write(body.as_bytes());
    write(kind.as_str().as_bytes());
    write(&raw(id).to_le_bytes());
    if let Some(span) = span {
        write(span.file.as_str().as_bytes());
        write(&span.offset_range.0.to_le_bytes());
        write(&span.offset_range.1.to_le_bytes());
    }
    hash
}

/// Append `n`'s base-10 digits to `out`. A small fast path that skips
/// `core::fmt`'s padding-aware `Display for u32` machinery — ~2x on the per-
/// operand `r<n>` / `c<n>` / `bb<n>` sites that dominate the printer.
fn push_uint(out: &mut String, mut n: u32) {
    if n == 0 {
        out.push('0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    // buf[i..] is guaranteed ASCII digits.
    out.push_str(std::str::from_utf8(&buf[i..]).unwrap());
}

/// Whole-workspace codegen input not derivable from a single [`MirFunc`]: the
/// map of every real top-level Lua name (function names + globals) to its source
/// span (guards every synthetic `<name>__lambda<k>` / `<co>__step` / `__on_*` /
/// ... name against clobber). Computed once per emit.
pub struct CodegenCtx {
    /// Every REAL top-level Lua name (a function's mangled name or a global's
    /// name) -> the span of its source definition. A synthetic name minted
    /// through [`mint_synthetic`] that lands here would clobber the user's def
    /// last-write-wins, so it is rejected (span points at the user's def).
    reserved: HashMap<String, Span>,
    /// Rejections recorded during (parallel) emit -- name collisions and broken
    /// codegen invariants alike; drained by [`take_rejection`] at each public
    /// emit boundary. `Mutex` because `&CodegenCtx` is shared across rayon
    /// workers; contention is nil (a few per fn).
    rejections: Mutex<Vec<(String, CompilerError)>>,
    /// Compiler-assigned storage slots for mutable globals.
    state_slots: HashMap<ustr::Ustr, u32>,
    scalar_constants: HashMap<ustr::Ustr, Lit>,
    /// Every function's strongly connected component number over the
    /// whole-program call graph, from [`call_sccs`]. Read ONLY by
    /// [`callee_binding_plan`]; empty on any path that never populated it,
    /// which binds nothing rather than binding unsoundly.
    call_sccs: HashMap<FuncRef, u32>,
    /// Whether a body over LuaJIT's parse-time frame limits is rejected
    /// ([`frame_limits`]). Always true in production; the boundary-agreement
    /// test turns it off so it can obtain the Lua whose `loadfile` verdict the
    /// check is compared against. There is deliberately NO environment switch:
    /// a program the check rejects cannot load, so shipping an escape hatch
    /// would only trade a Rock diagnostic for LuaJIT's.
    frame_limits: bool,
    /// Whether emitted functions carry LuaJIT `# <line> "<file>"` directives
    /// mapping their statements to Rock source ([`line_directives_mode`]).
    line_directives: bool,
    trace_frames: Option<Mutex<trace_frames::Contributors>>,
}

impl Clone for CodegenCtx {
    fn clone(&self) -> Self {
        Self {
            reserved: self.reserved.clone(),
            rejections: Mutex::new(self.rejections.lock().expect("rejection sink").clone()),
            state_slots: self.state_slots.clone(),
            scalar_constants: self.scalar_constants.clone(),
            call_sccs: self.call_sccs.clone(),
            frame_limits: self.frame_limits,
            line_directives: self.line_directives,
            trace_frames: self
                .trace_frames
                .as_ref()
                .map(|rows| Mutex::new(rows.lock().expect("trace frame sink").clone())),
        }
    }
}

/// True if `name` is an identifier skull emits as a bare LOCAL inside function
/// bodies -- a value temp (`r<N>`, or the uncolored `v<N>` fallback), a cell
/// local (`c<N>`), or the closure env param ([`ENV_PARAM`]). `Inst::Global`
/// reads/writes a user global by its bare name, so a top-level def with one of
/// these names is shadowed by the local inside any body that uses one (the
/// env-param-`e` miscompile `5c378c810`; `global r1` crashing at runtime with
/// "arithmetic on local 'r1' (a nil value)").
///
/// This is the reserved *local* namespace only. The `__rock_*` prefix at large
/// is a shared reserved namespace the stdlib and host runtimes legitimately
/// extend with top-level `@nomangle` fns (`__rock_create_struct`,
/// `__rock_install_serialize_value`, ...) -- those are top-level globals, never
/// body locals, so they don't collide and must not be rejected. Only the exact
/// emitted param name (`__rock_env`) is a body local.
fn is_reserved_lexical(name: &str) -> bool {
    if name == ENV_PARAM {
        return true;
    }
    let digits = match name.as_bytes().first() {
        Some(b'r' | b'c' | b'v') => &name[1..],
        _ => return false,
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Record a spanned rejection if `name` (a user def's emitted Lua name) lands in
/// the reserved lexical namespace. Same sink as [`CodegenCtx::mint_synthetic`],
/// drained at the same emit boundaries.
fn reserved_namespace_collision(name: &str, span: Span, sink: &mut Vec<(String, CompilerError)>) {
    if !is_reserved_lexical(name) {
        return;
    }
    let msg = format!(
        "`{name}` collides with a reserved codegen identifier: skull emits `r<N>`/`v<N>` \
         value temps, `c<N>` cell locals, and the `{ENV_PARAM}` closure-env param as bare \
         locals inside function bodies, which would shadow this top-level definition at \
         runtime -- rename it"
    );
    sink.push((name.to_string(), CompilerError::new_with_label(span, msg)));
}

fn scalar_constants(ws: &Workspace) -> HashMap<ustr::Ustr, Lit> {
    use bone_core::hir::{Body, BodyOwner, Callee, Expr};
    use bone_core::workspace::BodyInference;
    use skull_mir::monoty::{MonoSubst, MonoTy, convert};

    struct Folder<'a> {
        ws: &'a Workspace,
        body: &'a Body,
        inf: Option<&'a BodyInference>,
        known: &'a HashMap<ustr::Ustr, Lit>,
        subst: &'a MonoSubst,
    }
    impl Folder<'_> {
        fn numeric_type(&self, id: Idx<Expr>) -> Option<MonoTy> {
            let inf = self.inf?;
            let ty = convert(self.ws, inf, *inf.expr_types.get(id)?, self.subst).ok()?;
            matches!(ty, MonoTy::Int | MonoTy::Float).then_some(ty)
        }

        fn fold(&self, id: Idx<Expr>, depth: usize) -> Option<Lit> {
            // Initializers can be arbitrarily nested; leave large expressions at
            // runtime instead of growing the compiler stack without a bound.
            if depth >= 64 {
                return None;
            }
            match self.body.expr_arena[id] {
                Expr::Int(v) => Some(Lit::Int(v)),
                Expr::Float(v) => Some(Lit::Float(v.into_inner())),
                Expr::Bool(v) => Some(Lit::Bool(v)),
                Expr::String(v) => Some(Lit::Str(v)),
                Expr::Global(name) => self.known.get(&name).cloned(),
                Expr::UnaryMinus(inner, _) => match self.fold(inner, depth + 1)? {
                    Lit::Int(v) => v.checked_neg().map(Lit::Int),
                    Lit::Float(v) => Some(Lit::Float(-v)),
                    _ => None,
                },
                Expr::Call {
                    callee: Callee::Unresolved { name, .. },
                    args,
                    ..
                } if matches!(
                    name.as_str(),
                    "__op_add" | "__op_sub" | "__op_mul" | "__op_div"
                ) =>
                {
                    let mut args = args.into_iter();
                    let left = args.next()?;
                    let right = args.next()?;
                    if args.next().is_some() {
                        return None;
                    }
                    // Only the primitive numeric route is admitted. In particular,
                    // uint, newtype operators and string coercions keep their
                    // runtime checks and overload behavior.
                    self.numeric_type(left)?;
                    self.numeric_type(right)?;
                    let result_ty = self.numeric_type(id)?;
                    let number = |lit| match lit {
                        Lit::Int(v) if v.unsigned_abs() <= 9_007_199_254_740_991 => Some(v as f64),
                        Lit::Float(v) if v.is_finite() => Some(v),
                        _ => None,
                    };
                    let left = number(self.fold(left, depth + 1)?)?;
                    let right = number(self.fold(right, depth + 1)?)?;
                    let result = match name.as_str() {
                        "__op_add" => left + right,
                        "__op_sub" => left - right,
                        "__op_mul" => left * right,
                        "__op_div" if right != 0.0 => left / right,
                        _ => return None,
                    };
                    if !result.is_finite() {
                        return None;
                    }
                    match result_ty {
                        MonoTy::Float => Some(Lit::Float(result)),
                        MonoTy::Int
                            if result.abs() <= 9_007_199_254_740_991.0
                                && !(result == 0.0 && result.is_sign_negative()) =>
                        {
                            Some(Lit::Int(result as i64))
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        }
    }
    let mut pending: Vec<_> = ws
        .modules
        .iter()
        .flat_map(|module| {
            module.globals.iter().filter_map(|(id, global)| {
                if !global.is_const {
                    return None;
                }
                let owner = BodyOwner::Global(id);
                let inf = ws
                    .inference(module.id)
                    .and_then(|inf| inf.per_fn.get(&owner));
                module
                    .bodies
                    .get(&owner)
                    .map(|body| (global.name, body, inf))
            })
        })
        .collect();
    let mut known = HashMap::default();
    let subst = MonoSubst::default();
    loop {
        let before = pending.len();
        pending.retain(|(name, body, inf)| {
            let folder = Folder {
                ws,
                body,
                inf: *inf,
                known: &known,
                subst: &subst,
            };
            if let Some(lit) = body.root.and_then(|root| folder.fold(root, 0)) {
                known.insert(*name, lit);
                false
            } else {
                true
            }
        });
        if pending.len() == before {
            return known;
        }
    }
}

impl CodegenCtx {
    /// Refresh baked scalar values after a constant initializer edit. Readers of
    /// every returned name need new emitted text, including readers of aliases.
    pub fn refresh_scalar_constants(&mut self, ws: &Workspace) -> HashSet<ustr::Ustr> {
        let next = scalar_constants(ws);
        let changed = self
            .scalar_constants
            .keys()
            .chain(next.keys())
            .filter(
                |name| match (self.scalar_constants.get(name), next.get(name)) {
                    (Some(Lit::Float(x)), Some(Lit::Float(y))) => x.to_bits() != y.to_bits(),
                    (a, b) => a != b,
                },
            )
            .copied()
            .collect();
        self.scalar_constants = next;
        changed
    }

    pub fn new(ws: &Workspace) -> Self {
        Self::new_with_state(ws, HashMap::default())
    }

    pub fn new_with_state(ws: &Workspace, state_slots: HashMap<ustr::Ustr, u32>) -> Self {
        let mut reserved: HashMap<String, Span> = HashMap::default();
        let mut collisions: Vec<(String, CompilerError)> = Vec::new();
        for m in ws.modules.iter() {
            for (_, f) in m.functions.iter() {
                let name = mangle_fn(ws, f);
                reserved_namespace_collision(&name, f.name_span, &mut collisions);
                reserved.insert(name, f.name_span);
            }
            for (_, g) in m.globals.iter() {
                let symbol = skull_mir::mangle::symbol_of_key(g.name);
                reserved_namespace_collision(symbol.as_str(), g.name_span, &mut collisions);
                reserved.insert(symbol.as_str().to_string(), g.name_span);
            }
        }
        // Permanent mangle-collision check (reject, don't fall back): two distinct
        // signatures mangling alike, or a mangled name equal to an exempt bare
        // name. Drained through the same sink as the synthetic-name collisions.
        collisions.extend(skull_mir::mangle::collision_errors(ws));
        CodegenCtx {
            reserved,
            rejections: Mutex::new(collisions),
            state_slots,
            scalar_constants: scalar_constants(ws),
            call_sccs: HashMap::default(),
            frame_limits: true,
            line_directives: line_directives_mode(),
            trace_frames: trace_frames::enabled().then(|| Mutex::new(Default::default())),
        }
    }

    /// The table a mutable-global access indexes: the chunk-level storage
    /// cell every chunk (a full program or an overlay) declares.
    pub fn state_table(&self) -> &'static str {
        STATE_CELL
    }

    /// Turn the LuaJIT frame-limit rejection off for this context. ONLY the
    /// boundary-agreement test does this: it needs the emitted Lua of a body
    /// the check rejects so it can ask the bundled LuaJIT whether that Lua
    /// really fails to load. Production always leaves it on.
    pub fn set_frame_limits(&mut self, on: bool) {
        self.frame_limits = on;
    }

    pub fn frame_limits(&self) -> bool {
        self.frame_limits
    }

    /// Override the line-directive switch this context was built with. The
    /// production path takes it from `ROCK_SKULL_LINE_DIRECTIVES`.
    pub fn set_line_directives(&mut self, on: bool) {
        self.line_directives = on;
    }

    pub fn line_directives(&self) -> bool {
        self.line_directives
    }

    /// Opt-in trace-slot diagnostic over the final assembled Lua. The selected
    /// LuaJIT parses the chunk without running its initializers or user code.
    /// Diagnostic failures remain warnings and never change parse-limit checks.
    pub fn report_trace_frames(&self, lua: &str) {
        let Some(rows) = &self.trace_frames else {
            return;
        };
        let result = trace_frames::report(lua, &rows.lock().expect("trace frame sink"));
        match result {
            Ok(report) => {
                if let Some(path) = std::env::var_os("ROCK_LUA_TRACE_FRAME_REPORT") {
                    if let Err(error) = std::fs::write(path, report) {
                        eprintln!(
                            "warning: cannot write LuaJIT emitted-call frame report: {error}"
                        );
                    }
                } else {
                    eprint!("{report}");
                }
            }
            Err(error) => {
                eprintln!("warning: LuaJIT emitted-call frame report unavailable: {error}")
            }
        }
    }

    /// Record every function's call-graph strongly connected component. The
    /// caller owns the graph this is derived from (`skull::mir_emit` projects
    /// the whole-program instance graph onto declarations, which can only MERGE
    /// components and so only ever binds less); codegen only reads the answer.
    pub fn set_call_sccs(&mut self, sccs: HashMap<FuncRef, u32>) {
        self.call_sccs = sccs;
    }

    /// `func`'s strongly connected component number, or `None` when the graph
    /// this context was given does not mention it.
    pub fn scc_of(&self, func: FuncRef) -> Option<u32> {
        self.call_sccs.get(&func).copied()
    }

    /// Mint a compiler-reserved synthetic top-level Lua name: a hoisted lambda
    /// (`<fn>__lambda<k>`) or coroutine machinery (`<co>__step`/`__resume`/
    /// `__advance`/`__defer<k>`/`__on_killed`/`__on_resume`/`__on_hot_reload`).
    /// Returns `name` unchanged. If it collides with a REAL top-level name (a
    /// user fn's Lua name or a global), records a spanned rejection pointing at
    /// that user definition -- drained after emit by [`take_rejection`]. This is
    /// reject-not-panic: a collision is illegal user code, not a compiler bug, so
    /// it surfaces as a diagnostic and keeps interactive compiler hosts alive.
    /// THE single choke point for every synthetic top-level name across skull.
    pub fn mint_synthetic(&self, name: String) -> String {
        if let Some(&span) = self.reserved.get(&name) {
            let msg = format!(
                "`{name}` collides with a compiler-reserved synthetic name emitted by \
                 codegen; rename this definition (the `__lambda<k>`, `__step`, `__resume`, \
                 `__advance`, `__defer<k>`, and `__on_killed`/`__on_resume`/`__on_hot_reload` \
                 forms are reserved)"
            );
            self.reject(name.clone(), CompilerError::new_with_label(span, msg));
        }
        name
    }

    /// Record a rejection found mid-emit, keyed by `key` for deterministic
    /// ordering across rayon scheduling. THE sink for anything the render path
    /// cannot return a `Result` from.
    pub fn reject(&self, key: String, err: CompilerError) {
        self.rejections
            .lock()
            .expect("rejection sink")
            .push((key, err));
    }

    /// Drain the recorded rejections and return the first (by key, for
    /// determinism across rayon scheduling), or `None`. Called at each public
    /// emit boundary; draining clears the sink so a later emit on the same
    /// (cached) ctx starts fresh.
    pub fn take_rejection(&self) -> Option<CompilerError> {
        let mut sink = self.rejections.lock().expect("rejection sink");
        sink.sort_by(|a, b| a.0.cmp(&b.0));
        sink.drain(..).next().map(|(_, e)| e)
    }

    /// The storage slot of `name`. The map is keyed on the emitted SYMBOL, so a
    /// caller may hand over either spelling of a v1 global: its DefKey (what MIR
    /// carries) or the symbol (what the schema publishes).
    pub fn state_slot(&self, name: ustr::Ustr) -> Option<u32> {
        self.state_slots
            .get(&skull_mir::mangle::symbol_of_key(name))
            .copied()
    }

    /// The highest compiler-assigned storage slot, 0 when there is none. The
    /// emitter sizes the storage table's array part to this slot's rehash fixpoint.
    pub fn state_slot_max(&self) -> u32 {
        self.state_slots.values().copied().max().unwrap_or(0)
    }
}

/// `ROCK_SKULL_LINE_DIRECTIVES=1` writes LuaJIT line directives into every
/// emitted function (see [`write_line_directives`]). Off otherwise: a LuaJIT
/// without directive support rejects them as a syntax error.
pub fn line_directives_mode() -> bool {
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        matches!(
            std::env::var("ROCK_SKULL_LINE_DIRECTIVES").as_deref(),
            Ok("1")
        )
    })
}

/// The global holding the `Lit::Omitted` marker: a private table, so no Rock
/// value compares equal to it. The runtime prelude creates it once per Lua
/// state, so reloaded code compares against the same table.
pub const OMITTED_ARG: &str = "__rock_omitted";

/// Chunk-level local that every emitted function reads a mutable global
/// through: the process's one global storage table, `_G.__rock_globals`. Each
/// chunk (a full program or an overlay) declares its own and assigns the same
/// table, so no upvalue ever needs rejoining.
pub const STATE_CELL: &str = "__rock_state_cell";

/// The return statements of a one-expression wrapper -- an `extern(ffi)`
/// binding, a value-struct `_vt`/`_lct` ctor -- whose whole result is `expr`.
/// Two-space indented, no trailing newline.
///
/// `return lib.f(..)` would be a `CALLT` into C: a root trace started at the
/// wrapper's entry cannot leave through it (`rec_ret` needs a real `RET`) and
/// aborts `leaving loop in root trace` every time it is tried. Returning
/// through a local costs one bytecode and gives the trace a real `RET`. The
/// local is a reserved `__rock` name so it can never shadow a wrapper
/// parameter that `expr` reads.
pub fn wrapper_return(expr: &str) -> String {
    format!("  local __rock_ret = {expr}\n  return __rock_ret")
}

/// The shared per-type metatable global of a GC-tier `value` struct -- the table
/// every instance of `<name>` carries, so LuaJIT's `__eq` fires between any two
/// of them. ONE spelling, read by the definition emitter
/// (`skull::value_struct_gc_defs`).
pub fn value_struct_metatable_name(name: &str) -> String {
    format!("__rock_vt_{name}")
}

/// LuaJIT's hard limit on upvalues per function prototype (`LJ_MAX_UPVAL`).
/// A body with more bindable callees than this binds the first
/// `MAX_BOUND_CALLEES` in name order and leaves the rest global lookups.
pub const MAX_BOUND_CALLEES: usize = 60;

/// The factory holding the only copy of a bound function's body.
pub const CALLEE_FACTORY_PREFIX: &str = "__rock_mk_";

/// Recover the body symbol from a top-level emitted definition header.
/// Factories return the body on the same line, preserving body-local offsets.
pub fn emitted_body_name(line: &str) -> Option<&str> {
    let name = if let Some(rest) = line.strip_prefix("function ") {
        rest.split_once('(')?.0
    } else {
        let rest = line.strip_prefix(CALLEE_FACTORY_PREFIX)?;
        let (name, signature) = rest.split_once(" = function(")?;
        let (_, body) = signature.split_once(')')?;
        body.strip_prefix(" return function(")?;
        name
    };
    is_lua_ident(name).then_some(name)
}

#[cfg(test)]
mod emitted_body_header_tests {
    use super::emitted_body_name;

    #[test]
    fn recognizes_both_emitted_definition_shapes() {
        assert_eq!(emitted_body_name("function leaf_int(c0)"), Some("leaf_int"));
        assert_eq!(
            emitted_body_name("__rock_mk_leaf_int = function(callee) return function(c0)"),
            Some("leaf_int")
        );
    }

    #[test]
    fn rejects_non_headers_and_malformed_factory_shapes() {
        for line in [
            "leaf = __rock_mk_leaf(callee)",
            "__rock_mk_leaf = function(callee)",
            "__rock_mk_leaf = function(callee) return callee()",
            "__rock_mk_ = function() return function()",
            "__rock_mk_bad-name = function() return function()",
            "function ()",
            "function bad.name()",
            "function 1bad()",
            "  function nested()",
            "-- function commented()",
            "local function nested()",
        ] {
            assert_eq!(emitted_body_name(line), None, "{line}");
        }
    }
}

type EnumBoxGroups = Vec<(Vec<ustr::Ustr>, (usize, Vec<ustr::Ustr>))>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionTarget {
    Cell { cell: Idx<Cell>, join: Idx<Block> },
    Return,
}

#[derive(Clone, Copy)]
enum SelectionArm {
    Join {
        store: Option<(Idx<Cell>, Idx<Inst>)>,
        join: Idx<Block>,
    },
    Return(Idx<Inst>),
}

impl SelectionArm {
    fn target_value(self) -> Option<(SelectionTarget, Idx<Inst>)> {
        match self {
            Self::Join {
                store: Some((cell, value)),
                join,
            } => Some((SelectionTarget::Cell { cell, join }, value)),
            Self::Return(value) => Some((SelectionTarget::Return, value)),
            _ => None,
        }
    }
}

struct Emitter<'a> {
    enum_box_groups: RefCell<HashMap<ustr::Ustr, std::rc::Rc<EnumBoxGroups>>>,
    /// The mangled name of each declaration this body calls directly, derived
    /// once per body rather than at every call site.
    callee_symbols: RefCell<HashMap<FuncRef, std::rc::Rc<str>>>,
    selection_preds: std::cell::OnceCell<PredMap>,
    ws: &'a Workspace,
    ctx: &'a CodegenCtx,
    /// The build's one arena, so the symbol a call spells comes from the same
    /// [`Instance`] object the definition was emitted under rather than from a
    /// second derivation of the same encoder.
    ///
    /// [`Instance`]: skull_mir::instance::Instance
    interner: &'a MonoInterner,
    func: &'a MirFunc,
    /// Dense `raw(inst) -> color` (`u32::MAX` = uncolored); read per operand by
    /// [`write_vname`] as `colors[raw(id)]` instead of a hash probe.
    colors: Vec<u32>,
    /// `color -> owning loop header` (None = function top): loop-owned colors
    /// get their `local` line inside the `for` block ([`Self::expand_for`]).
    color_scope: Vec<Option<Idx<Block>>>,
    cell_colors: &'a [u32],
    frame: Option<&'a FrameCtx>,
    /// Recognized counted loops, keyed by header — a goto into an un-emitted
    /// header expands a structured `for`.
    loops: &'a HashMap<Idx<Block>, CountedLoop>,
    /// Reverse-postorder block layout (loop bodies emit their region in RPO).
    rpo: &'a [Idx<Block>],
    /// Values tree-folded into their single use site (no register); referenced
    /// through [`Self::opnd`], which renders their expression in place.
    /// Dense `raw(inst) -> bool`: values tree-folded into their single use site
    /// (no register), rendered in place by [`Self::write_opnd`].
    inlinable: Vec<bool>,
    /// Dense `raw(block) -> bool`: blocks already emitted (loop bodies self-skip).
    emitted: Vec<bool>,
    /// All-constant `MakeValueStruct` insts hoisted to a module-scope global:
    /// `inst -> group`. A reference emits `hoist_names[group]`; the statement is
    /// skipped (the def is emitted once, before the function).
    hoist: &'a HashMap<Idx<Inst>, u32>,
    /// `group -> global name` for [`Self::hoist`].
    hoist_names: &'a [String],
    state_slots: &'a HashMap<ustr::Ustr, u32>,
    static_slots: &'a HashMap<ustr::Ustr, String>,
    allocation: Option<AllocationEmission<'a>>,
    tail_forwards: &'a [Option<TailForward>],
    call_arg_forwards: &'a [Option<CallArgForward>],
    suppressed: &'a [bool],
    /// Death point of every declared local, written as `<local> = nil`.
    kills: regalloc::NilKills,
    /// Where each statement starts and which instruction it renders, when the
    /// definition is numbered with line directives.
    line_marks: Option<&'a RefCell<Vec<LineMark>>>,
}

fn write_lit(out: &mut String, l: &Lit) {
    match l {
        Lit::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Lit::Uint(i) => {
            let _ = write!(out, "{i}");
        }
        Lit::Float(x) => {
            let _ = write!(out, "{x:?}");
        }
        Lit::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        Lit::Str(s) => write_lua_string(out, s.as_str()),
        Lit::Nil => out.push_str("nil"),
        Lit::Omitted => out.push_str(OMITTED_ARG),
    }
}

/// Escape `s` as a double-quoted Lua string literal (also used by the workspace
/// emitter for inline(lua) default-value rendering).
pub fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_lua_string(&mut out, s);
    out
}

/// A Rock `[string]` value as a Lua literal.
///
/// The Vec representation is owned by `Inst::MakeVec` above; this is the same
/// shape, exposed so a compiler-emitted call site (the `rock_shader`
/// registrations in `skull::mir_emit`) cannot drift from it.
pub fn lua_string_vec(items: &[&str]) -> String {
    let mut out = String::from("{_data = {");
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_lua_string(&mut out, s);
    }
    let _ = write!(out, "}}, len = {}, __ty = \"Vec\"}}", items.len());
    out
}

/// The `[int]` sibling of [`lua_string_vec`], for driver text handing the engine
/// a list of integers (a uniform block's member offsets).
pub fn lua_int_vec(items: &[i64]) -> String {
    let mut out = String::from("{_data = {");
    for (i, n) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{n}");
    }
    let _ = write!(out, "}}, len = {}, __ty = \"Vec\"}}", items.len());
    out
}

/// A field key is a Lua identifier unless it is an `embed` leaf's physical key,
/// which is a dotted path (`"p.x"`). Lua keywords never reach here -- the
/// tokenizer already escapes them to `__rock_<kw>` -- so anything non-identifier
/// is a compiler-minted physical key. Constant-string bracket indexing compiles
/// to the same `TGETS` as dot sugar, so the bracket form costs nothing.
fn is_lua_ident(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Suffix position: `base.f` or `base["a.b"]`.
fn write_field_key(out: &mut String, field: &str) {
    if is_lua_ident(field) {
        let _ = write!(out, ".{field}");
    } else {
        out.push('[');
        write_lua_string(out, field);
        out.push(']');
    }
}

/// Table-constructor key position: `f = ` or `["a.b"] = `.
fn write_table_key(out: &mut String, field: &str) {
    if is_lua_ident(field) {
        let _ = write!(out, "{field} = ");
    } else {
        out.push('[');
        write_lua_string(out, field);
        out.push_str("] = ");
    }
}

/// Append the double-quoted, escaped Lua string literal for `s` into `out`.
fn write_lua_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
}
