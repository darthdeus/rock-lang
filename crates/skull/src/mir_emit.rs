//! New-path whole-workspace emit: the cell-model IR (`skull-mir`) lowered and
//! printed by `skull-codegen-lua`, assembled into a complete runnable Lua chunk.
//!
//! This is the end-to-end execution oracle for the bone-mir-faithful rebuild
//! the per-function lowering
//! coverage lens proves emit *succeeds*, but only running the assembled program
//! and diffing its output proves it is *correct*. It reuses the IR-AGNOSTIC
//! string helpers of the old backend (runtime prelude, value-struct cdefs, enum
//! singletons, type tables — all pure `Workspace -> String`) and swaps every
//! per-function body to the new path.
//!
//! LENIENT by design: a function the new path can't lower yet is skipped and
//! recorded, not fatal — so the oracle can run the assembled program and the
//! skipped list is the live parity gap. inline(lua) bodies emit verbatim,
//! externs are pre-existing Lua globals (no def), coroutines lower via the
//! `skull-transform` stackless pass, decorators via `lower_decorator`.
//!
//! Global inits are emitted in transitive-dependency order (`direct_refs` ->
//! `transitive_global_reads` -> `sort_global_init_lines`): a global read through
//! a call chain still inits first, mirroring rock's `global_dependencies`.

use bone_core::prelude::Instant;
use bone_semantic::hir::{BodyOwner, Expr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{Idx, ModuleId, is_stdlib_module_file};
use rustc_hash::{FxHashMap, FxHashSet};
use skull_codegen_lua::{CodegenCtx, emit_function, mangle_fn};
use skull_mir::instance::Instance;
use skull_mir::mir::{FuncRef, Inst, MirFunc, Term};
use skull_parser::CompilerError;
#[cfg(feature = "renderer-wgpu")]
use skull_parser::CompilerErrorKind;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::time::Duration;
use ustr::Ustr;

/// Map `f` over `items`, in PARALLEL by default (rayon work-stealing pool) or
/// SEQUENTIALLY under `--features sequential` (clean single-thread sampling
/// profiles — mirrors bone's `sequential` shim). Order-preserving in both modes,
/// so the assembled Lua chunk is byte-identical regardless of the feature. Each
/// `emit_*_unit` is independent (read-only `ws`/`scx`/`cg`, owns its own
/// `MirFunc`), so the lower+cleanup+emit work fans out cleanly per definition.
#[cfg(not(feature = "sequential"))]
struct WorkerMode {
    inline_off: bool,
    ablation: skull_mir::ablation::Context,
    allocation_report: bool,
    allocation_activation: bool,
    mem2reg_census: bool,
}

#[cfg(not(feature = "sequential"))]
impl WorkerMode {
    fn capture() -> Self {
        Self {
            inline_off: skull_transform::inline_off_active(),
            ablation: skull_mir::ablation::current_context(),
            allocation_report: crate::allocation_report::enabled(),
            allocation_activation: crate::allocation_report::activation_enabled(),
            mem2reg_census: crate::mem2reg_stats::enabled(),
        }
    }

    fn enter(&self) -> WorkerScope {
        WorkerScope {
            _inline: self.inline_off.then(skull_transform::inline_off_guard),
            _ablation: skull_mir::ablation::enter_context(self.ablation.clone()),
            _allocation_report: crate::allocation_report::scope(self.allocation_report),
            _allocation_activation: crate::allocation_report::activation_scope(
                self.allocation_activation,
            ),
            _mem2reg_census: crate::mem2reg_stats::scope(self.mem2reg_census),
        }
    }
}

#[cfg(not(feature = "sequential"))]
struct WorkerScope {
    _inline: Option<skull_transform::InlineOffGuard>,
    _ablation: skull_mir::ablation::ContextGuard,
    _allocation_report: crate::allocation_report::Scope,
    _allocation_activation: crate::allocation_report::ActivationScope,
    _mem2reg_census: crate::mem2reg_stats::Scope,
}

#[cfg(not(feature = "sequential"))]
fn par_map<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync + Send) -> Vec<R> {
    use rayon::prelude::*;
    let mode = WorkerMode::capture();
    items
        .into_par_iter()
        .map_init(move || mode.enter(), move |_, item| f(item))
        .collect()
}
#[cfg(feature = "sequential")]
fn par_map<T, R>(items: Vec<T>, f: impl Fn(T) -> R) -> Vec<R> {
    items.into_iter().map(f).collect()
}

/// Like [`par_map`], but threads a per-worker mutable state `S` (built by `init`)
/// into `f` — for the function fan-out, an [`InlineCache`](skull_transform::InlineCache)
/// reused across every fn a rayon worker handles, so each callee lowers + classifies
/// at most once per worker. PARALLEL by default (`map_init`, one `S` per worker
/// thread); SEQUENTIALLY under `--features sequential` ONE `S` is shared across all
/// items, exactly reproducing the original whole-workspace cache. Order-preserving
/// in both modes.
#[cfg(not(feature = "sequential"))]
fn par_map_with<T: Send, R: Send, S>(
    items: Vec<T>,
    init: impl Fn() -> S + Sync + Send,
    f: impl Fn(&mut S, T) -> R + Sync + Send,
) -> Vec<R> {
    use rayon::prelude::*;
    let mode = WorkerMode::capture();
    items
        .into_par_iter()
        .map_init(
            move || (init(), mode.enter()),
            move |state, item| f(&mut state.0, item),
        )
        .collect()
}
#[cfg(feature = "sequential")]
fn par_map_with<T, R, S>(
    items: Vec<T>,
    init: impl Fn() -> S,
    f: impl Fn(&mut S, T) -> R,
) -> Vec<R> {
    let mut state = init();
    items.into_iter().map(|t| f(&mut state, t)).collect()
}

#[cfg(not(feature = "sequential"))]
fn parallel_join<A: Send, B: Send>(
    left: impl FnOnce() -> A + Send,
    right: impl FnOnce() -> B + Send,
) -> (A, B) {
    rayon::join(left, right)
}

#[cfg(feature = "sequential")]
fn parallel_join<A, B>(left: impl FnOnce() -> A, right: impl FnOnce() -> B) -> (A, B) {
    (left(), right())
}

/// The globals a function reads DIRECTLY plus the (statically-resolved) functions
/// it calls, walking only blocks reachable from `mf.entry`. A lambda's body is a
/// sub-CFG unreachable from entry (its reads fire when the closure is later
/// *called*, not at this function's load time), so it is intentionally not
/// descended into — keeping init ordering from over-constraining on closures.
fn direct_refs(ws: &Workspace, mf: &MirFunc) -> (Vec<Ustr>, Vec<Ustr>) {
    let mut globals = Vec::new();
    let mut calls = Vec::new();
    let mut seen: FxHashSet<Idx<skull_mir::mir::Block>> = Default::default();
    let mut stack = vec![mf.entry];
    while let Some(b) = stack.pop() {
        if !seen.insert(b) {
            continue;
        }
        let block = &mf.blocks[b];
        for &iid in &block.insts {
            match &mf.insts[iid] {
                Inst::Global(n) => globals.push(*n),
                Inst::Call { func, .. } | Inst::CallMulti { func, .. } => {
                    let f = &ws.module(func.module).functions[func.func];
                    calls.push(Ustr::from(&mangle_fn(ws, f)));
                }
                _ => {}
            }
        }
        match &block.term {
            Term::Goto(t) => stack.push(*t),
            Term::Branch {
                then_bb, else_bb, ..
            } => {
                stack.push(*then_bb);
                stack.push(*else_bb);
            }
            Term::Switch { cases, default, .. } => {
                stack.extend(cases.iter().map(|(_, t)| *t));
                stack.push(*default);
            }
            Term::Yield { resume, .. } => stack.push(*resume),
            Term::Return(_) | Term::Unreachable => {}
        }
    }
    (globals, calls)
}

/// One pending global-init line plus the dependency facts the topo-sort needs.
struct GlobalInit {
    /// The global's name (the LHS of `<NAME> = __skull_ginit_<NAME>()`).
    name: Ustr,
    /// The globals this init transitively reads (through its call graph),
    /// restricted to globals that themselves have an init line.
    deps: Vec<Ustr>,
    line: String,
}

/// Round-based topo-sort: an init emits once every global it (transitively) reads
/// that ALSO has an init line is already emitted (self-refs ignored). Source order
/// is kept within a round. No progress => mutually-referential initializers —
/// rejected, not emitted to fail at runtime as a nil latch.
fn sort_global_init_lines(inits: Vec<GlobalInit>) -> Result<Vec<String>, CompilerError> {
    let known: FxHashSet<Ustr> = inits.iter().map(|g| g.name).collect();
    let mut done: FxHashSet<Ustr> = FxHashSet::default();
    let mut lines = Vec::with_capacity(inits.len());
    let mut remaining = inits;
    while !remaining.is_empty() {
        let mut next = Vec::new();
        let mut progress = false;
        for g in remaining {
            let ready = g
                .deps
                .iter()
                .all(|d| *d == g.name || !known.contains(d) || done.contains(d));
            if ready {
                done.insert(g.name);
                lines.push(g.line);
                progress = true;
            } else {
                next.push(g);
            }
        }
        if !progress {
            let names: Vec<&str> = next.iter().map(|g| g.name.as_str()).collect();
            return Err(CompilerError::new_no_location(format!(
                "skull: global initializers are mutually recursive: {}",
                names.join(", ")
            )));
        }
        remaining = next;
    }
    Ok(lines)
}

const LUA_INIT_PART_BYTES: usize = 32 * 1024;

fn wrap_large_lua_section(name: &str, body: &str) -> String {
    if body.len() <= LUA_INIT_PART_BYTES {
        return body.to_string();
    }
    format!("local function {name}()\n{body}end\n{name}()\n")
}

fn state_initializer_lua(init_lines: &[String]) -> String {
    let mut parts: Vec<Vec<&str>> = vec![Vec::new()];
    let mut part_bytes = 0;
    for line in init_lines {
        if part_bytes > 0 && part_bytes + line.len() > LUA_INIT_PART_BYTES {
            parts.push(Vec::new());
            part_bytes = 0;
        }
        parts.last_mut().unwrap().push(line.as_str());
        part_bytes += line.len();
    }

    let mut out = String::new();
    if parts.len() == 1 {
        out.push_str("function __rock_state_initialize()\n");
        for line in &parts[0] {
            out.push_str(line);
        }
        out.push_str("end\n__rock_state_initialize()\n");
        return out;
    }

    for (index, part) in parts.iter().enumerate() {
        writeln!(out, "local function __rock_state_initialize_part_{index}()").unwrap();
        for line in part {
            out.push_str(line);
        }
        out.push_str("end\n");
    }
    out.push_str("function __rock_state_initialize()\n");
    for index in 0..parts.len() {
        writeln!(out, "__rock_state_initialize_part_{index}()").unwrap();
    }
    out.push_str("end\n__rock_state_initialize()\n");
    out
}

#[cfg(test)]
mod state_initializer_tests {
    use super::{state_initializer_lua, wrap_large_lua_section};

    #[test]
    fn large_global_initializer_is_split_without_reordering() {
        let lines = vec![
            format!("A = \"{}\"\n", "a".repeat(20_000)),
            format!("B = \"{}\"\n", "b".repeat(20_000)),
            format!("C = \"{}\"\n", "c".repeat(20_000)),
        ];
        let lua = state_initializer_lua(&lines);

        assert!(lua.contains("local function __rock_state_initialize_part_0()"));
        assert!(lua.contains("local function __rock_state_initialize_part_1()"));
        assert!(lua.contains("__rock_state_initialize_part_0()"));
        assert!(lua.contains("__rock_state_initialize_part_1()"));
        assert!(lua.find("A =").unwrap() < lua.find("B =").unwrap());
        assert!(lua.find("B =").unwrap() < lua.find("C =").unwrap());
        assert_eq!(lua.matches("A =").count(), 1);
        assert_eq!(lua.matches("B =").count(), 1);
        assert_eq!(lua.matches("C =").count(), 1);
    }

    #[test]
    fn large_publication_section_is_nested_without_reordering() {
        let body = "A = 1\nB = 2\n".repeat(20_000);
        let lua = wrap_large_lua_section("__rock_publication_part", &body);

        assert!(lua.starts_with("local function __rock_publication_part()\n"));
        assert!(lua.ends_with("end\n__rock_publication_part()\n"));
        assert_eq!(lua.matches("A = 1").count(), 20_000);
        assert_eq!(lua.matches("B = 2").count(), 20_000);
    }
}

/// Forward transitive closure over a symbol-reference graph (`name -> names it
/// references`): every name reachable FROM `start` by following `deps` edges
/// (NOT including `start` itself unless a cycle leads back). Used by the const
/// overlay to find which consts an edited const affects -- the same
/// `BodyInference.deps` symbol-reference info goto-def / references use.
fn forward_reach(deps: &FxHashMap<Ustr, FxHashSet<Ustr>>, start: Ustr) -> FxHashSet<Ustr> {
    let mut seen: FxHashSet<Ustr> = FxHashSet::default();
    let mut stack = vec![start];
    while let Some(n) = stack.pop() {
        if let Some(ds) = deps.get(&n) {
            for &d in ds {
                if seen.insert(d) {
                    stack.push(d);
                }
            }
        }
    }
    seen
}

/// Render a literal default-value expression as Lua (for inline(lua) and
/// `extern(ffi)` default fills). `None` for a non-literal default.
fn render_default_lit(body: &bone_semantic::hir::Body, e: Idx<Expr>) -> Option<String> {
    match body.expr_arena[e] {
        Expr::Null | Expr::Unit => Some("nil".to_string()),
        Expr::Int(n) => Some(n.to_string()),
        Expr::Float(f) => Some(format!("{:?}", f.0)),
        Expr::Bool(b) => Some(b.to_string()),
        Expr::String(s) => Some(skull_codegen_lua::lua_string(s.as_str())),
        Expr::UnaryMinus(inner, _) => render_default_lit(body, inner).map(|s| format!("-{}", s)),
        _ => None,
    }
}

/// Callee-side `if p == nil then p = <lit> end` fills for a generated Lua
/// wrapper. A Rock direct call already passes a literal default at the site;
/// this covers an indirect or Lua call that omitted it. `inline(lua)` and
/// `extern(ffi)` share the same fill because both wrappers are the body the
/// omitted argument actually arrives at. A nullable parameter with a non-null
/// default gets no fill: its nil is an explicit `null` (EXPLICIT_NULL_DEFAULT).
fn default_fill_prologue(
    ws: &Workspace,
    mid: ModuleId,
    fid: Idx<bone_semantic::hir::Function>,
    f: &bone_semantic::hir::Function,
) -> String {
    let mut prologue = String::new();
    let Some(fbody) = ws.module(mid).bodies.get(&BodyOwner::Function(fid)) else {
        return prologue;
    };
    for (i, def) in fbody.param_defaults.iter().enumerate() {
        if let (Some(d), Some(pd)) = (def, f.param_decls.get(i)) {
            if skull_mir::lower::param_null_overrides_default(ws, fbody, i, pd.ty) {
                continue;
            }
            if let Some(lit) = render_default_lit(fbody, *d) {
                let p = pd.name.as_str();
                let _ = writeln!(prologue, "if {p} == nil then {p} = {lit} end");
            }
        }
    }
    prologue
}

/// What an `extern(ffi)` wrapper does with the callee's return, decided ONCE
/// from the declared Rock type.
///
/// The `ffi.cdef` lives inside an opaque `inline(lua)` body, so the declaration
/// is the only contract the compiler has -- which is exactly why an unresolvable
/// declared type is a hard error here and not a default. Falling back to
/// "return it as-is" would silently reproduce the boxed-handle bug this
/// classification exists to prevent, and would do it precisely where the
/// compiler has admitted it does not know what the type is.
#[derive(Debug)]
enum ExternRet {
    /// No return value.
    None,
    /// Rock `int`. LuaJIT hands back a BOXED cdata for a 64-bit C integer
    /// (`uint64_t`, `int64_t`), so the wrapper coerces to a number. A cdata
    /// leaking through is not cosmetic: LuaJIT keys a table by cdata IDENTITY,
    /// so a handle used as a `Map<int, V>` key only ever hits on the box that
    /// was inserted (`b1_ffi_handle.rock`).
    Int,
    /// Rock `uint` uses the same numeric bridge, but remains a distinct static
    /// type so its caller-side normalization and ABI identity are preserved.
    Uint,
    /// A `value` struct returned BY VALUE; the C ABI is then
    /// `void f(T* out, ..)`, so the wrapper bridges an out-param.
    ValueStruct(String),
    /// Rock `bool`. The C side declares the return `uint8_t`, not `bool`, so
    /// the wrapper turns the byte into a Lua boolean -- the same coercion
    /// principle as `Int`, applied to the other representation the declaration
    /// promises but the C ABI does not hand back directly.
    ///
    /// The reason the `cdef` says `uint8_t` is LuaJIT, not the ABI: recording a
    /// `bool`-returning C call needs a Lua frame INSIDE the trace above the call
    /// (`crec_call` -> `crec_snap_caller`, `lj_crecord.c`), and a trace rooted
    /// at this one-line wrapper has none, so it aborts `NYI: unsupported C
    /// function type` every time the wrapper itself becomes a trace root. A
    /// `uint8_t` return records unconditionally. Rust `extern "C" fn -> bool`
    /// already returns one byte 0/1 in that register, so only the `cdef` and
    /// this arm move -- and they MUST move together: `~= 0` over a `bool` cdef
    /// compares a Lua boolean to 0 and is always true.
    Bool,
    /// Everything else crosses as the callee produced it: LuaJIT already hands
    /// back a Lua value for `float`/`double` and a string, and a pointer return
    /// is declared `Ptr<T>` and stays a cdata by design.
    AsIs,
}

fn extern_ret(
    ws: &Workspace,
    f: &bone_semantic::hir::Function,
) -> Result<ExternRet, CompilerError> {
    use skull_mir::monoty::{MonoCtor, MonoSubst, MonoTy, MonoTyReason};

    let Some(ret) = f.return_ty else {
        return Ok(ExternRet::None);
    };
    let unrealizable = |what: &str| {
        CompilerError::new_with_label(
            f.name_span,
            format!(
                "extern `{}` returns {what}, which has no C representation: the wrapper \
                 cannot know what the callee hands back or how to give it the declared \
                 Rock type. Declare a concrete return type.",
                f.name
            ),
        )
    };
    // Every non-conversion is a REASON, so none of them can read as "some other
    // opaque type" the way a partial normalizer's fallback node would -- which
    // matters most for the cycle check below, where a summarized instantiation
    // could manufacture a false match.
    let describe = |r: MonoTyReason| -> CompilerError {
        match r {
            MonoTyReason::UnboundGeneric(n) => {
                unrealizable(&format!("the unbound generic parameter `{n}`"))
            }
            MonoTyReason::DepthLimit => unrealizable("a type that does not bottom out"),
            MonoTyReason::RecoveryError => unrealizable("a type inference could not recover"),
            other => unrealizable(&format!(
                "a type the compiler cannot normalize ({})",
                other.bucket()
            )),
        }
    };

    let ty = skull_mir::monoty::convert_decl(ws, ret, &MonoSubst::default()).map_err(describe)?;
    let ty = skull_mir::monoty::resolve_newtype_chain(ws, ty).map_err(|reason| {
        let detail = match reason {
            skull_mir::monoty::NewtypeResolutionError::Conversion(reason) => {
                return describe(reason);
            }
            other => other.to_string(),
        };
        unrealizable(&format!("a newtype representation ({detail})"))
    })?;

    Ok(match ty {
        MonoTy::Int => ExternRet::Int,
        MonoTy::Uint => ExternRet::Uint,
        MonoTy::Bool => ExternRet::Bool,
        MonoTy::App {
            ctor: MonoCtor::Nominal(name),
            ..
        } if crate::is_value_struct(ws, name) => ExternRet::ValueStruct(name.as_str().to_string()),
        _ => ExternRet::AsIs,
    })
}

/// O(1) container lengths and a dependency total folded by the existing
/// production dependency-index loop. No diagnostic graph walk is needed.
#[derive(Clone, Copy, Debug, Default)]
pub struct CompileCounts {
    pub concrete_plans: usize,
    pub chunks: usize,
    pub dependency_edges: Option<usize>,
}

/// Containers released before one-shot assembly finishes. Live builds retain them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompileReleaseStats {
    pub graph_instances: usize,
    pub chunks: usize,
    pub concrete_plans: usize,
    pub retained_dependency_edges: usize,
    pub retained_static_keys: usize,
}

/// The assembled new-path program plus the parity gap.
pub struct MirEmit {
    pub release_stats: CompileReleaseStats,
    pub compile_counts: CompileCounts,
    pub lua: String,
    /// Allocation-capable operations registered by final Lua emission, with
    /// residual call edges and compiler-owned per-frame lifecycle routes.
    pub allocation_report: crate::allocation_report::AllocationReport,
    /// Artifact-owned callable identities and their self-validating census.
    pub code_artifact: std::sync::Arc<skull_mir::code_identity::CodeArtifact>,
    /// The compiler-owned lifecycle inventory staged by this artifact.
    pub lifecycle_plan: skull_mir::lifecycle::LifecyclePlan,
    /// The mutable-global layout fingerprint the program opens its storage with.
    pub global_layout: String,
    /// The compiler-owned definition inventory included in the exact Artifact
    /// identity and published to the runtime.
    pub definition_plan: skull_mir::definition::DefinitionPlan,
    /// The semantic identity of this exact emitted Artifact. It is computed
    /// before the backend payload leaves the compiler.
    pub artifact_fingerprint: String,
    /// `(lua_name, reason)` for every definition the new path skipped.
    pub skipped: Vec<(String, String)>,
    /// Coarse per-stage time for the emit, in run order. `setup:` entries are
    /// direct wall-clock measurements; the remaining entries cover HIR->MIR
    /// lowering, MIR cleanup, Lua emission, and sequential assembly. The latter
    /// are per-unit elapsed timers that overlap under parallel emit and become
    /// wall-clock measurements under `sequential`.
    pub phases: Vec<(&'static str, std::time::Duration)>,
    /// ABI-plan memo counters over the emitting context's lifetime (== this
    /// emission for a full build, cumulative across edits on the incremental
    /// paths). Counts, so load-immune: `hits() > 0` on any real corpus is the
    /// memo's non-vacuity witness. Surfaced by `backend-stats`.
    pub abi_plan_stats: skull_mir::lower::AbiPlanMemoStats,
    /// Body-oriented attribution for the current emission. Counters are reduced
    /// from per-body results after the parallel fanout, so the hot passes do not
    /// use shared atomics.
    pub attribution: EmitAttribution,
    /// Shared-arena traffic split at the setup/fanout boundary (full builds;
    /// zeroed on paths without a fresh setup phase). The `*_final - *_setup`
    /// deltas are the post-setup traffic a frozen read view would serve.
    pub arena_traffic: ArenaTraffic,
    /// The three independent setup lanes and their production populations.
    /// The overlap bits come from the lanes' actual execution intervals, not
    /// from a wall-clock estimate.
    pub setup_lanes: SetupLaneAttribution,
    /// Operational graph work and repeated source-fact populations from the
    /// graph used by this emission.
    pub graph_census: crate::instance_graph::RunStats,
    /// Concrete-plan ownership and assembly populations from this emission.
    pub concrete_stats: crate::concrete_body::ConcreteBuildStats,
    /// Whether this emission defines the game entry point `main`, so the caller
    /// can append `__rock_main_wrapper(main)`.
    ///
    /// Reported by the emitter rather than recovered from the text: a caller
    /// that searched `lua` for `function main(` silently stopped finding it the
    /// moment `ROCK_SKULL_CALLEE_BINDING=upvalue` moved the body inside a
    /// factory, and the game booted to a chunk that defined everything and ran
    /// nothing.
    pub emits_main: bool,
    /// Every shader this artifact registers, as the source the registration
    /// carries, in registration order. Shared with the emit cache, which
    /// keeps them across incremental builds.
    pub shaders: std::sync::Arc<[ShaderSource]>,
}

/// One registered shader's emitted source.
#[derive(Clone, Debug)]
pub struct ShaderSource {
    pub key: String,
    pub language: ShaderLanguage,
    pub source: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderLanguage {
    Glsl,
    Wgsl,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetupLaneAttribution {
    pub cpu_population: usize,
    pub shader_population: usize,
    pub abi_population: usize,
    pub cpu_shader_overlap: bool,
    pub abi_cpu_overlap: bool,
    pub abi_shader_overlap: bool,
    pub first_started_lane: Option<SetupLaneKind>,
    pub mono_write_acqs_during_join: u64,
    pub mono_intern_attempts_during_join: u64,
    pub mono_intern_new_during_join: u64,
    #[cfg(test)]
    pub setup_handshake_entered: usize,
    #[cfg(test)]
    pub setup_handshake_released: usize,
    #[cfg(test)]
    pub setup_handshake_released_after_both: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupLaneKind {
    CpuArtifact,
    Shader,
    AbiWarm,
}

#[derive(Clone, Copy, Debug)]
struct SetupLaneWindow {
    started: Instant,
    finished: Instant,
    population: usize,
}

fn setup_lanes_overlap(left: SetupLaneWindow, right: SetupLaneWindow) -> bool {
    left.started < right.finished && right.started < left.finished
}

fn first_setup_lane(
    cpu: SetupLaneWindow,
    shader: SetupLaneWindow,
    abi: SetupLaneWindow,
) -> SetupLaneKind {
    let mut first = (cpu.started, SetupLaneKind::CpuArtifact);
    if shader.started < first.0 {
        first = (shader.started, SetupLaneKind::Shader);
    }
    if abi.started < first.0 {
        first = (abi.started, SetupLaneKind::AbiWarm);
    }
    first.1
}

#[derive(Clone, Debug, Default)]
pub struct EmitAttribution {
    pub bodies_examined: usize,
    pub bodies_with_work: usize,
    pub lower: AttributionPhase,
    pub inline_phase: AttributionPhase,
    pub inline: InlineAttribution,
    pub cleanup: AttributionPhase,
    pub closure: AttributionPhase,
    pub counted_loop: AttributionPhase,
    pub hoisted_inline: AttributionPhase,
    pub validation: AttributionPhase,
    pub regalloc: AttributionPhase,
    pub stringify: AttributionPhase,
    pub top_bodies: Vec<BodyAttributionOwner>,
}

#[derive(Clone, Debug, Default)]
pub struct AttributionPhase {
    pub bodies_examined: usize,
    pub bodies_with_work: usize,
    pub elapsed: Duration,
    pub instructions_visited: usize,
    pub work_volume: usize,
    pub p50_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
    pub concrete_mono_requests: usize,
    pub concrete_mono_cache_hits: usize,
    pub concrete_mono_derivations: usize,
    pub concrete_mono_unique_ids: usize,
    pub concrete_mono_cloned_nodes: usize,
    pub concrete_mono_interner_miss_cloned_nodes: usize,
    pub concrete_mono_cache_insertion_cloned_nodes: usize,
    pub concrete_mono_cache_hit_cloned_nodes: usize,
    pub concrete_mono_explicit_owned_escape_cloned_nodes: usize,
    pub concrete_mono_explicit_owned_escapes: usize,
    pub concrete_mono_lookup_ns: u64,
    pub concrete_mono_cache_lookup_ns: u64,
    pub concrete_mono_shared_handle_requests: usize,
    pub concrete_mono_clone_self_ns: u64,
    pub concrete_mono_timed_requests: usize,
    pub concrete_mono_by_consumer: [skull_mir::lower::ConcreteMonoConsumerStats;
        skull_mir::lower::ConcreteMonoConsumer::COUNT],
    pub abi_param_copy_volume: usize,
    pub abi_param_shared_views: usize,
    pub mir_instruction_allocations: usize,
    pub mir_instruction_reallocations: usize,
    pub mir_instruction_reallocation_ns: u64,
    pub mir_instruction_capacity: usize,
    pub mir_cell_allocations: usize,
    pub mir_cell_reallocations: usize,
    pub mir_cell_reallocation_ns: u64,
    pub mir_cell_capacity: usize,
    pub mir_block_allocations: usize,
    pub mir_block_reallocations: usize,
    pub mir_block_reallocation_ns: u64,
    pub mir_block_capacity: usize,
}

#[derive(Clone, Debug, Default)]
pub struct InlinePopulationAttribution {
    pub candidate_published_keys: usize,
    pub candidate_lower_attempted: usize,
    pub candidate_lowered_keys: usize,
    pub candidate_lower_failed: usize,
    pub candidate_eligible_keys: usize,
    pub candidate_rejected_before_lower: usize,
    pub candidate_rejected_after_lower: usize,
    pub candidate_full_clone_ns: u64,
    pub candidate_compact_ns: u64,
    pub candidate_raw_instructions: usize,
    pub candidate_raw_cells: usize,
    pub candidate_raw_blocks: usize,
    pub candidate_cleaned_instructions: usize,
    pub candidate_cleaned_cells: usize,
    pub candidate_cleaned_blocks: usize,
    pub candidate_full_clone_capacity: (usize, usize, usize),
    pub candidate_compact_capacity: (usize, usize, usize),
    pub ordinary_lowered_keys: usize,
    pub specialized_lowered_keys: usize,
    pub ordinary_lowering_derivations: usize,
    pub specialized_lowering_derivations: usize,
    pub ordinary_consumer_keys: usize,
    pub specialized_consumer_keys: usize,
    pub candidate_ordinary_consumer_overlap: usize,
    pub candidate_specialized_consumer_overlap: usize,
    pub ordinary_canonical_reuse_keys: usize,
    pub specialized_canonical_reuse_keys: usize,
    pub ordinary_canonical_miss_keys: usize,
    pub specialized_canonical_miss_keys: usize,
    pub ordinary_canonical_miss_instances: Vec<Instance>,
    pub specialized_canonical_miss_instances: Vec<Instance>,
    pub canonical_clone_derivations: usize,
    pub always_lowered_units: usize,
    pub always_callee_keys: usize,
    pub candidate_ordinary_overlap: usize,
    pub candidate_specialized_overlap: usize,
    pub candidate_always_callee_overlap: usize,
    pub ordinary_specialized_overlap: usize,
    pub candidate_requested_unique_keys: usize,
    pub candidate_spliced_unique_keys: usize,
    pub candidate_published_never_requested: usize,
    pub candidate_requested_never_spliced: usize,
    pub candidate_requested_not_published: usize,
    pub keys_lowered_more_than_once: usize,
    pub first_lower_ns: u64,
    pub duplicate_lower_ns: u64,
}

#[derive(Clone, Debug, Default)]
pub struct InlineAttribution {
    pub accepted_splices: usize,
    pub copied_instructions: usize,
    pub ordinary_splices: usize,
    pub specialized_splices: usize,
    pub bounded_splices: usize,
    pub hoisted_splices: usize,
    pub arena_allocations: usize,
    pub cleanup_live_before: usize,
    pub cleanup_live_after: usize,
    pub borrowed_block_scans: usize,
    pub instructions_scanned: usize,
    pub census: skull_transform::InlineScanStats,
    pub population: InlinePopulationAttribution,
    pub candidate_unique_keys: usize,
    pub candidate_snapshot_duplicates: usize,
    pub candidate_setup_keys: usize,
    pub candidate_setup_warmed: usize,
    pub candidate_setup_reused: usize,
    pub candidate_setup_eligible: usize,
    pub candidate_setup_rejected: usize,
    pub candidate_setup_lower_ns: u64,
    pub candidate_setup_cleanup_ns: u64,
    pub candidate_setup_eligibility_ns: u64,
    pub candidate_setup_worker_derivations: BTreeMap<String, usize>,
    pub candidate_setup_rejection_reasons: BTreeMap<String, usize>,
    pub top_owners: Vec<InlineOwnerAttribution>,
}

#[derive(Clone, Debug, Default)]
pub struct InlineOwnerAttribution {
    pub owner: String,
    pub splices: usize,
    pub copied_instructions: usize,
    pub splice_ns: u64,
    pub live_cloned_entries: usize,
    pub dead_cloned_entries: usize,
}

#[derive(Clone, Debug, Default)]
pub struct BodyAttributionOwner {
    pub owner: String,
    pub lower_us: u64,
    pub mir_instructions: usize,
    pub concrete_mono_requests: usize,
    pub concrete_mono_cloned_nodes: usize,
}

/// Counter totals of the build's shared `RepArena` and `MonoInterner`, taken
/// when the body-lowering fanout began (`setup`) and again at emission end
/// (`final`). Surfaced by `backend-stats`; the freeze-premise gate for
/// publishing lock-free read views is `intern_new`/`write_acqs` deltas near
/// zero after setup.
#[derive(Clone, Copy, Debug, Default)]
pub struct ArenaTraffic {
    pub rep_setup: skull_mir::lower::RepArenaStats,
    pub rep_final: skull_mir::lower::RepArenaStats,
    pub mono_setup: skull_mir::monoty::MonoInternerStats,
    pub mono_final: skull_mir::monoty::MonoInternerStats,
}

/// Coarse per-stage wall-clock for one emitted definition. Summed across every
/// cached chunk + decorator/global unit in [`assemble`] to produce the
/// `backend-stats` breakdown.
#[derive(Default, Clone, Copy)]
pub(crate) struct PhaseTimes {
    lower: Duration,
    lower_work: bool,
    inline: Duration,
    inline_work: bool,
    cleanup: Duration,
    cleanup_work: bool,
    closure: Duration,
    closure_work: bool,
    counted_loop: Duration,
    counted_loop_work: bool,
    hoisted_inline: Duration,
    hoisted_inline_work: bool,
    validation: Duration,
    validation_work: bool,
    emit: Duration,
    emit_work: bool,
    /// Regalloc subset of `emit` (read from the per-thread `REGALLOC_TIME`
    /// accumulator, reset just before this unit's emit so it is exact per-unit
    /// even when units run concurrently on a rayon worker pool).
    regalloc: Duration,
    regalloc_work: bool,
}

impl PhaseTimes {
    fn add(&mut self, o: &PhaseTimes) {
        self.lower += o.lower;
        self.lower_work |= o.lower_work;
        self.inline += o.inline;
        self.inline_work |= o.inline_work;
        self.cleanup += o.cleanup;
        self.cleanup_work |= o.cleanup_work;
        self.closure += o.closure;
        self.closure_work |= o.closure_work;
        self.counted_loop += o.counted_loop;
        self.counted_loop_work |= o.counted_loop_work;
        self.hoisted_inline += o.hoisted_inline;
        self.hoisted_inline_work |= o.hoisted_inline_work;
        self.validation += o.validation;
        self.validation_work |= o.validation_work;
        self.emit += o.emit;
        self.emit_work |= o.emit_work;
        self.regalloc += o.regalloc;
        self.regalloc_work |= o.regalloc_work;
    }
}

#[derive(Default, Clone)]
struct InlineWork {
    accepted_splices: usize,
    copied_instructions: usize,
    ordinary_splices: usize,
    specialized_splices: usize,
    bounded_splices: usize,
    hoisted_splices: usize,
    arena_allocations: usize,
    cleanup_live_before: usize,
    cleanup_live_after: usize,
    borrowed_block_scans: usize,
    instructions_scanned: usize,
    census: skull_transform::InlineScanStats,
    owners: BTreeMap<String, (usize, usize, u64, usize, usize)>,
}

impl InlineWork {
    fn add_records(&mut self, records: &[skull_transform::SpliceRecord]) {
        self.accepted_splices += records.len();
        for record in records {
            self.copied_instructions += record.callee_insts;
            self.bounded_splices += usize::from(record.bounded);
            self.census.clone.add(record.clone);
            let owner = inline_owner_key(record.callee);
            let entry = self.owners.entry(owner).or_default();
            entry.0 += 1;
            entry.1 += record.callee_insts;
            entry.2 += record.splice_ns;
            entry.3 +=
                record.clone.live_instructions + record.clone.live_cells + record.clone.live_blocks;
            entry.4 += record.clone.dead_cloned_instructions
                + record.clone.dead_cloned_cells
                + record.clone.dead_cloned_blocks;
        }
    }

    fn add_allocations(&mut self, before: (usize, usize, usize), after: (usize, usize, usize)) {
        self.arena_allocations += after.0.saturating_sub(before.0);
        self.arena_allocations += after.1.saturating_sub(before.1);
        self.arena_allocations += after.2.saturating_sub(before.2);
    }

    fn add_scan(&mut self, stats: skull_transform::InlineScanStats) {
        self.borrowed_block_scans += stats.borrowed_block_scans;
        self.instructions_scanned += stats.instructions_scanned;
        self.census.add(stats);
    }

    fn add(&mut self, other: InlineWork) {
        self.accepted_splices += other.accepted_splices;
        self.copied_instructions += other.copied_instructions;
        self.ordinary_splices += other.ordinary_splices;
        self.specialized_splices += other.specialized_splices;
        self.bounded_splices += other.bounded_splices;
        self.hoisted_splices += other.hoisted_splices;
        self.arena_allocations += other.arena_allocations;
        self.cleanup_live_before += other.cleanup_live_before;
        self.cleanup_live_after += other.cleanup_live_after;
        self.borrowed_block_scans += other.borrowed_block_scans;
        self.instructions_scanned += other.instructions_scanned;
        self.census.add(other.census);
        for (owner, (splices, copied, splice_ns, live, dead)) in other.owners {
            let entry = self.owners.entry(owner).or_default();
            entry.0 += splices;
            entry.1 += copied;
            entry.2 += splice_ns;
            entry.3 += live;
            entry.4 += dead;
        }
    }
}

#[derive(Default, Clone)]
struct UnitAttribution {
    owner: String,
    examined: bool,
    times: PhaseTimes,
    hir_nodes: usize,
    mir_instructions: usize,
    lower_facts: skull_mir::lower::LowerAttribution,
    inline: InlineWork,
    lowered_instance: Option<(Instance, LoweredInstanceKind)>,
    consumer_instance: Option<(Instance, LoweredInstanceKind)>,
    canonical_reused: bool,
    always_lowered: bool,
    always_callee_keys: Vec<Instance>,
}

#[derive(Clone, Copy)]
enum LoweredInstanceKind {
    Ordinary,
    Specialized,
}

#[derive(Default)]
struct AttributionAccumulator {
    result: EmitAttribution,
    lower_samples: Vec<u64>,
    inline_samples: Vec<u64>,
    cleanup_samples: Vec<u64>,
    closure_samples: Vec<u64>,
    counted_loop_samples: Vec<u64>,
    hoisted_inline_samples: Vec<u64>,
    validation_samples: Vec<u64>,
    regalloc_samples: Vec<u64>,
    stringify_samples: Vec<u64>,
    top_bodies: Vec<BodyAttributionOwner>,
    census_enabled: bool,
    candidate_published_keys: FxHashSet<Instance>,
    candidate_eligible_keys: FxHashSet<Instance>,
    ordinary_lowered_keys: FxHashSet<Instance>,
    specialized_lowered_keys: FxHashSet<Instance>,
    ordinary_consumer_keys: FxHashSet<Instance>,
    specialized_consumer_keys: FxHashSet<Instance>,
    ordinary_canonical_reuse_keys: FxHashSet<Instance>,
    specialized_canonical_reuse_keys: FxHashSet<Instance>,
    canonical_clone_derivations: usize,
    candidate_lower_ns_by_key: BTreeMap<Instance, Vec<u64>>,
    ordinary_lower_ns_by_key: BTreeMap<Instance, Vec<u64>>,
    specialized_lower_ns_by_key: BTreeMap<Instance, Vec<u64>>,
    always_callee_keys: FxHashSet<Instance>,
    always_lowered_units: usize,
}

fn inline_owner_key(fref: FuncRef) -> String {
    format!(
        "m{}:f{}",
        fref.module.index(),
        u32::from(fref.func.into_raw())
    )
}

fn hir_nodes_for_owner(ws: &Workspace, mid: ModuleId, owner: BodyOwner) -> usize {
    ws.module(mid)
        .bodies
        .get(&owner)
        .map(|body| {
            body.expr_arena.len()
                + body.binding_arena.len()
                + body.statement_arena.len()
                + body.pattern_arena.len()
                + body.match_arm_arena.len()
        })
        .unwrap_or(0)
}

fn mir_live_instructions(mf: &MirFunc) -> usize {
    mf.blocks.iter().map(|(_, block)| block.insts.len()).sum()
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().max(1) as u64
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

fn record_phase(
    phase: &mut AttributionPhase,
    samples: &mut Vec<u64>,
    duration: Duration,
    did_work: bool,
    work_volume: usize,
    instructions: usize,
) {
    phase.bodies_examined += 1;
    phase.elapsed += duration;
    phase.work_volume += work_volume;
    phase.instructions_visited += instructions;
    if duration > Duration::ZERO {
        samples.push(duration_us(duration));
    }
    if did_work {
        phase.bodies_with_work += 1;
    }
}

fn finish_distribution(phase: &mut AttributionPhase, samples: &mut [u64]) {
    samples.sort_unstable();
    if samples.is_empty() {
        return;
    }
    let at = |percentile: usize| samples[(samples.len() - 1) * percentile / 100];
    phase.p50_us = at(50);
    phase.p95_us = at(95);
    phase.max_us = *samples.last().expect("nonempty duration sample");
}

impl AttributionAccumulator {
    fn add_unit(&mut self, unit: &UnitAttribution) {
        if !unit.examined {
            return;
        }
        self.result.bodies_examined += 1;
        if unit.mir_instructions > 0 {
            self.result.bodies_with_work += 1;
        }
        if self.census_enabled {
            if let Some((instance, kind)) = &unit.consumer_instance {
                match kind {
                    LoweredInstanceKind::Ordinary => {
                        self.ordinary_consumer_keys.insert(instance.clone());
                        if unit.canonical_reused {
                            self.ordinary_canonical_reuse_keys.insert(instance.clone());
                        }
                    }
                    LoweredInstanceKind::Specialized => {
                        self.specialized_consumer_keys.insert(instance.clone());
                        if unit.canonical_reused {
                            self.specialized_canonical_reuse_keys
                                .insert(instance.clone());
                        }
                    }
                }
                if unit.canonical_reused {
                    self.canonical_clone_derivations += 1;
                }
            }
            if let Some((instance, kind)) = &unit.lowered_instance {
                match kind {
                    LoweredInstanceKind::Ordinary => {
                        self.ordinary_lowered_keys.insert(instance.clone());
                        self.ordinary_lower_ns_by_key
                            .entry(instance.clone())
                            .or_default()
                            .push(duration_ns(unit.times.lower));
                    }
                    LoweredInstanceKind::Specialized => {
                        self.specialized_lowered_keys.insert(instance.clone());
                        self.specialized_lower_ns_by_key
                            .entry(instance.clone())
                            .or_default()
                            .push(duration_ns(unit.times.lower));
                    }
                }
            }
            if unit.always_lowered {
                self.always_lowered_units += 1;
            }
            self.always_callee_keys
                .extend(unit.always_callee_keys.iter().cloned());
        }
        let times = &unit.times;
        record_phase(
            &mut self.result.lower,
            &mut self.lower_samples,
            times.lower,
            times.lower_work,
            unit.hir_nodes,
            unit.mir_instructions,
        );
        self.result.lower.concrete_mono_requests += unit.lower_facts.concrete_mono_requests;
        self.result.lower.concrete_mono_cache_hits += unit.lower_facts.concrete_mono_cache_hits;
        self.result.lower.concrete_mono_derivations += unit.lower_facts.concrete_mono_derivations;
        self.result.lower.concrete_mono_unique_ids += unit.lower_facts.concrete_mono_unique_ids;
        self.result.lower.concrete_mono_cloned_nodes += unit.lower_facts.concrete_mono_cloned_nodes;
        self.result.lower.concrete_mono_interner_miss_cloned_nodes +=
            unit.lower_facts.concrete_mono_interner_miss_cloned_nodes;
        self.result.lower.concrete_mono_cache_insertion_cloned_nodes +=
            unit.lower_facts.concrete_mono_cache_insertion_cloned_nodes;
        self.result.lower.concrete_mono_cache_hit_cloned_nodes +=
            unit.lower_facts.concrete_mono_cache_hit_cloned_nodes;
        self.result
            .lower
            .concrete_mono_explicit_owned_escape_cloned_nodes += unit
            .lower_facts
            .concrete_mono_explicit_owned_escape_cloned_nodes;
        self.result.lower.concrete_mono_explicit_owned_escapes +=
            unit.lower_facts.concrete_mono_explicit_owned_escapes;
        self.result.lower.concrete_mono_lookup_ns += unit.lower_facts.concrete_mono_lookup_ns;
        self.result.lower.concrete_mono_cache_lookup_ns +=
            unit.lower_facts.concrete_mono_cache_lookup_ns;
        self.result.lower.concrete_mono_shared_handle_requests +=
            unit.lower_facts.concrete_mono_shared_handle_requests;
        self.result.lower.concrete_mono_clone_self_ns +=
            unit.lower_facts.concrete_mono_clone_self_ns;
        self.result.lower.concrete_mono_timed_requests +=
            unit.lower_facts.concrete_mono_timed_requests;
        for (total, child) in self
            .result
            .lower
            .concrete_mono_by_consumer
            .iter_mut()
            .zip(unit.lower_facts.concrete_mono_by_consumer.iter())
        {
            total.requests += child.requests;
            total.cloned_nodes += child.cloned_nodes;
            total.explicit_owned_escapes += child.explicit_owned_escapes;
        }
        self.result.lower.abi_param_copy_volume += unit.lower_facts.abi_param_copy_volume;
        self.result.lower.abi_param_shared_views += unit.lower_facts.abi_param_shared_views;
        self.result.lower.mir_instruction_allocations +=
            unit.lower_facts.mir_instruction_allocations;
        self.result.lower.mir_instruction_reallocations +=
            unit.lower_facts.mir_instruction_reallocations;
        self.result.lower.mir_instruction_reallocation_ns +=
            unit.lower_facts.mir_instruction_reallocation_ns;
        self.result.lower.mir_instruction_capacity += unit.lower_facts.mir_instruction_capacity;
        self.result.lower.mir_cell_allocations += unit.lower_facts.mir_cell_allocations;
        self.result.lower.mir_cell_reallocations += unit.lower_facts.mir_cell_reallocations;
        self.result.lower.mir_cell_reallocation_ns += unit.lower_facts.mir_cell_reallocation_ns;
        self.result.lower.mir_cell_capacity += unit.lower_facts.mir_cell_capacity;
        self.result.lower.mir_block_allocations += unit.lower_facts.mir_block_allocations;
        self.result.lower.mir_block_reallocations += unit.lower_facts.mir_block_reallocations;
        self.result.lower.mir_block_reallocation_ns += unit.lower_facts.mir_block_reallocation_ns;
        self.result.lower.mir_block_capacity += unit.lower_facts.mir_block_capacity;
        record_phase(
            &mut self.result.inline_phase,
            &mut self.inline_samples,
            times.inline,
            times.inline_work,
            unit.inline.accepted_splices,
            unit.inline.copied_instructions,
        );
        record_phase(
            &mut self.result.cleanup,
            &mut self.cleanup_samples,
            times.cleanup,
            times.cleanup_work,
            unit.mir_instructions,
            0,
        );
        record_phase(
            &mut self.result.closure,
            &mut self.closure_samples,
            times.closure,
            times.closure_work,
            0,
            0,
        );
        record_phase(
            &mut self.result.counted_loop,
            &mut self.counted_loop_samples,
            times.counted_loop,
            times.counted_loop_work,
            0,
            0,
        );
        record_phase(
            &mut self.result.hoisted_inline,
            &mut self.hoisted_inline_samples,
            times.hoisted_inline,
            times.hoisted_inline_work,
            0,
            0,
        );
        record_phase(
            &mut self.result.validation,
            &mut self.validation_samples,
            times.validation,
            times.validation_work,
            0,
            0,
        );
        record_phase(
            &mut self.result.regalloc,
            &mut self.regalloc_samples,
            times.regalloc,
            times.regalloc_work,
            0,
            0,
        );
        let stringify = times.emit.saturating_sub(times.regalloc);
        record_phase(
            &mut self.result.stringify,
            &mut self.stringify_samples,
            stringify,
            times.emit_work,
            unit.mir_instructions,
            0,
        );
        self.result.inline.accepted_splices += unit.inline.accepted_splices;
        self.result.inline.copied_instructions += unit.inline.copied_instructions;
        self.result.inline.ordinary_splices += unit.inline.ordinary_splices;
        self.result.inline.specialized_splices += unit.inline.specialized_splices;
        self.result.inline.bounded_splices += unit.inline.bounded_splices;
        self.result.inline.hoisted_splices += unit.inline.hoisted_splices;
        self.result.inline.arena_allocations += unit.inline.arena_allocations;
        self.result.inline.cleanup_live_before += unit.inline.cleanup_live_before;
        self.result.inline.cleanup_live_after += unit.inline.cleanup_live_after;
        self.result.inline.borrowed_block_scans += unit.inline.borrowed_block_scans;
        self.result.inline.instructions_scanned += unit.inline.instructions_scanned;
        self.result.inline.census.add(unit.inline.census.clone());
        for (owner, (splices, copied, splice_ns, live, dead)) in &unit.inline.owners {
            let entry = self
                .result
                .inline
                .top_owners
                .iter_mut()
                .find(|item| item.owner == *owner);
            if let Some(entry) = entry {
                entry.splices += *splices;
                entry.copied_instructions += *copied;
                entry.splice_ns += *splice_ns;
                entry.live_cloned_entries += *live;
                entry.dead_cloned_entries += *dead;
            } else {
                self.result.inline.top_owners.push(InlineOwnerAttribution {
                    owner: owner.clone(),
                    splices: *splices,
                    copied_instructions: *copied,
                    splice_ns: *splice_ns,
                    live_cloned_entries: *live,
                    dead_cloned_entries: *dead,
                });
            }
        }
        self.top_bodies.push(BodyAttributionOwner {
            owner: unit.owner.clone(),
            lower_us: duration_us(times.lower),
            mir_instructions: unit.mir_instructions,
            concrete_mono_requests: unit.lower_facts.concrete_mono_requests,
            concrete_mono_cloned_nodes: unit.lower_facts.concrete_mono_cloned_nodes,
        });
    }

    fn finish(mut self) -> EmitAttribution {
        finish_distribution(&mut self.result.lower, &mut self.lower_samples);
        finish_distribution(&mut self.result.inline_phase, &mut self.inline_samples);
        finish_distribution(&mut self.result.cleanup, &mut self.cleanup_samples);
        finish_distribution(&mut self.result.closure, &mut self.closure_samples);
        finish_distribution(
            &mut self.result.counted_loop,
            &mut self.counted_loop_samples,
        );
        finish_distribution(
            &mut self.result.hoisted_inline,
            &mut self.hoisted_inline_samples,
        );
        finish_distribution(&mut self.result.validation, &mut self.validation_samples);
        finish_distribution(&mut self.result.regalloc, &mut self.regalloc_samples);
        finish_distribution(&mut self.result.stringify, &mut self.stringify_samples);
        self.result.inline.top_owners.sort_by(|a, b| {
            b.copied_instructions
                .cmp(&a.copied_instructions)
                .then_with(|| a.owner.cmp(&b.owner))
        });
        self.result.inline.top_owners.truncate(8);
        self.result.inline.candidate_unique_keys = self
            .result
            .inline
            .census
            .candidate_requested_keys
            .iter()
            .collect::<FxHashSet<_>>()
            .len();
        let ordinary_keys: FxHashSet<_> = self.result.inline.census.ordinary_keys.iter().collect();
        self.result.inline.candidate_snapshot_duplicates = self
            .result
            .inline
            .census
            .candidate_keys
            .iter()
            .filter(|key| ordinary_keys.contains(key))
            .count();
        if self.census_enabled {
            let published = &self.candidate_published_keys;
            let requested: FxHashSet<_> = self
                .result
                .inline
                .census
                .candidate_requested_keys
                .iter()
                .cloned()
                .collect();
            let spliced: FxHashSet<_> = self
                .result
                .inline
                .census
                .spliced_keys
                .iter()
                .cloned()
                .collect();
            let mut lowerings_by_key: BTreeMap<Instance, Vec<u64>> = BTreeMap::new();
            for (key, times) in &self.candidate_lower_ns_by_key {
                lowerings_by_key
                    .entry(key.clone())
                    .or_default()
                    .extend(times.iter().copied());
            }
            for (key, times) in &self.ordinary_lower_ns_by_key {
                lowerings_by_key
                    .entry(key.clone())
                    .or_default()
                    .extend(times.iter().copied());
            }
            for (key, times) in &self.specialized_lower_ns_by_key {
                lowerings_by_key
                    .entry(key.clone())
                    .or_default()
                    .extend(times.iter().copied());
            }
            let mut keys_lowered_more_than_once = 0;
            let mut first_lower_ns = 0;
            let mut duplicate_lower_ns = 0;
            for times in lowerings_by_key.values() {
                if times.len() > 1 {
                    keys_lowered_more_than_once += 1;
                }
                if let Some((first, rest)) = times.split_first() {
                    first_lower_ns += *first;
                    duplicate_lower_ns += rest.iter().sum::<u64>();
                }
            }
            let ordinary_lowering_derivations =
                self.ordinary_lower_ns_by_key.values().map(Vec::len).sum();
            let specialized_lowering_derivations = self
                .specialized_lower_ns_by_key
                .values()
                .map(Vec::len)
                .sum();
            let candidate_ordinary_consumer_overlap = self
                .candidate_eligible_keys
                .intersection(&self.ordinary_consumer_keys)
                .count();
            let candidate_specialized_consumer_overlap = self
                .candidate_eligible_keys
                .intersection(&self.specialized_consumer_keys)
                .count();
            let ordinary_canonical_miss_instances: Vec<_> = self
                .candidate_eligible_keys
                .intersection(&self.ordinary_consumer_keys)
                .filter(|key| !self.ordinary_canonical_reuse_keys.contains(*key))
                .cloned()
                .collect();
            let specialized_canonical_miss_instances: Vec<_> = self
                .candidate_eligible_keys
                .intersection(&self.specialized_consumer_keys)
                .filter(|key| !self.specialized_canonical_reuse_keys.contains(*key))
                .cloned()
                .collect();
            let population = &mut self.result.inline.population;
            population.candidate_published_keys = published.len();
            population.candidate_eligible_keys = self.candidate_eligible_keys.len();
            population.ordinary_lowered_keys = self.ordinary_lowered_keys.len();
            population.specialized_lowered_keys = self.specialized_lowered_keys.len();
            population.ordinary_lowering_derivations = ordinary_lowering_derivations;
            population.specialized_lowering_derivations = specialized_lowering_derivations;
            population.ordinary_consumer_keys = self.ordinary_consumer_keys.len();
            population.specialized_consumer_keys = self.specialized_consumer_keys.len();
            population.candidate_ordinary_consumer_overlap = candidate_ordinary_consumer_overlap;
            population.candidate_specialized_consumer_overlap =
                candidate_specialized_consumer_overlap;
            population.ordinary_canonical_reuse_keys = self.ordinary_canonical_reuse_keys.len();
            population.specialized_canonical_reuse_keys =
                self.specialized_canonical_reuse_keys.len();
            population.ordinary_canonical_miss_keys = ordinary_canonical_miss_instances.len();
            population.specialized_canonical_miss_keys = specialized_canonical_miss_instances.len();
            population.ordinary_canonical_miss_instances = ordinary_canonical_miss_instances;
            population.specialized_canonical_miss_instances = specialized_canonical_miss_instances;
            population.canonical_clone_derivations = self.canonical_clone_derivations;
            population.always_lowered_units = self.always_lowered_units;
            population.always_callee_keys = self.always_callee_keys.len();
            population.candidate_ordinary_overlap =
                published.intersection(&self.ordinary_lowered_keys).count();
            population.candidate_specialized_overlap = published
                .intersection(&self.specialized_lowered_keys)
                .count();
            population.candidate_always_callee_overlap =
                published.intersection(&self.always_callee_keys).count();
            population.ordinary_specialized_overlap = self
                .ordinary_lowered_keys
                .intersection(&self.specialized_lowered_keys)
                .count();
            population.candidate_requested_unique_keys = requested.len();
            population.candidate_spliced_unique_keys = spliced.len();
            population.candidate_published_never_requested =
                published.difference(&requested).count();
            population.candidate_requested_never_spliced = requested.difference(&spliced).count();
            population.candidate_requested_not_published = requested.difference(published).count();
            population.candidate_lower_failed = population
                .candidate_lower_attempted
                .saturating_sub(population.candidate_lowered_keys);
            population.candidate_rejected_after_lower = population
                .candidate_lowered_keys
                .saturating_sub(population.candidate_eligible_keys);
            population.keys_lowered_more_than_once = keys_lowered_more_than_once;
            population.first_lower_ns = first_lower_ns;
            population.duplicate_lower_ns = duplicate_lower_ns;
        }
        self.top_bodies.sort_by(|a, b| {
            b.lower_us
                .cmp(&a.lower_us)
                .then_with(|| a.owner.cmp(&b.owner))
        });
        self.top_bodies.truncate(8);
        self.result.top_bodies = self.top_bodies;
        self.result
    }
}

/// One emitted definition (function / decorator / global-init): the Lua text to
/// append plus the facts the surrounding assembly merges back in. Produced
/// independently per definition (see [`par_map`]) so the heavy lower+cleanup+emit
/// work fans out; results are folded back in source order so output is stable.
#[derive(Default)]
struct UnitOut {
    /// Lua to append to the chunk (empty for a skipped or no-op definition).
    lua: String,
    /// `(lua_name, reason)` if the new path could not lower this definition.
    skipped: Option<(String, String)>,
    /// A REJECTION, as opposed to the lenient `skipped` channel above (which is
    /// only ever printed under `SKULL_DEBUG_SKIPPED`). A decorator body or global
    /// initializer that lowered but then failed a post-lowering invariant must
    /// fail the compile: routing it to `skipped` would emit the broken Lua and say
    /// nothing, which is the miscompile this check exists to prevent.
    hard_error: Option<CompilerError>,
    /// `(emit_key, direct_global_reads, direct_callees)` for global-init ordering.
    refs: Option<(Ustr, Vec<Ustr>, Vec<Ustr>)>,
    /// `(global_name, init_key, load_line)` for a global initializer.
    pending_init: Option<(Ustr, Ustr, String)>,
    /// Callees this definition still calls -- reachability roots, since decorator
    /// bodies and global initializers are always emitted.
    calls: Vec<FuncRef>,
    /// Specialized callees this definition needs emitted (see `Chunk::specs`).
    specs: Vec<skull_mir::bound::SpecDemand>,
    /// Compiler-owned logical callable identities in this emitted body.
    code_artifact: skull_mir::code_identity::CodeArtifact,
    /// Module-global names read/written (fn-as-value edges; see `Chunk`).
    global_names: Vec<Ustr>,
    /// Comptime inputs this unit's body baked in (`build_var`/`embed_string`).
    /// Always-emitted units have no Chunk to carry these, and ALL in-tree
    /// production comptime reads are global initializers -- dropping them here
    /// is how the cimgui embeds went missing from the watch set.
    comptime_reads: Vec<skull_mir::mir::ComptimeRead>,
    allocation_bodies: Vec<crate::allocation_report::AllocationBody>,
    times: PhaseTimes,
    attribution: UnitAttribution,
}

/// Record one emitted definition's Lua, emit time, and (for the non-coroutine
/// paths) its call-graph facts. `collect_refs` carries the emit key when the
/// definition seeds global-init ordering; `None` for coroutines, which never do.
/// Reads the per-thread regalloc accumulator that the caller reset before emit.
fn emit_lowered(
    ws: &Workspace,
    out: &mut UnitOut,
    mf: &MirFunc,
    lua: String,
    body_name: &str,
    collect_refs: Option<Ustr>,
    t_emit: Duration,
) {
    let tv = Instant::now();
    if let Some(key) = collect_refs {
        let (gs, cs) = direct_refs(ws, mf);
        out.refs = Some((key, gs, cs));
    }
    out.calls = skull_mir::reach::call_targets(mf);
    out.global_names = skull_mir::reach::global_ref_names(mf);
    out.comptime_reads = mf.comptime_reads.clone();
    out.lua = lua;
    out.code_artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree_with_volatile_vars(
        mf,
        &ws.comptime_inputs.volatile_vars,
    );
    out.code_artifact.set_body_names(mf, body_name);
    out.times.validation += tv.elapsed();
    out.times.validation_work = true;
    out.times.emit += t_emit;
    out.times.emit_work |= t_emit > Duration::ZERO;
    let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(t_emit);
    out.times.regalloc += regalloc;
    out.times.regalloc_work |= regalloc > Duration::ZERO;
}

/// Lower + emit one decorator definition (`deco <name>` -> `__deco_<name>`).
fn emit_decorator_unit(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    mid: ModuleId,
    did: Idx<bone_semantic::hir::DecoratorDef>,
) -> UnitOut {
    let mut out = UnitOut::default();
    let d = &ws.module(mid).decorators[did];
    let deco_name = crate::decorator_fn_name(d.def_key.as_str());
    out.attribution.owner = format!("m{}:deco{}", mid.index(), u32::from(did.into_raw()));
    let tl = Instant::now();
    let owner = skull_mir::concrete::ConcreteOwner::Decorator {
        module: mid,
        index: did.into_raw().into_u32(),
    };
    let concrete_plan = concrete.plan_for_owner(&owner);
    let lowered = concrete_plan
        .ok_or_else(|| {
            CompilerError::new_with_label(d.span, "no concrete plan for decorator".to_string())
        })
        .and_then(|plan| {
            skull_mir::lower::lower_decorator_with_plans(
                ws,
                scx,
                plan,
                &concrete.plans,
                ustr::Ustr::from(deco_name.as_str()),
            )
        });
    out.times.lower = tl.elapsed();
    match lowered {
        Ok(mut mf) => {
            out.attribution.examined = true;
            out.attribution.hir_nodes = hir_nodes_for_owner(ws, mid, BodyOwner::Decorator(did));
            out.attribution.mir_instructions = mf.insts.len();
            out.attribution.lower_facts = mf.lower_attribution.clone();
            out.times.lower_work = out.attribution.mir_instructions > 0;
            let cleanup_before = mir_live_instructions(&mf);
            let tc = Instant::now();
            if let Err(e) = skull_transform::cleanup_with_nobox(ws, &mut mf) {
                out.hard_error = Some(e);
                return out;
            }
            out.times.cleanup = tc.elapsed();
            out.times.cleanup_work = mir_live_instructions(&mf) != cleanup_before;
            // Decorator bodies can carry lambdas too -- convert like an ordinary fn.
            let pre_nested = mf.nested_fns.len();
            let tclosure = Instant::now();
            skull_transform::convert_closures(&mut mf, &deco_name);
            out.times.closure = tclosure.elapsed();
            out.times.closure_work = mf.nested_fns.len() > pre_nested;
            for nf in &mf.nested_fns[pre_nested..] {
                cg.mint_synthetic(nf.name.to_string());
            }
            let owner = bone_semantic::hir::BodyOwner::Decorator(did);
            let plan = concrete_plan.expect("a lowered decorator has a concrete plan");
            out.attribution.always_lowered = true;
            out.attribution
                .always_callee_keys
                .extend(plan.callees.values().cloned());
            let ctx = skull_mir::bound::concrete_body_ctx(
                ws,
                mid,
                owner,
                &plan.bound_tree,
                scx.interner(),
            );
            let tv = Instant::now();
            let check =
                check_unit_spec_bindings(ws, &ctx, mid, &deco_name, &mf, d.span, &mut out.specs);
            out.times.validation += tv.elapsed();
            out.times.validation_work = true;
            if let Err(e) = check {
                out.hard_error = Some(e);
                return out;
            }
            skull_codegen_lua::regalloc_time_reset();
            let te = Instant::now();
            // A decorator / global-initializer body runs once at chunk load,
            // outside the definitions section the binding pass covers.
            let emission = emit_named_with_allocation_report(
                ws,
                cg,
                scx.interner(),
                &deco_name,
                &mf,
                &[],
                None,
            );
            let mut lua = emission.lua;
            if let Some(body) = crate::allocation_report::collect_emitted_body(
                ws,
                scx.interner(),
                &deco_name,
                d.name.as_str(),
                &mf,
                emission.allocation_sites,
            ) {
                out.allocation_bodies.push(body);
            }
            let root_emit = te.elapsed();
            out.times.emit += root_emit;
            out.times.emit_work |= root_emit > Duration::ZERO;
            let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(root_emit);
            out.times.regalloc += regalloc;
            out.times.regalloc_work |= regalloc > Duration::ZERO;
            if let Err(e) = emit_pass_nested_fns(
                ws,
                cg,
                scx,
                &mut mf.nested_fns,
                &mf.statics,
                Some(&mut out.allocation_bodies),
                &mut lua,
                &mut out.times,
            ) {
                out.hard_error = Some(e);
                return out;
            }
            emit_lowered(
                ws,
                &mut out,
                &mf,
                lua,
                &deco_name,
                Some(Ustr::from(&deco_name)),
                Duration::ZERO,
            );
            out.attribution.times = out.times;
        }
        Err(e) => out.skipped = Some((deco_name, e.message)),
    }
    out
}

/// The assignment target of a global's init line: its own name, or -- for a
/// `nobox` global, whose storage IS its leaves -- the comma-separated leaf list
/// bound from the initializer's multiple return. Leaf names are compiler-minted
/// top-level names, so they go through the collision sink.
fn global_init_target(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    g: &bone_semantic::hir::GlobalDef,
) -> Result<String, CompilerError> {
    let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty)
        .map_err(|e| CompilerError::new_no_location(e.message))?;
    // The LHS is an identifier, so it is the global's emitted symbol; a v1
    // DefKey never reaches the artifact.
    let symbol = skull_mir::mangle::symbol_of_key(g.name);
    let Some(leaves) = plan.leaf_names(symbol) else {
        return Ok(symbol.to_string());
    };
    Ok(leaves
        .iter()
        .map(|l| cg.mint_synthetic(l.to_string()))
        .collect::<Vec<_>>()
        .join(", "))
}

/// The load-time initialization of one mutable global. It runs only when this
/// load created the global storage: a program loaded over live storage keeps
/// every value the running program wrote.
fn global_init_line(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    g: &bone_semantic::hir::GlobalDef,
    init_name: &str,
) -> Result<String, CompilerError> {
    let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty).map_err(|_| {
        CompilerError::new_no_location(format!("no storage plan for global `{}`", g.name))
    })?;
    let slot = |leaf: Ustr| {
        cg.state_slot(leaf).ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "global `{}` has no compiler-owned storage slot for `{leaf}`",
                g.name
            ))
        })
    };
    let cell = cg.state_table();
    match plan.leaf_names(skull_mir::mangle::symbol_of_key(g.name)) {
        Some(leaves) => {
            let mut line = String::from("if __rock_globals_fresh then\n");
            line.push_str(&format!(
                "  local __rock_init_values = {{ {init_name}() }}\n"
            ));
            for (index, leaf) in leaves.iter().enumerate() {
                line.push_str(&format!(
                    "  {cell}[{}] = __rock_init_values[{}]\n",
                    slot(*leaf)?,
                    index + 1
                ));
            }
            line.push_str("end\n");
            Ok(line)
        }
        None => Ok(format!(
            "if __rock_globals_fresh then {cell}[{}] = {init_name}() end\n",
            slot(g.name)?
        )),
    }
}

pub fn lua_global_state_slots(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
) -> Result<std::collections::BTreeMap<ustr::Ustr, u32>, CompilerError> {
    let mut slots = skull_mir::global_storage::global_storage_keys(ws, scx)
        .map_err(CompilerError::new_no_location)?
        .into_iter()
        .collect::<Vec<_>>();
    slots.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(slots
        .into_iter()
        .enumerate()
        .map(|(index, (physical, _))| {
            let slot = u32::try_from(index + 1).expect("Lua global slot population exceeds u32");
            (physical, slot)
        })
        .collect())
}

/// The line that opens (or, on a later load, validates) the global storage and
/// seats this chunk's storage cell. The table is sized to its rehash fixpoint:
/// the array part at the highest slot's next power of two, plus one for index
/// 0, which is what LuaJIT's `rehashtab` settles on once more than half of that
/// range is populated, so a later rehash cannot move it -- and a hash part with
/// twice the statics' string keys, so no static's first store overflows it.
/// `ROCK_SKULL_STATE_RESERVE=0` leaves it to grow from `{}`.
fn globals_open_lua(layout: &str, cg: &CodegenCtx, static_count: usize) -> String {
    let (narr, nhash) = if skull_codegen_lua::state_reserve_mode() {
        (
            cg.state_slot_max().next_power_of_two() + 1,
            (static_count * 4).next_power_of_two().max(16),
        )
    } else {
        (0, 0)
    };
    format!(
        "{} = __rock_globals_open({layout:?}, {narr}, {nhash})\n",
        skull_codegen_lua::STATE_CELL
    )
}

/// The global directory every reflection reader and the `_G` bridge use.
fn globals_directory_lua(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
) -> Result<String, CompilerError> {
    use skull_codegen_lua::lua_string;
    let mut entries = Vec::new();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (_, global) in module.globals.iter().filter(|(_, g)| !g.is_const) {
            let identity = skull_mir::global_storage::global_identity(module.file, global.name);
            let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, global.ty)
                .map_err(|_| {
                    CompilerError::new_no_location(format!(
                        "no storage plan for global `{}`",
                        global.name
                    ))
                })?;
            let physical = skull_mir::global_storage::physical_leaf_names(&plan, global.name);
            let slots = physical
                .iter()
                .map(|leaf| {
                    cg.state_slot(*leaf)
                        .map(|slot| slot.to_string())
                        .ok_or_else(|| {
                            CompilerError::new_no_location(format!(
                                "global `{}` has no compiler-owned storage slot for `{leaf}`",
                                global.name
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let name = identity
                .rsplit("::")
                .next()
                .unwrap_or(&identity)
                .to_string();
            entries.push((identity, name, physical, slots));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from("_G.__rock_globals_publish({");
    for (identity, name, physical, slots) in entries {
        let physical = physical
            .iter()
            .map(|leaf| lua_string(leaf))
            .collect::<Vec<_>>()
            .join(",");
        let _ = writeln!(
            out,
            "{{name={},identity={},physical={{{physical}}},slots={{{}}}}},",
            lua_string(&name),
            lua_string(&identity),
            slots.join(",")
        );
    }
    out.push_str("})\n");
    Ok(out)
}

/// Lower + emit one global's initializer fn, recording its `pending_init` line.
fn emit_global_unit(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    mid: ModuleId,
    gid: Idx<bone_semantic::hir::GlobalDef>,
) -> UnitOut {
    let mut out = UnitOut::default();
    let g = &ws.module(mid).globals[gid];
    let init_name = crate::global_init_fn_name(skull_mir::mangle::symbol_of_key(g.name).as_str());
    out.attribution.owner = format!("m{}:global{}", mid.index(), u32::from(gid.into_raw()));
    let tl = Instant::now();
    let owner = skull_mir::concrete::ConcreteOwner::Global {
        module: mid,
        index: gid.into_raw().into_u32(),
    };
    let concrete_plan = concrete.plan_for_owner(&owner);
    let lowered = match concrete_plan {
        Some(plan) => skull_mir::lower::lower_global_with_plans(
            ws,
            scx,
            plan,
            &concrete.plans,
            ustr::Ustr::from(init_name.as_str()),
        ),
        None if ws
            .module(mid)
            .bodies
            .get(&bone_semantic::hir::BodyOwner::Global(gid))
            .and_then(|body| body.root)
            .is_none() =>
        {
            Ok(None)
        }
        None => Err(CompilerError::new_with_label(
            g.name_span,
            "no concrete plan for global initializer".to_string(),
        )),
    };
    out.times.lower = tl.elapsed();
    match lowered {
        Ok(Some(mut mf)) => {
            out.attribution.examined = true;
            out.attribution.hir_nodes = hir_nodes_for_owner(ws, mid, BodyOwner::Global(gid));
            out.attribution.mir_instructions = mf.insts.len();
            out.attribution.lower_facts = mf.lower_attribution.clone();
            out.times.lower_work = out.attribution.mir_instructions > 0;
            let cleanup_before = mir_live_instructions(&mf);
            let tc = Instant::now();
            if let Err(e) = skull_transform::cleanup_with_nobox(ws, &mut mf) {
                out.hard_error = Some(e);
                return out;
            }
            out.times.cleanup = tc.elapsed();
            out.times.cleanup_work = mir_live_instructions(&mf) != cleanup_before;
            // A global-initializer body can carry lambdas (e.g. a `[TileDef]`
            // literal with fn fields) -- route them through the shared pass, same
            // as an ordinary fn. Their global reads are NOT init-ordering
            // constraints (the fn value is stored, not called at init), so `refs`
            // is computed on the post-pass `mf` where those reads live in the
            // hoisted siblings, not the main body.
            let pre_nested = mf.nested_fns.len();
            let tclosure = Instant::now();
            skull_transform::convert_closures(&mut mf, &init_name);
            out.times.closure = tclosure.elapsed();
            out.times.closure_work = mf.nested_fns.len() > pre_nested;
            for nf in &mf.nested_fns[pre_nested..] {
                cg.mint_synthetic(nf.name.to_string());
            }
            let owner = bone_semantic::hir::BodyOwner::Global(gid);
            let plan = concrete_plan.expect("a lowered global has a concrete plan");
            out.attribution.always_lowered = true;
            out.attribution
                .always_callee_keys
                .extend(plan.callees.values().cloned());
            let ctx = skull_mir::bound::concrete_body_ctx(
                ws,
                mid,
                owner,
                &plan.bound_tree,
                scx.interner(),
            );
            let tv = Instant::now();
            let check = check_unit_spec_bindings(
                ws,
                &ctx,
                mid,
                g.name.as_str(),
                &mf,
                g.name_span,
                &mut out.specs,
            );
            out.times.validation += tv.elapsed();
            out.times.validation_work = true;
            if let Err(e) = check {
                out.hard_error = Some(e);
                return out;
            }
            let key = Ustr::from(&init_name);
            skull_codegen_lua::regalloc_time_reset();
            let te = Instant::now();
            // A decorator / global-initializer body runs once at chunk load,
            // outside the definitions section the binding pass covers.
            let emission = emit_named_with_allocation_report(
                ws,
                cg,
                scx.interner(),
                &init_name,
                &mf,
                &[],
                None,
            );
            let mut lua = emission.lua;
            if let Some(body) = crate::allocation_report::collect_emitted_body(
                ws,
                scx.interner(),
                &init_name,
                g.name.as_str(),
                &mf,
                emission.allocation_sites,
            ) {
                out.allocation_bodies.push(body);
            }
            let root_emit = te.elapsed();
            out.times.emit += root_emit;
            out.times.emit_work |= root_emit > Duration::ZERO;
            let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(root_emit);
            out.times.regalloc += regalloc;
            out.times.regalloc_work |= regalloc > Duration::ZERO;
            if let Err(e) = emit_pass_nested_fns(
                ws,
                cg,
                scx,
                &mut mf.nested_fns,
                &mf.statics,
                Some(&mut out.allocation_bodies),
                &mut lua,
                &mut out.times,
            ) {
                out.hard_error = Some(e);
                return out;
            }
            emit_lowered(
                ws,
                &mut out,
                &mf,
                lua,
                &init_name,
                Some(key),
                Duration::ZERO,
            );
            out.attribution.times = out.times;
            // Hot-reload init line. A `const` is a compile-time constant: tweaking
            // the source literal should show the new value next reload, so its init
            // runs UNCONDITIONALLY (no preserve guard), mirroring rock-codegen's
            // `if is_const` branch (`incremental.rs::generate_global_code`).
            //
            // A mutable `global` holds live runtime state we WANT to survive the
            // re-dofile, so it keeps the preservation guard: on a reload
            // (`_G.HOT_RELOAD`), skip the assignment when this global existed with
            // the SAME type in the previous run. Cold start (HOT_RELOAD nil) always
            // runs the init. The type string is bone's render of `g.ty`, also
            // written into `__rock_known_globals` below (which lists const + mutable
            // alike); both come from the same render so "type unchanged" compares
            // equal.
            // Same TYPE is not same LAYOUT, and this guard deliberately does not
            // detect the difference: a layout drift restarts the process upstream
            // (`shape_drift_tests::marking_a_type_nobox_requests_restart`), and a
            // respawn is a cold start that never reaches this branch.
            let target = match global_init_target(ws, scx, cg, g) {
                Ok(target) => target,
                Err(e) => {
                    out.hard_error = Some(e);
                    return out;
                }
            };
            let line = if g.is_const {
                format!("{target} = {init_name}()\n")
            } else {
                match global_init_line(ws, scx, cg, g, &init_name) {
                    Ok(line) => line,
                    Err(error) => {
                        out.hard_error = Some(error);
                        return out;
                    }
                }
            };
            out.pending_init = Some((g.name, key, line));
        }
        Ok(None) => {}
        // A global initializer that fails to LOWER is a hard error, not a
        // lenient skip. `skipped` drops the init and says nothing (it prints only
        // under `SKULL_DEBUG_SKIPPED`), so the global silently becomes nil at
        // runtime -- and when the failure is a GUARANTEE rejection, downgrading
        // it is precisely what this struct's own contract forbids. A `nobox`
        // fn-value ABI rejection landed here and vanished.
        Err(e) => out.hard_error = Some(e),
    }
    out
}

/// Fold parallel `UnitOut`s back into the chunk IN ORDER: append each unit's Lua,
/// collect skips / call-graph facts / pending inits, sum phase times.
fn merge_units(
    results: Vec<UnitOut>,
    out: &mut String,
    skipped: &mut Vec<(String, String)>,
    direct_globals: &mut FxHashMap<Ustr, Vec<Ustr>>,
    direct_calls: &mut FxHashMap<Ustr, Vec<Ustr>>,
    pending_inits: &mut Vec<(Ustr, Ustr, String)>,
    calls: &mut Vec<FuncRef>,
    global_names: &mut Vec<Ustr>,
    code_artifact: &mut skull_mir::code_identity::CodeArtifact,
    allocation_bodies: &mut Vec<crate::allocation_report::AllocationBody>,
    attribution: &mut AttributionAccumulator,
    times: &mut PhaseTimes,
) {
    for u in results {
        out.push_str(&u.lua);
        if let Some(s) = u.skipped {
            skipped.push(s);
        }
        if let Some((k, gs, cs)) = u.refs {
            direct_globals.insert(k, gs);
            direct_calls.insert(k, cs);
        }
        if let Some(pi) = u.pending_init {
            pending_inits.push(pi);
        }
        calls.extend(u.calls);
        global_names.extend(u.global_names);
        code_artifact.merge(u.code_artifact);
        allocation_bodies.extend(u.allocation_bodies);
        attribution.add_unit(&u.attribution);
        times.add(&u.times);
    }
}

/// Whether `f` must be emitted no matter what the call graph says.
///
/// `@nomangle` is the load-bearing one. It is precisely the annotation for "this
/// fn is called by its ORIGINAL name from outside the MIR call graph" (CLAUDE.md:
/// from another `inline(lua)` body, from compiler codegen, or from itself), so
/// rooting all of them covers every invisible edge class at once -- including the
/// the string-keyed `on_loop_jump` dispatch site, whose targets appear only as
/// string literals. CodeRef-backed `@on_reload` values are rooted by their
/// decorator application. Measured cost on an application: 24 fns / 7.2 KB, 0.3% of the
/// chunk. Sound beats clever at that price.
///
/// The rest name a fn from generated top-level text rather than from a body:
/// `main` (the CLI appends `__rock_main_wrapper(main)`), a decorated fn (its
/// mangled name goes in a `decorator_application_lines` call), `@sim`
/// (`sim_registration_lines`), and `@test` (the harness calls it bare).
/// Whether a module is a LIBRARY -- something the compilation target consumes --
/// rather than part of the target itself.
///
/// The distinction is what makes whole-program DCE both safe and worthwhile. A
/// library is prunable: nothing outside it depends on a definition no root
/// reaches, and libraries are where the dead weight is (a game chunk is mostly
/// stdlib + engine). The target's OWN sources are roots, because compiling a unit
/// and getting its functions is a real contract here -- codegen tests compile a
/// fragment with no entry point and expect its fns, and `rock compile foo.rock`
/// on a file of helpers should not emit an empty program.
///
/// The stdlib is always a library. Beyond that the caller supplies the target's
/// root directory: a module whose file lives outside it came from a declared
/// dependency (a host library via stick.toml). `None` means a single-file compile, whose
/// only library is the stdlib.
fn is_library_module(file: &str, project_root: Option<&str>) -> bool {
    if is_stdlib_module_file(file) {
        return true;
    }
    match project_root {
        Some(root) => !std::path::Path::new(file).starts_with(root),
        None => false,
    }
}

pub(crate) fn is_emission_root(
    file: &str,
    project_root: Option<&str>,
    f: &bone_semantic::hir::Function,
    deco_map: &DecoMap,
) -> bool {
    if !is_library_module(file, project_root) {
        return true;
    }
    if f.name.as_str() == "main" {
        return true;
    }
    let has = |n: &str| f.annotations.iter().any(|a| a.is_builtin(n));
    if has("sim") || has("test") {
        return true;
    }
    is_global_emission_root(f, deco_map)
}

fn is_global_emission_root(f: &bone_semantic::hir::Function, deco_map: &DecoMap) -> bool {
    if f.annotations.iter().any(|a| a.is_builtin("nomangle")) {
        return true;
    }
    // `extern` decls bind a bare name an inline(lua) body may call directly.
    if f.is_extern {
        return true;
    }
    f.annotations.iter().any(|a| {
        let key = a.resolved_name.unwrap_or(a.name.name);
        deco_map.contains_key(&key)
    })
}

/// The diagnostic for a demand clash, at the declaration whose specialization
/// two sites disagree about.
fn demand_clash(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    body: &Instance,
) -> (String, CompilerError) {
    let fref = body.func;
    let f = &ws.module(fref.module).functions[fref.func];
    let name = body.symbol(ws, scx.interner());
    (
        name.clone(),
        CompilerError::new_with_label(
            f.name_span,
            format!(
                "internal: two call sites demand `{name}` with different `Iterator` \
                 bound resolutions, so one emitted definition cannot serve both"
            ),
        ),
    )
}

/// The spanned rejection for a specialization demand whose type arguments have
/// outgrown [`crate::instance_graph::MAX_INSTANTIATION_DEPTH`], or `None` when
/// it is within bounds.
///
/// This is the emission side of plan S 0h's `ChangingInstanceRecursion` call --
/// changing-instance recursion is REJECT, and the rejection has to come from
/// the INSTANCE, because E1 leaves no declaration body to reject from. The
/// depth test is the graph's: one `MonoTy::depth`, one constant, so "does this
/// instantiation terminate" has a single answer on both sides.
///
/// Depth is the right discriminant rather than a chain walk: a recursion whose
/// arguments do not change is same-instance recursion, which is legal and which
/// the walk's dedup already closes. Only a chain that grows its arguments
/// diverges, and growth is exactly what depth measures.
fn too_deep_to_instantiate(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    inst: &Instance,
) -> Option<(String, CompilerError)> {
    let args = inst.arg_types(scx.interner());
    let at = skull_mir::monoty::too_deep_to_instantiate(args.iter())?.render();
    let f = &ws.module(inst.func.module).functions[inst.func.func];
    let name = inst.symbol(ws, scx.interner());
    Some((
        name,
        CompilerError::new_with_label(
            f.name_span,
            format!(
                "polymorphic recursion: instantiating `{}` at `{at}` exceeds the \
             instantiation depth limit of {}. A recursive call whose type \
             arguments GROW never terminates -- each call needs a new body -- so \
             it is rejected rather than expanded. Same-instance recursion (type \
             arguments unchanged) is unaffected.",
                f.name,
                crate::instance_graph::MAX_INSTANTIATION_DEPTH,
            ),
        ),
    ))
}

fn inline_candidate_keys(concrete: &crate::concrete_body::ConcreteBodies) -> Vec<Instance> {
    concrete.candidate_keys().iter().cloned().collect()
}

fn concrete_stats_requested() -> bool {
    std::env::var_os("SKULL_CONCRETE_STATS").is_some()
}

fn build_inline_candidate_table(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    keys: Vec<Instance>,
    base: Option<&std::sync::Arc<skull_transform::InlineCandidateTable>>,
    invalidated: Option<&FxHashSet<FuncRef>>,
    warm_keys_override: Option<Vec<Instance>>,
    added: Option<&BTreeSet<Instance>>,
    removed: Option<&BTreeSet<Instance>>,
) -> Result<std::sync::Arc<skull_transform::InlineCandidateTable>, CompilerError> {
    let mut census = CandidateBuildStats::default();
    build_inline_candidate_table_with_census(
        ws,
        scx,
        concrete,
        keys,
        base,
        invalidated,
        warm_keys_override,
        added,
        removed,
        &mut census,
    )
}

#[derive(Clone, Copy, Debug, Default)]
struct CandidateBuildStats {
    keys_examined: usize,
    warmed: usize,
}

fn build_inline_candidate_table_with_census(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    keys: Vec<Instance>,
    base: Option<&std::sync::Arc<skull_transform::InlineCandidateTable>>,
    invalidated: Option<&FxHashSet<FuncRef>>,
    warm_keys_override: Option<Vec<Instance>>,
    added: Option<&BTreeSet<Instance>>,
    removed: Option<&BTreeSet<Instance>>,
    census: &mut CandidateBuildStats,
) -> Result<std::sync::Arc<skull_transform::InlineCandidateTable>, CompilerError> {
    // The old incremental route left this unset and filtered every key here.
    // Count the exact iterator population so a mutation that restores that
    // route is visible rather than looking like a timing-only regression.
    census.keys_examined = usize::from(warm_keys_override.is_none()) * keys.len();
    let warm_keys: Vec<Instance> = warm_keys_override.unwrap_or_else(|| {
        keys.iter()
            .filter(|key| match base {
                None => true,
                Some(base) => {
                    invalidated.is_some_and(|frefs| frefs.contains(&key.func))
                        || !base.contains(key)
                }
            })
            .cloned()
            .collect()
    });
    let warms: Vec<skull_transform::InlineCandidateWarm> = par_map_with(
        warm_keys,
        skull_transform::InlineCache::default,
        |inline_cache, inst| inline_cache.warm_candidate(ws, scx, inst, concrete.plans.as_ref()),
    )
    .into_iter()
    .collect::<Result<_, _>>()?;
    census.warmed = warms.len();
    if let Some(base) = base {
        if let (Some(added), Some(removed)) = (added, removed) {
            return Ok(std::sync::Arc::new(
                skull_transform::InlineCandidateTable::from_warms_incremental(
                    std::sync::Arc::clone(base),
                    warms,
                    &added.iter().cloned().collect::<Vec<_>>(),
                    &removed.iter().cloned().collect::<Vec<_>>(),
                ),
            ));
        }
    }
    Ok(std::sync::Arc::new(
        skull_transform::InlineCandidateTable::from_warms_with_base(
            base.map(std::sync::Arc::as_ref),
            keys,
            warms,
        ),
    ))
}

/// **Phase 2's specialized-body population, driven by SpecDemand closure over
/// `cache.lua_view`'s retained ordinary population -- not by `lua_view.
/// instances` directly, and not by a reachability walk.**
///
/// `lua_view` answers RETENTION (is this instance part of the program at
/// all), which is a different question from DELIVERY (does it need a
/// standalone Lua definition, or is every call to it spliced/inlined away at
/// the call site). Delivery is decided per call site during ordinary lowering
/// -- already done, once, in `emit_full`'s upfront parallel chunk build -- and
/// is exactly what a chunk's own `specs: Vec<SpecDemand>` records. A generic
/// instance the graph retains but nothing DEMANDS standalone (every caller
/// spliced it) has no chunk to build at all: `map_new<int, IVec2>` is such a
/// case in the shipped corpus (found empirically -- every real project
/// reaches it only through a global's `Map<int, IVec2> = map_new()`
/// initializer, which the column-storage inliner splices away entirely, so
/// building it standalone hits a REAL rejection -- `transmute` cannot build a
/// column-stored `Map` -- for an instance nothing ever calls that way).
///
/// `pending` seeds from the already-lowered chunk of every ordinary/coroutine
/// declaration `should_emit_via_view` retains, plus every already-collected
/// always-emitted (decorator/global) demand; each newly BUILT specialized
/// chunk's own `.specs` extends the worklist, mirroring
/// `emit_specialized_callees`'s render-time fixpoint but for BUILDING. Each
/// instance's tree is the concrete plan's resolved bound tree, which the graph
/// installs as part of that owner plan (never `DemandMap` -- sufficient per
/// `instance_tree_to_resolved`'s doc, corpus-verified byte-identical against
/// `DemandMap`-lowered text for every case both sides reach). A demand clash
/// (`cache.graph.has_bound_tree_disagreement`) rejects
/// that one instance rather than picking an arbitrary tree -- the
/// graph-driven equivalent of `DemandMap`'s clash check.
fn view_driven_specialized_chunks(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    cache: &SkullEmitCache,
    initial: &[skull_mir::bound::SpecDemand],
) -> (FxHashMap<Instance, Chunk>, Vec<(String, CompilerError)>) {
    let mut built: FxHashMap<Instance, Chunk> = FxHashMap::default();
    let mut errs: Vec<(String, CompilerError)> = Vec::new();
    let mut pending: Vec<Instance> = initial.iter().map(|d| d.inst.clone()).collect();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            let self_ref = FuncRef {
                module: module.id,
                func: fid,
            };
            if !is_cached_emitted_body(f, &shared(self_ref))
                || !should_emit_via_view(ws, &cache.lua_view, self_ref)
            {
                continue;
            }
            if let Some(Ok(c)) = cache.chunks.get(&shared(self_ref)) {
                pending.extend(c.specs.iter().map(|d| d.inst.clone()));
            }
        }
    }
    let mut done: FxHashSet<Instance> = FxHashSet::default();
    while !pending.is_empty() {
        let mut jobs = Vec::new();
        for inst in std::mem::take(&mut pending).into_iter().rev() {
            if !done.insert(inst.clone()) {
                continue;
            }
            // Polymorphic recursion (plan S 0h: REJECT). The graph's own build
            // silently CAPS a too-deep instance (`depth_capped`, no rejection --
            // it is a target-neutral fact-recorder, not a diagnostic surface), so
            // emission is where the spanned rejection has to live, same as before.
            if let Some((name, err)) = too_deep_to_instantiate(ws, scx, &inst) {
                errs.push((name, err));
                continue;
            }
            if cache.graph.has_bound_tree_disagreement(&inst) {
                errs.push(demand_clash(ws, scx, &inst));
                continue;
            }
            if let Some(Ok(c)) = cache.chunks.get(&inst) {
                pending.extend(c.specs.iter().map(|d| d.inst.clone()));
                built.insert(inst, c.clone());
                continue;
            }
            jobs.push(inst);
        }
        let results = par_map_with(
            jobs,
            || match cache.inline_cache.candidate_table() {
                Some(table) => skull_transform::InlineCache::with_candidate_table(table),
                None => skull_transform::InlineCache::default(),
            },
            |inline_cache, inst| {
                let name = inst.symbol(ws, scx.interner());
                let mut specs = Vec::new();
                let result = emit_spec_chunk(
                    ws,
                    cg,
                    scx,
                    &cache.concrete,
                    &inst,
                    &name,
                    inline_cache,
                    &mut specs,
                );
                (inst, name, result)
            },
        );
        for (inst, name, result) in results {
            match result {
                Ok(chunk) => {
                    pending.extend(chunk.specs.iter().map(|d| d.inst.clone()));
                    built.insert(inst, chunk);
                }
                Err(e) => errs.push((name, e)),
            }
        }
    }
    (built, errs)
}

/// Emit the whole workspace via the new (cell-model) path. `user_mid` is the
/// entry module (kept for signature parity with the old `emit_workspace`; the
/// new path is uniformly lenient across all modules today). Registered shaders
/// emit Core330 for desktop OpenGL.
pub fn emit_workspace_mir(ws: &Workspace, user_mid: ModuleId) -> Result<MirEmit, CompilerError> {
    emit_workspace_mir_in(ws, user_mid, None)
}

/// [`emit_workspace_mir`] for a PROJECT build: `project_root` is the target's
/// directory, so modules outside it (a stick.toml dependency such as a host library) are
/// libraries and prune down to what the project reaches. See [`is_library_module`].
pub fn emit_workspace_mir_in(
    ws: &Workspace,
    user_mid: ModuleId,
    project_root: Option<&str>,
) -> Result<MirEmit, CompilerError> {
    emit_workspace_mir_in_for_renderer(ws, user_mid, project_root, RendererTarget::Gl)
}

pub fn emit_workspace_mir_in_for_renderer(
    ws: &Workspace,
    user_mid: ModuleId,
    project_root: Option<&str>,
    renderer_target: RendererTarget,
) -> Result<MirEmit, CompilerError> {
    let _allocation_environment = crate::allocation_report::environment_scope();
    emit_full_in_with_mutation(
        ws,
        user_mid,
        project_root,
        BodyMutation::None,
        renderer_target,
        FullBuild::OneShotProgram,
        ShapeOverrides::default(),
    )
    .map(|(emit, _cache)| emit)
}

/// Sim receipt emission keeps the shared workspace but roots only selected
/// sims, their registry dependencies, main and the global roots.
pub fn emit_full_for_selected_sims(
    ws: &Workspace,
    user_mid: ModuleId,
    project_root: Option<&str>,
    renderer_target: RendererTarget,
    sims: &[String],
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    let _allocation_environment = crate::allocation_report::environment_scope();
    emit_full_in_with_mutation(
        ws,
        user_mid,
        project_root,
        BodyMutation::None,
        renderer_target,
        FullBuild::SelectedSims(sims),
        ShapeOverrides::default(),
    )
}

/// [`emit_workspace_mir`] for a package's `rock test` program, rooted at its
/// `tests`. See [`BuildKind::PackageTest`].
pub fn emit_workspace_mir_for_package_test(
    ws: &Workspace,
    user_mid: ModuleId,
    tests: &[FuncRef],
) -> Result<MirEmit, CompilerError> {
    let _allocation_environment = crate::allocation_report::environment_scope();
    emit_full_in_with_mutation(
        ws,
        user_mid,
        None,
        BodyMutation::None,
        RendererTarget::Gl,
        FullBuild::PackageTest(tests),
        ShapeOverrides::default(),
    )
    .map(|(emit, _cache)| emit)
}

/// One emitted ordinary/coroutine function definition plus the facts incremental
/// emit needs. The cheap definitions (prelude, decorators, externs, inline(lua),
/// global inits, init ordering, deco applications) are NOT cached — they are
/// regenerated on every [`assemble`].
#[derive(Clone)]
struct Chunk {
    lua: String,
    /// Includes hoisted closures and spliced bodies, unlike init-ordering refs.
    literal_globals: Vec<Ustr>,
    allocation_bodies: Vec<crate::allocation_report::AllocationBody>,
    /// Source and eligibility dependencies, including residual callees.
    deps: FxHashSet<FuncRef>,
    /// Bodies embedded in this chunk, including transitive and hoisted splices.
    /// These always invalidate on a body edit; residual edges can instead use
    /// the old/new inline candidate's eligibility facts.
    embedded_deps: FxHashSet<FuncRef>,
    /// Body-derived transport is independent of inline candidate eligibility.
    abi_deps: FxHashSet<FuncRef>,
    /// Global-init ordering facts (globals read + functions called).
    direct_globals: Vec<Ustr>,
    direct_calls: Vec<Ustr>,
    /// Per-stage emit time captured when this chunk was built, summed back into
    /// the `backend-stats` breakdown by [`assemble`] when the chunk is pulled.
    times: PhaseTimes,
    attribution: UnitAttribution,
    /// `static` cells this body declares -- aggregated at chunk-pull for the
    /// hot-reload preservation preamble + `__rock_known_globals` registration.
    statics: Vec<skull_mir::mir::StaticCell>,
    /// Comptime inputs baked into this chunk's Lua (build_var/embed_string).
    /// `update_chunk_cache` compares them against the current inputs and treats
    /// a stale read as a body edit -- bone's reinferred set cannot see these.
    comptime_reads: Vec<skull_mir::mir::ComptimeRead>,
    /// The callees this body still CALLS after inlining -- the reachability edge
    /// set. Distinct from `deps`, which is an invalidation superset (it also holds
    /// everything inlined AWAY, and would keep spliced-out callees alive).
    calls: Vec<FuncRef>,
    /// Callees this body invokes through a SPECIALIZED instantiation that no
    /// splice served, so each needs a standalone specialized copy emitted.
    /// Deduplicated program-wide by [`assemble`] -- many bodies ask for the same
    /// one, and it must be defined exactly once.
    specs: Vec<skull_mir::bound::SpecDemand>,
    /// Compiler-owned logical callable identities in this emitted body.
    code_artifact: skull_mir::code_identity::CodeArtifact,
    /// Under `CalleeBindingMode::Upvalue`, the definitions in this chunk that
    /// were emitted as factories, each with the second-pass line that binds it.
    /// Empty under the global-lookup shape. [`assemble`] emits these in
    /// ascending `rank` after EVERY definition, so each captured callee is
    /// already its final closure.
    binds: Vec<skull_codegen_lua::CalleeBindingPlan>,
}

// Each global definition costs the enclosing LuaJIT proto a child proto and name.
const LUA_DEFINITIONS_PER_SCOPE: usize = 4_096;

// Every global a statement names is a constant of the enclosing LuaJIT proto,
// whose constant index is 16 bits. The binding pass is nothing BUT global
// names, so it is split by name count rather than by statement count.
const LUA_NAMES_PER_BIND_SCOPE: usize = 16_384;

/// The callee binding's SECOND PASS: one `<name> = __rock_mk_<name>(<callees>)`
/// line per factory, emitted after every definition so a captured callee's
/// global already holds its final closure.
///
/// Ordered by call-graph component rank, which [`skull_codegen_lua::call_sccs`]
/// numbers in Tarjan completion order: a captured callee's component is
/// strictly lower than its caller's (they are never equal -- a same-component
/// callee stays a global lookup), so ascending rank binds callees first. Ties
/// break on name so the emitted text is deterministic.
fn callee_binding_pass_lua(plans: &mut [skull_codegen_lua::CalleeBindingPlan]) -> String {
    if plans.is_empty() {
        return String::new();
    }
    plans.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.name.cmp(&b.name)));
    let mut out = String::new();
    let mut names_in_scope = 0usize;
    for plan in plans.iter() {
        if names_in_scope == 0 {
            out.push_str("do\n(function()\n");
        }
        out.push_str(&plan.bind_line());
        names_in_scope += plan.captures.len() + 2;
        if names_in_scope >= LUA_NAMES_PER_BIND_SCOPE {
            out.push_str("end)()\nend\n");
            names_in_scope = 0;
        }
    }
    if names_in_scope > 0 {
        out.push_str("end)()\nend\n");
    }
    if std::env::var("SKULL_EMIT_PROFILE").is_ok() {
        let capped = plans.iter().filter(|p| p.capped).count();
        let captures: usize = plans.iter().map(|p| p.captures.len()).sum();
        eprintln!(
            "    callee binding: {} bound definitions, {} captures ({:.1} mean), {} capped at {}",
            plans.len(),
            captures,
            captures as f64 / plans.len() as f64,
            capped,
            skull_codegen_lua::MAX_BOUND_CALLEES,
        );
    }
    out
}

struct LuaDefinitionBuffer {
    lua: String,
    units: Vec<(usize, usize, usize)>,
    definition_count: usize,
}

/// The global a top-level definition assigns, in either emitted shape.
/// Definition splitting consumes this inventory.
fn definition_global(line: &str) -> Option<&str> {
    let body = skull_codegen_lua::emitted_body_name(line)?;
    if line.starts_with("function ") {
        return Some(body);
    }
    Some(&line[..skull_codegen_lua::CALLEE_FACTORY_PREFIX.len() + body.len()])
}

impl LuaDefinitionBuffer {
    fn new() -> Self {
        Self {
            lua: String::new(),
            units: Vec::new(),
            definition_count: 0,
        }
    }

    fn push_str(&mut self, lua: &str) {
        let start = self.lua.len();
        let count = lua.lines().filter_map(definition_global).count();
        self.lua.push_str(lua);
        self.units.push((start, self.lua.len(), count));
        self.definition_count += count;
    }

    fn append_to(self, out: &mut String) {
        if self.definition_count <= LUA_DEFINITIONS_PER_SCOPE {
            out.push_str(&self.lua);
            return;
        }

        out.push_str("do\n(function()\n");
        let mut definitions_in_scope = 0;
        for (start, end, count) in self.units {
            if definitions_in_scope > 0 && definitions_in_scope + count > LUA_DEFINITIONS_PER_SCOPE
            {
                out.push_str("end)()\nend\ndo\n(function()\n");
                definitions_in_scope = 0;
            }
            out.push_str(&self.lua[start..end]);
            definitions_in_scope += count;
        }
        out.push_str("end)()\nend\n");
    }
}

#[derive(Clone, Copy)]
enum BodyMutation {
    None,
    #[cfg(test)]
    RemoveCalleeOwner {
        caller: FuncRef,
        callee: FuncRef,
    },
    #[cfg(test)]
    ClearCalleeOwnerKey {
        caller: FuncRef,
        callee: FuncRef,
    },
    #[cfg(test)]
    ClearBoundPlan {
        body: FuncRef,
    },
    #[cfg(test)]
    ClearCoroutineResultPlan {
        body: FuncRef,
    },
    #[cfg(test)]
    ClearIteratorWitnessInstance {
        body: FuncRef,
    },
    #[cfg(test)]
    SkipAbiPlanPublish,
    #[cfg(test)]
    ConcreteResultOrder(skull_graph::concrete_body::BuildResultOrder),
    #[cfg(all(test, not(feature = "sequential")))]
    SetupLaneSchedule(SetupLaneSchedule),
}

#[cfg(all(test, not(feature = "sequential")))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupLaneSchedule {
    CpuFirst,
    AbiFirst,
    Handshake,
}

#[cfg(all(test, not(feature = "sequential")))]
#[derive(Clone, Copy, Debug, Default)]
struct SetupHandshakeState {
    entered: usize,
    released: usize,
    released_after_both: bool,
}

#[cfg(all(test, not(feature = "sequential")))]
struct SetupLaneScheduler {
    schedule: SetupLaneSchedule,
    finished: std::sync::Mutex<bool>,
    handshake: std::sync::Mutex<SetupHandshakeState>,
    wake: std::sync::Condvar,
}

#[cfg(all(test, not(feature = "sequential")))]
impl SetupLaneScheduler {
    fn new(schedule: SetupLaneSchedule) -> Self {
        Self {
            schedule,
            finished: std::sync::Mutex::new(false),
            handshake: std::sync::Mutex::new(SetupHandshakeState::default()),
            wake: std::sync::Condvar::new(),
        }
    }

    fn wait(&self, lane: SetupLaneKind) {
        if self.schedule == SetupLaneSchedule::Handshake {
            if !matches!(lane, SetupLaneKind::CpuArtifact | SetupLaneKind::Shader) {
                return;
            }
            let mut handshake = self.handshake.lock().expect("setup handshake lock");
            handshake.entered += 1;
            if handshake.entered == 2 {
                handshake.released_after_both = true;
                self.wake.notify_all();
            }
            while handshake.entered < 2 {
                handshake = self.wake.wait(handshake).expect("setup handshake wait");
            }
            handshake.released += 1;
            return;
        }
        let first = match self.schedule {
            SetupLaneSchedule::CpuFirst => SetupLaneKind::CpuArtifact,
            SetupLaneSchedule::AbiFirst => SetupLaneKind::AbiWarm,
            SetupLaneSchedule::Handshake => unreachable!(),
        };
        if lane != first {
            let mut finished = self.finished.lock().expect("setup scheduler lock");
            while !*finished {
                finished = self.wake.wait(finished).expect("setup scheduler wait");
            }
        }
    }

    fn finish(&self, lane: SetupLaneKind) {
        let first = match self.schedule {
            SetupLaneSchedule::CpuFirst => SetupLaneKind::CpuArtifact,
            SetupLaneSchedule::AbiFirst => SetupLaneKind::AbiWarm,
            SetupLaneSchedule::Handshake => return,
        };
        if lane != first {
            return;
        }
        *self.finished.lock().expect("setup scheduler lock") = true;
        self.wake.notify_all();
    }

    fn handshake(&self) -> (usize, usize, bool) {
        let handshake = *self.handshake.lock().expect("setup handshake lock");
        (
            handshake.entered,
            handshake.released,
            handshake.released_after_both,
        )
    }
}

/// The shared body of `fref` -- the id under which a declaration's own
/// definition is cached. Written out at every chunk lookup so the day a
/// SPECIALIZED body answers one of them is a visible change rather than a
/// silent widening.
fn shared(fref: FuncRef) -> Instance {
    Instance::base(fref)
}

/// Whether `self_ref`'s OWN ordinary/coroutine chunk must be emitted, per
/// `view` -- the exact predicate a population-source cutover would gate the
/// module-fn loop's `emit_def` with. TWO independent reasons, matching this
/// session's author decision:
///
/// - **call-serving**: `self_ref` is the declaration `realize_decl` SELECTED
///   to serve calls to its canonical `Instance` (which may be a DIFFERENT
///   `FuncRef` than `self_ref` when `self_ref` is a non-representative
///   sibling, and vice versa when `self_ref` IS the representative but a
///   same-name sibling serves calls instead).
/// - **publication**: `self_ref` carries an explicit `@nomangle` root whose
///   own bare-name promise the artifact must publish regardless of whether
///   any resolved call reaches it that way.
///
/// A declaration can be neither (pruned), either, or -- the whole point of
/// modeling publication separately -- BOTH under two different symbols.
fn should_emit_via_view(
    ws: &Workspace,
    view: &crate::artifact::ArtifactView,
    self_ref: FuncRef,
) -> bool {
    let canonical = skull_mir::lower::canonical_variant(ws, self_ref);
    let call_serving = view
        .realization
        .get(&Instance::base(canonical))
        .is_some_and(|r| r.decl == self_ref);
    let publication = view.publications.contains_key(&Instance::base(self_ref));
    call_serving || publication
}

/// Whether a FOREIGN (`inline(lua)`) body needs a standalone definition.
///
/// Retention alone over-emits here: a `@pure @inline` stdlib operator is
/// retained (something calls `+`) and spliced away at every call site, so the
/// question is delivery, not membership. Four independent reasons force a
/// definition -- a bare-name publication promise, being a DECLARED root in its
/// own right (an own-project `inline(lua)` fn is rooted with no caller at all,
/// same reasoning as `main`/`@sim`/`@test`), a residual non-spliced call, or a
/// FUNCTION-VALUE take, which is not a call and so has nothing to splice into:
/// the program stores the definition itself under its own symbol.
///
/// `declared_roots`, not `roots`: the latter also carries the graph's own
/// "callee of an always-emitted body, no incoming edge" category, which is an
/// ordinary reached instance subject to the same splicing question (found
/// empirically -- `__op_mul`/`__op_div`, reached from a global initializer's
/// constant expression, are not declared roots and are spliced away
/// everywhere). A generic declaration has no base instance and so is never in
/// the set.
///
/// ONE oracle: the definition loop and the emitted-body census both call it, so
/// they cannot disagree about what the assembly contains.
fn emits_foreign_definition(
    ws: &Workspace,
    view: &crate::artifact::ArtifactView,
    needed_foreign: &FxHashSet<Instance>,
    self_ref: FuncRef,
) -> bool {
    let canonical = shared(skull_mir::lower::canonical_variant(ws, self_ref));
    view.publications.contains_key(&shared(self_ref))
        || view.declared_roots.contains(&canonical)
        || view.fn_value_targets.contains(&canonical)
        || needed_foreign.contains(&shared(self_ref))
}

/// Deterministic overlay order over emitted bodies. Reduces to the declaration
/// order the overlay used before specializations could appear in it: a
/// declaration's own body sorts ahead of its specializations, and those sort by
/// their instantiation's canonical spelling, which is injective.
fn overlay_order_key(
    scx: &skull_mir::lower::ScalarCtx,
    body: &Instance,
) -> (usize, u32, Option<String>) {
    let fref = body.func;
    // The instantiation's canonical spelling, which is injective -- a
    // declaration's own body sorts ahead of its specializations because it has
    // no arguments to spell.
    let key = (!body.args.is_empty()).then(|| {
        body.arg_types(scx.interner())
            .iter()
            .map(skull_mir::monoty::MonoTy::canonical)
            .collect::<Vec<_>>()
            .join(",")
    });
    (fref.module.index(), u32::from(fref.func.into_raw()), key)
}

/// Re-emit ONE emitted body's chunk. The shared body is the declaration's own;
/// a specialized one lowers under its instantiation. Both go through the same
/// invalidation, which is the point of keying the cache on the body rather than
/// the declaration.
fn rebuild_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    body: &Instance,
    f: &bone_semantic::hir::Function,
    inline_cache: &mut skull_transform::InlineCache,
) -> Result<Chunk, CompilerError> {
    match body.args.is_empty() {
        true => emit_body_chunk(ws, cg, scx, concrete, body.func, f, inline_cache),
        false => {
            // A rebuild that cannot emit DROPS the entry rather than storing the
            // rejection: assembly rebuilds a miss and reports the error from
            // there, which is the one place a specialized rejection is ordered
            // against its siblings.
            let name = body.symbol(ws, scx.interner());
            let mut specs = Vec::new();
            emit_spec_chunk(ws, cg, scx, concrete, body, &name, inline_cache, &mut specs)
        }
    }
}

/// A `static` cell as the last full build left it: its reload key and storage keys.
#[derive(Clone)]
struct StaticBaseline {
    key: String,
    value_slot: String,
    done_slot: String,
}

/// Per-body emit cache for incremental codegen: the expensive function chunks
/// (ordinary + coroutine bodies), keyed by [`Instance`]. `Err` records a body the
/// new path could not lower (the parity gap). Everything else is regenerated by
/// [`assemble`] every time.
pub struct SkullEmitCache {
    chunks: FxHashMap<Instance, Result<Chunk, CompilerError>>,
    concrete: crate::concrete_body::ConcreteBodies,
    /// **The authoritative instance graph, built EAGERLY by `emit_full_in`
    /// before any chunk is lowered, and retained across body edits.**
    ///
    /// `emit_full_in` mints the arena, builds this graph into it FIRST, and
    /// hands the same `Arc<MonoInterner>` to `scx` (`ScalarCtx::sharing`) --
    /// that order is load-bearing, because an `Instance` the graph minted has
    /// to mean something to lowering. There is no lazy/first-edit path: a
    /// one-shot `rock compile` pays the build unconditionally, which is what
    /// makes `lua_view` (below) a real production fact rather than a shadow
    /// computed beside emission.
    ///
    /// Maintained in place by `update_bodies` on every edit -- never rebuilt,
    /// which would invalidate every `Instance` key in this cache.
    graph: crate::instance_graph::InstanceGraph,
    /// The Lua artifact's realization of `graph`, from the SAME roots
    /// `emit_full_in` builds the graph from. Recomputed by every full/edit
    /// cycle that touches the graph, so it never drifts from what emission
    /// actually discovers.
    lua_view: crate::artifact::ArtifactView,
    /// Functions changed since the last full build (`emit_full`), accumulated by
    /// `emit_overlay` so the `incremental.lua` overlay always carries the latest
    /// version of every touched fn (not just the most recent edit). Reset to
    /// empty by `emit_full`.
    changed_since_full: FxHashSet<Instance>,
    /// Workspace overload set (drives mangling). Derived from function SIGNATURES,
    /// so it is byte-identical across every body edit and only changes on an
    /// interface/structural edit -- which routes to `emit_full` (a fresh cache).
    /// Built once here, reused by the body-edit paths (`emit_overlay` /
    /// `emit_incremental`) instead of an O(program) rescan per edit.
    cg: CodegenCtx,
    /// Scalar-ABI context (the set of fns taken by reference). Maintained
    /// incrementally per body edit (`ScalarCtx::update_body`): the edited body's
    /// value-reference contribution is diffed into the cached set, and any name
    /// whose taken-by-ref membership FLIPS drives re-emit of that fn + its callers
    /// (their scalar ABI / call-site shape changed). Rebuilt by `emit_full`.
    scx: skull_mir::lower::ScalarCtx,
    /// Callee-eligibility/lowered-body cache, persisted across body edits: an
    /// unchanged callee keeps its cached lowered body instead of re-lowering on
    /// every edit (the dominant re-lower cost). `update_chunk_cache` invalidates
    /// the affected entries each edit (stale bodies are a subset of affected).
    /// Starts empty after `emit_full` (whose per-worker caches are discarded) and
    /// warms lazily as edits touch callees.
    inline_cache: skull_transform::InlineCache,
    /// Candidate identities currently demanded by concrete owner plans. The
    /// concrete publisher updates this set by owner delta; it is not rebuilt
    /// by scanning every plan on a body edit.
    /// Reverse dependency index: `dependents[C]` = the chunks that have `C` in
    /// their codegen deps (a body inlined `C` in, or calls `C`). Lets
    /// `update_chunk_cache` compute the affected set with one reverse hop per
    /// changed fn (O(edit x fan-out)) instead of scanning every chunk's deps.
    /// Built in `emit_full`, maintained as chunks re-emit. Sound on the body-edit
    /// path because the chunk key set is invariant between full builds.
    dependents: FxHashMap<FuncRef, FxHashSet<Instance>>,
    /// `const` globals changed since the last full build (`emit_full`), accumulated
    /// by `emit_overlay`. The overlay re-emits each one's init UNGUARDED so the new
    /// literal takes effect on reload (mutable `global`s are NEVER here -- their
    /// guard preserves the live value, and their value edits stay interface-tier).
    /// Reset to empty by `emit_full`.
    changed_consts_since_full: FxHashSet<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)>,
    /// Baseline `static` cells from the last full build, keyed by cell name.
    /// `assemble_overlay` compares each changed fn's current cell key
    /// (`type@init_hash`) against this and emits `__rock_static_prepare` for any
    /// cell whose key changed, so an initializer edit re-inits on the next
    /// reach; an unchanged key is left alone (an unrelated body edit preserves
    /// the live value). A baseline cell no current chunk holds was removed and
    /// is dropped. Rebuilt by `emit_full`.
    static_key_baseline: FxHashMap<Ustr, StaticBaseline>,
    roots: RootCache,
    shader_registrations: ShaderRegistrations,
    shader_view: crate::shader_artifact::ShaderArtifactView,
    /// The definitions the last assembly actually EMITTED (global DCE prunes
    /// the rest), keyed by EMITTED BODY. Graph-delta materialization uses it to
    /// distinguish a cached body that is newly retained from one already present
    /// in the live program.
    ///
    /// By emitted body rather than by declaration, because a specialization is
    /// its own definition with its own symbol.
    emitted: FxHashSet<Instance>,
    /// Logical callable table from the last complete assembly. Body overlays
    /// preserve this identity table; a full/incremental assembly refreshes it.
    code_artifact: std::sync::Arc<skull_mir::code_identity::CodeArtifact>,
    /// Lifecycle metadata for the published artifact. Body overlays replace
    /// fingerprints while structural changes take the full transition path.
    lifecycle_plan: Option<std::sync::Arc<skull_mir::lifecycle::LifecyclePlan>>,
    /// Global layout from the last complete assembly. A body overlay cannot
    /// change it; a full/incremental assembly replaces it.
    global_layout: Option<String>,
    /// Definition inventory from the last complete assembly. Body overlays
    /// retain it because definition structure is a full-reload boundary.
    definition_plan: Option<std::sync::Arc<skull_mir::definition::DefinitionPlan>>,
    /// Exact identity of the Artifact currently staged by this cache.
    artifact_fingerprint: Option<String>,
    fingerprint_code_rows: Option<crate::artifact_fingerprint::CodeRows>,
    /// Comptime reads of the always-emitted units (global initializers,
    /// decorator defs), which have no Chunk. Feeds the watch set
    /// (`comptime_file_hashes`) and the overlay's stale-const re-init; refreshed by
    /// every `assemble` (full and incremental reassembly re-lower all units).
    unit_comptime_reads: UnitComptimeReads,
    renderer_target: RendererTarget,
    build_kind: BuildKind,
    selected_sim_functions: Option<FxHashSet<Ustr>>,
    repair_stats: IncrementalRepairStats,
    body_edit_rollback: Option<BodyEditRollback>,
}

/// Structural work performed by the last body-only cache update. The counters
/// are ordinary per-cache state; they do not use synchronization or process-
/// global atomics. The route records only work it already performs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IncrementalRepairStats {
    pub body_only_edits: usize,
    pub owners_revisited: usize,
    pub instances_revisited: usize,
    pub graph_contributions_retracted: usize,
    pub graph_contributions_installed: usize,
    pub full_graph_rebuilds: usize,
    pub concrete_entries_derived: usize,
    pub concrete_entries_reused: usize,
    pub concrete_entries_retracted: usize,
    pub full_concrete_rebuilds: usize,
    pub artifact_chunks_revisited: usize,
    pub artifact_chunks_added: usize,
    pub artifact_chunks_removed: usize,
    pub full_artifact_view_realizations: usize,
    pub broad_owner_scans: usize,
    pub broad_concrete_scans: usize,
    pub broad_artifact_scans: usize,
    pub broad_candidate_scans: usize,
    pub root_requests: usize,
    pub root_modules_examined: usize,
    pub root_functions_examined: usize,
    pub root_reseeds: usize,
    pub root_instances_checked: usize,
    pub shader_delta_requests: usize,
    pub shader_roots_examined: usize,
    pub shader_roots_derived: usize,
    pub declaration_instance_lookups: usize,
    pub declaration_lookup_requests: usize,
    pub candidate_entries_invalidated: usize,
    pub candidate_entries_derived: usize,
    pub candidate_entries_reused: usize,
    pub candidate_entries_requested: usize,
    pub dependents_visited: usize,
    pub dependent_requests: usize,
    pub chunks_rebuilt: usize,
    pub chunk_rebuild_requests: usize,
    pub artifact_iterations: usize,
    pub artifact_iteration_requests: usize,
    pub artifact_root_entries_examined: usize,
    pub artifact_index_rows_examined: usize,
    pub artifact_scalar_context_builds: usize,
    pub artifact_realize_invocations: usize,
    pub root_hole_names_examined: usize,
    pub root_hole_rows_examined: usize,
    pub root_hole_decls_examined: usize,
    pub root_hole_reason_buckets_examined: usize,
    pub erased_catalogue_entries_examined: usize,
    pub graph_update_ns: u64,
    pub concrete_update_ns: u64,
    pub artifact_update_ns: u64,
    pub candidate_update_ns: u64,
    pub affected_selection_ns: u64,
    pub relowering_ns: u64,
    pub code_artifact_assembly_reused: bool,
    pub fingerprint_code_rows_derived: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererTarget {
    Gl,
    Wgpu,
}

/// What an emitted program is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildKind {
    /// A game, tool or single-file program; it may be reloaded in place.
    Program,
    /// A package's `rock test` program. Each test child loads it once and never
    /// reloads it. It runs no sims, so it registers none.
    PackageTest,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheLifetime {
    Retained,
    OneShot,
}

/// A full build's kind with the roots it is taken over.
#[derive(Clone, Copy)]
enum FullBuild<'a> {
    Program,
    OneShotProgram,
    PackageTest(&'a [FuncRef]),
    SelectedSims(&'a [String]),
}

impl FullBuild<'_> {
    fn lifetime(self) -> CacheLifetime {
        match self {
            Self::Program | Self::SelectedSims(_) => CacheLifetime::Retained,
            Self::OneShotProgram | Self::PackageTest(_) => CacheLifetime::OneShot,
        }
    }

    fn kind(self) -> BuildKind {
        match self {
            FullBuild::Program | FullBuild::OneShotProgram | FullBuild::SelectedSims(_) => {
                BuildKind::Program
            }
            FullBuild::PackageTest(_) => BuildKind::PackageTest,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WgpuProjectionCensus {
    pub examined: usize,
    pub admitted: usize,
    pub omitted: usize,
    pub omissions: BTreeMap<String, String>,
}

#[cfg(feature = "renderer-wgpu")]
enum WgpuProjection<T> {
    Admitted(T),
    Unsupported { capability: String },
}

#[cfg(feature = "renderer-wgpu")]
fn classify_wgpu_projection<T>(
    result: Result<T, CompilerError>,
) -> Result<WgpuProjection<T>, CompilerError> {
    match result {
        Ok(value) => Ok(WgpuProjection::Admitted(value)),
        Err(error) => match error.kind {
            CompilerErrorKind::UnsupportedWgslProjection { capability } => {
                Ok(WgpuProjection::Unsupported { capability })
            }
            _ => Err(error),
        },
    }
}

#[cfg(feature = "renderer-wgpu")]
fn collect_wgpu_projections<T, K>(
    items: impl IntoIterator<Item = (String, K)>,
    mut project: impl FnMut(&str, K) -> Result<T, CompilerError>,
) -> Result<(Vec<(String, T)>, WgpuProjectionCensus), CompilerError> {
    let mut census = WgpuProjectionCensus::default();
    let mut admitted = Vec::new();
    for (key, input) in items {
        census.examined += 1;
        match classify_wgpu_projection(project(&key, input))? {
            WgpuProjection::Admitted(value) => {
                census.admitted += 1;
                admitted.push((key, value));
            }
            WgpuProjection::Unsupported { capability } => {
                census.omitted += 1;
                census.omissions.insert(key, capability);
            }
        }
    }
    debug_assert_eq!(census.examined, census.admitted + census.omitted);
    Ok((admitted, census))
}

#[derive(Default, Clone)]
struct ShaderRegistrations {
    lua: String,
    sources: std::sync::Arc<[ShaderSource]>,
    dependencies: FxHashSet<FuncRef>,
    callsites: FxHashSet<(ModuleId, BodyOwner)>,
    shader_contributions: HashMap<(ModuleId, BodyOwner), Vec<skull_mir::lower::ShaderPair>>,
    compute_contributions: HashMap<(ModuleId, BodyOwner), Vec<skull_mir::lower::ComputeEntry>>,
    shader_pair_users: BTreeMap<String, usize>,
    shader_pairs: BTreeMap<String, skull_mir::lower::ShaderPair>,
    compute_entry_users: BTreeMap<String, usize>,
    compute_entries: BTreeMap<String, skull_mir::lower::ComputeEntry>,
    shader_root_set: FxHashSet<FuncRef>,
    roots: Vec<FuncRef>,
    keys: BTreeSet<String>,
    abi_wires: BTreeMap<String, String>,
    wgpu_projection_census: Option<WgpuProjectionCensus>,
}

#[derive(Clone, Default)]
struct RootCache {
    project_root: Option<String>,
    interface_roots: Vec<FuncRef>,
    sim_roots: Vec<FuncRef>,
    shader_roots: Vec<FuncRef>,
    roots: Vec<FuncRef>,
}

struct BodyEditRollback {
    reinferred: Vec<(ModuleId, BodyOwner)>,
    edits: Vec<crate::instance_graph::SpanEdit>,
    changed_since_full: FxHashSet<Instance>,
    changed_consts_since_full: FxHashSet<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)>,
    static_key_baseline: FxHashMap<Ustr, StaticBaseline>,
    emitted: FxHashSet<Instance>,
    code_artifact: std::sync::Arc<skull_mir::code_identity::CodeArtifact>,
    lifecycle_plan: Option<std::sync::Arc<skull_mir::lifecycle::LifecyclePlan>>,
    global_layout: Option<String>,
    definition_plan: Option<std::sync::Arc<skull_mir::definition::DefinitionPlan>>,
    artifact_fingerprint: Option<String>,
    fingerprint_code_rows: Option<crate::artifact_fingerprint::CodeRows>,
    unit_comptime_reads: UnitComptimeReads,
    repair_stats: IncrementalRepairStats,
}

/// A `static` cell's reload key: rendered type + initializer content hash. Equal
/// keys mean "same meaning, preserve the live cell"; a changed key triggers a
/// reset + re-init. Shared by `assemble` and `assemble_overlay` so complete and
/// incremental publication compare initializer meaning identically.
fn static_cell_key(sc: &skull_mir::mir::StaticCell) -> String {
    format!("{}@{:x}", sc.type_str, sc.init_hash)
}

/// The statically-resolved callees referenced by a (post-inline) MIR function —
/// the callees still CALLED after inlining (their bodies could flip eligibility
/// on a future edit). Unioned with the driver's spliced set to form the chunk's
/// codegen deps.
fn collect_callee_frefs(mf: &MirFunc, out: &mut FxHashSet<FuncRef>) {
    for (_, inst) in mf.insts.iter() {
        match inst {
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } => {
                out.insert(*func);
            }
            _ => {}
        }
    }
}

fn collect_abi_dependencies(mf: &MirFunc, out: &mut FxHashSet<FuncRef>) {
    out.extend(
        mf.call_abi_plans
            .values()
            .filter(|plan| plan.body_dependent_parameters())
            .map(|plan| plan.subject.func),
    );
    for nested in &mf.nested_fns {
        collect_abi_dependencies(nested, out);
    }
}

fn emit_function_with_allocation_report(
    ws: &Workspace,
    cg: &CodegenCtx,
    interner: &skull_mir::monoty::MonoInterner,
    self_ref: FuncRef,
    mf: &MirFunc,
    bound: Option<&skull_codegen_lua::CalleeBindingPlan>,
) -> skull_codegen_lua::LuaEmission {
    if crate::allocation_report::enabled() {
        skull_codegen_lua::emit_function_with_allocation_sites(
            ws,
            cg,
            interner,
            self_ref,
            mf,
            bound,
            crate::allocation_report::activation_enabled(),
        )
    } else {
        skull_codegen_lua::LuaEmission {
            lua: emit_function(ws, cg, interner, self_ref, mf, bound),
            allocation_sites: Vec::new(),
        }
    }
}

fn emit_named_with_allocation_report(
    ws: &Workspace,
    cg: &CodegenCtx,
    interner: &skull_mir::monoty::MonoInterner,
    name: &str,
    mf: &MirFunc,
    inherited_statics: &[skull_mir::mir::StaticCell],
    bound: Option<&skull_codegen_lua::CalleeBindingPlan>,
) -> skull_codegen_lua::LuaEmission {
    if crate::allocation_report::enabled() {
        skull_codegen_lua::emit_named_framed_with_allocation_sites(
            ws,
            cg,
            interner,
            name,
            mf,
            None,
            inherited_statics,
            bound,
            crate::allocation_report::activation_enabled(),
        )
    } else {
        skull_codegen_lua::LuaEmission {
            lua: skull_codegen_lua::emit_named_with_statics(
                ws,
                cg,
                interner,
                name,
                mf,
                inherited_statics,
                bound,
            ),
            allocation_sites: Vec::new(),
        }
    }
}

/// **The one Lua emission of a `co fn`**, shared by the declaration's own body
/// and by each specialized instantiation of a generic one.
///
/// Coroutines are not inlined into and do not inline: the call constructs a
/// coroutine object rather than running the body, which is why specialization
/// here is delivered by a standalone definition instead of a splice. That is the
/// whole difference from an ordinary specialization, and it is why this is not
/// [`prepare_instance_body_with_plans`] -- there is no inline pass, no `convert_closures`
/// (the step's lambdas convert framed, inside `emit_coroutine`) and no FORL
/// recovery (codegen's relooper does it).
///
/// The concrete body plan decides the instance symbol and yield channel.
fn emit_coroutine_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    ctx: &skull_mir::bound::BoundCtx,
    concrete_plan: &skull_mir::concrete::ConcreteBodyPlan,
    f: &bone_semantic::hir::Function,
    mf: &mut MirFunc,
    mut times: PhaseTimes,
) -> Result<Chunk, CompilerError> {
    let cleanup_before = mir_live_instructions(mf);
    let tc = Instant::now();
    skull_transform::cleanup_with_nobox(ws, mf)?;
    times.cleanup = tc.elapsed();
    times.cleanup_work = mir_live_instructions(mf) != cleanup_before;
    // Coroutines never inline, so a combinator chain here leaves a residual call
    // -- fine when the lowerer specialized it (a standalone body serves it), a
    // rejection when it did not.
    let tv = Instant::now();
    skull_transform::validate_no_residual_bound_calls(ws, f, mf)?;
    let mut specs = Vec::new();
    validate_emitted_body(ws, ctx, mf, &mut specs).map_err(|r| r.into_error(f.name_span, ""))?;
    times.validation += tv.elapsed();
    times.validation_work = true;
    skull_codegen_lua::regalloc_time_reset();
    let te = Instant::now();
    let inst = concrete_plan
        .function_instance()
        .expect("a coroutine emission plan must own a function instance");
    let plan = skull_mir::lower::CoroutinePlan::from_concrete_plan(
        ws,
        scx.interner(),
        concrete_plan,
        inst.symbol(ws, scx.interner()),
    )
    .map_err(|reason| {
        CompilerError::new_with_label(
            f.name_span,
            format!(
                "cannot build concrete coroutine plan for `{}`: {}",
                f.name,
                reason.bucket()
            ),
        )
    })?;
    let (lua, code_artifact, allocation_bodies, mut deps, statics) =
        crate::coroutine_emit::emit_coroutine_with_artifact(ws, cg, scx, f, &plan, mf)?;
    times.emit = te.elapsed();
    times.emit_work = times.emit > Duration::ZERO;
    times.regalloc = skull_codegen_lua::regalloc_time_elapsed().min(times.emit);
    times.regalloc_work = times.regalloc > Duration::ZERO;
    // No declaration edge here: "this instance instantiates that declaration" is
    // the GRAPH's fact, and `update_chunk_cache` reads it through
    // `instances_of`. `deps` carries only what codegen alone knows -- which
    // bodies this chunk's TEXT contains.
    let mut abi_deps = FxHashSet::default();
    collect_abi_dependencies(mf, &mut abi_deps);
    deps.extend(abi_deps.iter().copied());
    Ok(Chunk {
        abi_deps,
        lua,
        literal_globals: skull_mir::reach::global_ref_names(mf),
        allocation_bodies,
        embedded_deps: deps.clone(),
        deps,
        direct_globals: Vec::new(),
        direct_calls: Vec::new(),
        times,
        attribution: UnitAttribution::default(),
        statics,
        comptime_reads: mf.comptime_reads.clone(),
        calls: skull_mir::reach::call_targets(mf),
        specs,
        code_artifact,
        // A coroutine's emitted text is a constructor plus a `__step`, neither
        // of which is the declaration's own body, so neither binds callees.
        binds: Vec::new(),
    })
}

/// Emit one ordinary or coroutine function body to a cacheable [`Chunk`]. Extern
/// and `inline(lua)` functions are NOT handled here — they are cheap and
/// regenerated in [`assemble`]. Captures per-stage emit time into the chunk so the
/// parallel chunk fan-out's per-unit elapsed time still rolls up into
/// `backend-stats`.
fn emit_body_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    self_ref: FuncRef,
    f: &bone_semantic::hir::Function,
    inline_cache: &mut skull_transform::InlineCache,
) -> Result<Chunk, CompilerError> {
    emit_body_chunk_with_mutation(
        ws,
        cg,
        scx,
        concrete,
        self_ref,
        f,
        inline_cache,
        BodyMutation::None,
    )
}

fn emit_body_chunk_with_mutation(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    self_ref: FuncRef,
    f: &bone_semantic::hir::Function,
    inline_cache: &mut skull_transform::InlineCache,
    mutation: BodyMutation,
) -> Result<Chunk, CompilerError> {
    let mut times = PhaseTimes::default();
    let mut attribution = UnitAttribution {
        owner: inline_owner_key(self_ref),
        ..UnitAttribution::default()
    };
    let tl = Instant::now();
    let plan = concrete.plan_for(&shared(self_ref)).ok_or_else(|| {
        CompilerError::new_with_label(f.name_span, format!("no concrete plan for `{}`", f.name))
    })?;
    let ordinary_instance = plan.function_instance().cloned();
    let canonical_body = ordinary_instance
        .as_ref()
        .and_then(|instance| inline_cache.canonical_body(instance));
    let canonical_reused = canonical_body.is_some();
    let mut mf = match canonical_body {
        Some(body) => (*body).clone(),
        None => skull_mir::lower::lower_function_with_plans(ws, scx, plan, &concrete.plans)?,
    };
    times.lower = tl.elapsed();
    times.lower_work = mf.insts.len() > 0;
    attribution.examined = true;
    attribution.hir_nodes =
        hir_nodes_for_owner(ws, self_ref.module, BodyOwner::Function(self_ref.func));
    attribution.mir_instructions = mf.insts.len();
    attribution.lower_facts = mf.lower_attribution.clone();
    attribution.consumer_instance = ordinary_instance
        .clone()
        .map(|instance| (instance, LoweredInstanceKind::Ordinary));
    attribution.canonical_reused = canonical_reused;
    if !canonical_reused {
        attribution.lowered_instance = ordinary_instance
            .clone()
            .map(|instance| (instance, LoweredInstanceKind::Ordinary));
    }
    match mutation {
        BodyMutation::None => {}
        #[cfg(test)]
        BodyMutation::RemoveCalleeOwner { caller, callee }
        | BodyMutation::ClearCalleeOwnerKey { caller, callee }
            if caller == self_ref =>
        {
            let (call, owner) = mf
                .callee_instances
                .iter()
                .find(|(_, owner)| owner.func == callee)
                .map(|(call, owner)| (*call, owner.clone()))
                .expect("the mutation fixture must lower a concrete generic call");
            assert!(
                !owner.args.is_empty(),
                "the mutation fixture must have a complete concrete owner"
            );
            assert!(
                mf.spec_bindings.contains_key(&call),
                "the mutation must retain the specialized delivery record"
            );
            assert!(
                mf.call_abi_plans.contains_key(&call),
                "the mutation must retain the lowered ABI record"
            );
            match mutation {
                BodyMutation::RemoveCalleeOwner { .. } => {
                    mf.callee_instances.remove(&call);
                }
                BodyMutation::ClearCalleeOwnerKey { .. } => {
                    mf.callee_instances
                        .get_mut(&call)
                        .expect("owner disappeared before key mutation")
                        .args
                        .clear();
                }
                BodyMutation::None => unreachable!(),
                BodyMutation::ClearBoundPlan { .. } => unreachable!(),
                BodyMutation::ClearCoroutineResultPlan { .. } => unreachable!(),
                BodyMutation::ClearIteratorWitnessInstance { .. }
                | BodyMutation::SkipAbiPlanPublish => {
                    unreachable!()
                }
                BodyMutation::ConcreteResultOrder(_) => unreachable!(),
                #[cfg(all(test, not(feature = "sequential")))]
                BodyMutation::SetupLaneSchedule(_) => unreachable!(),
            }
        }
        #[cfg(test)]
        _ => {}
    }
    if f.is_coroutine {
        let owner = BodyOwner::Function(self_ref.func);
        let ctx = skull_mir::bound::concrete_body_ctx(
            ws,
            self_ref.module,
            owner,
            &plan.bound_tree,
            scx.interner(),
        );
        let mut chunk = emit_coroutine_chunk(ws, cg, scx, &ctx, plan, f, &mut mf, times)?;
        attribution.mir_instructions = mf.insts.len();
        attribution.times = chunk.times;
        chunk.attribution = attribution;
        return Ok(chunk);
    }
    // Inline eligible direct calls before cleanup so a hot loop traces with no
    // per-call frame.
    let before_inline = (mf.insts.len(), mf.blocks.len(), mf.cells.len());
    let ti = Instant::now();
    let (spliced, splices, scan_stats) = skull_transform::inline_functions_with_plans(
        ws,
        scx,
        self_ref,
        &mut mf,
        inline_cache,
        &concrete.plans,
    )?;
    let mut scan_stats = scan_stats;
    if let Some(instance) = ordinary_instance {
        scan_stats.ordinary_keys.push(instance);
    }
    times.inline = ti.elapsed();
    attribution.inline.add_records(&splices);
    attribution.inline.ordinary_splices += splices.len();
    attribution.inline.add_scan(scan_stats);
    times.inline_work = !splices.is_empty();
    attribution.inline.add_allocations(
        before_inline,
        (mf.insts.len(), mf.blocks.len(), mf.cells.len()),
    );
    let mut deps: FxHashSet<FuncRef> = spliced.into_iter().collect();
    let cleanup_before = mir_live_instructions(&mf);
    let tc = Instant::now();
    skull_transform::cleanup_with_nobox(ws, &mut mf)?; // drop dead values + dead-cell stores
    times.cleanup = tc.elapsed();
    let cleanup_after = mir_live_instructions(&mf);
    attribution.inline.cleanup_live_before += cleanup_before;
    attribution.inline.cleanup_live_after += cleanup_after;
    times.cleanup_work = cleanup_after != cleanup_before;
    // Shared closure conversion:
    // every `Inst::Lambda` in this ORDINARY fn becomes a `MakeEnv` record / bare
    // `Global` + a hoisted sibling in `nested_fns`. Runs AFTER inline+cleanup,
    // BEFORE FORL recovery (matching the JS pipeline) so a loop lifted into a
    // lambda body is FORL-recovered in the split fn. Coroutines skip this (their
    // `Inst::Lambda`s convert at codegen via `FrameCtx`/`HoistPlan` -- S3).
    let name = mangle_fn(ws, f);
    let pre_nested = mf.nested_fns.len();
    let tclosure = Instant::now();
    skull_transform::convert_closures(&mut mf, &name);
    times.closure = tclosure.elapsed();
    times.closure_work = mf.nested_fns.len() > pre_nested;
    // Route each pass-hoisted `<name>__lambda<k>` through the synthetic-name
    // collision guard now that they enter the Lua global namespace (a user fn
    // named `foo__lambda0` must reject, not silently clobber). The pass appends
    // its lambda bodies after any named nested `fn`s, so guard only the new tail.
    for nf in &mf.nested_fns[pre_nested..] {
        cg.mint_synthetic(nf.name.to_string());
    }
    convert_named_nested_closures(cg, &mut mf.nested_fns[..pre_nested]);
    let counted_before = mf.blocks.len();
    let tcounted = Instant::now();
    skull_transform::recover_counted_loops(ws, &mut mf); // desugared iter loops -> FORL shape
    times.counted_loop = tcounted.elapsed();
    times.counted_loop_work = mf.blocks.len() > counted_before;
    // Hoisted bodies inline in their own right, and only HERE. The inliner scans a
    // caller's top-level blocks and never a closure sub-CFG, because a cell used in
    // a lambda body is an upvalue and splicing there piles onto LuaJIT's 60-upvalue
    // cap. Once `convert_closures` has split those bodies out they are ordinary
    // standalone functions with their own top-level CFG, so that reason is gone --
    // running the pass before the split would hit it, running it after does not.
    //
    // Correctness, not just an optimization: a call the lowerer gave the
    // specialized scalar ABI has no serviceable shared body, and for a
    // column-stored element the shared `next` reads a `_data` that does not exist.
    let mut hoisted_work = InlineWork::default();
    inline_hoisted_bodies(
        ws,
        scx,
        self_ref,
        &mut mf.nested_fns,
        inline_cache,
        &concrete.plans,
        &mut deps,
        &mut times,
        &mut hoisted_work,
    )
    .map_err(|msg| CompilerError::new_with_label(f.name_span, msg))?;
    times.hoisted_inline_work = hoisted_work.accepted_splices > 0;
    attribution.inline.add(hoisted_work);
    let tv = Instant::now();
    skull_transform::validate_no_residual_bound_calls(ws, f, &mf)?;
    let mut specs = Vec::new();
    let owner = BodyOwner::Function(self_ref.func);
    let ctx = skull_mir::bound::concrete_body_ctx(
        ws,
        self_ref.module,
        owner,
        &plan.bound_tree,
        scx.interner(),
    );
    validate_emitted_body(ws, &ctx, &mf, &mut specs).map_err(|r| r.into_error(f.name_span, ""))?;
    let embedded_deps = deps.clone();
    collect_callee_frefs(&mf, &mut deps);
    let mut code_artifact =
        skull_mir::code_identity::CodeArtifact::from_mir_tree_with_volatile_vars(
            &mf,
            &ws.comptime_inputs.volatile_vars,
        );
    code_artifact.set_body_names(&mf, &mangle_fn(ws, f));
    let (direct_globals, direct_calls) = direct_refs(ws, &mf);
    times.validation = tv.elapsed();
    times.validation_work = true;
    skull_codegen_lua::regalloc_time_reset();
    let te = Instant::now();
    let bind = skull_codegen_lua::callee_binding_plan(ws, cg, scx.interner(), &name, self_ref, &mf);
    let emission =
        emit_function_with_allocation_report(ws, cg, scx.interner(), self_ref, &mf, bind.as_ref());
    let mut lua = emission.lua;
    let mut allocation_bodies = Vec::new();
    if let Some(body) = crate::allocation_report::collect_emitted_body(
        ws,
        scx.interner(),
        &name,
        f.name.as_str(),
        &mf,
        emission.allocation_sites,
    ) {
        allocation_bodies.push(body);
    }
    times.emit = te.elapsed();
    times.emit_work = times.emit > Duration::ZERO;
    times.regalloc = skull_codegen_lua::regalloc_time_elapsed().min(times.emit);
    times.regalloc_work = times.regalloc > Duration::ZERO;
    // Nested `fn`s are hoisted to sibling top-level Lua fns in the same chunk
    // (their `__local_*` names are already final -- emit_named, no mangling). Their
    // `static` cells fold into THIS chunk's `statics` so the hot-reload preamble +
    // overlay reset see them -- a nested-fn static is otherwise invisible to both.
    let mut statics = mf.statics.clone();
    emit_nested_fns(
        ws,
        cg,
        scx,
        &mut mf.nested_fns,
        &mf.statics,
        &mut deps,
        &mut statics,
        &mut allocation_bodies,
        &mut lua,
        &mut times,
    )?;
    attribution.mir_instructions = mf.insts.len();
    attribution.times = times;
    let calls = skull_mir::reach::call_targets(&mf);
    let mut abi_deps = FxHashSet::default();
    collect_abi_dependencies(&mf, &mut abi_deps);
    deps.extend(abi_deps.iter().copied());
    Ok(Chunk {
        abi_deps,
        lua,
        literal_globals: skull_mir::reach::global_ref_names(&mf),
        allocation_bodies,
        deps,
        embedded_deps,
        direct_globals,
        direct_calls,
        times,
        statics,
        calls,
        specs,
        code_artifact,
        attribution,
        comptime_reads: mf.comptime_reads.clone(),
        binds: bind.into_iter().collect(),
    })
}

/// Emit each hoisted nested `fn` (and its own nested fns, depth-first) as a
/// top-level Lua function appended to the enclosing chunk. Runs the standard
/// cleanup pipeline per fn, folds their callee refs into the chunk's deps (so an
/// edit to a callee still invalidates the enclosing chunk) and their `static`
/// cells into the chunk's `statics` (so hot-reload can reset them).
pub(crate) fn emit_nested_fns(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    nested: &mut [MirFunc],
    inherited_statics: &[skull_mir::mir::StaticCell],
    deps: &mut FxHashSet<FuncRef>,
    statics: &mut Vec<skull_mir::mir::StaticCell>,
    allocation_bodies: &mut Vec<crate::allocation_report::AllocationBody>,
    out: &mut String,
    times: &mut PhaseTimes,
) -> Result<(), CompilerError> {
    for nf in nested.iter_mut() {
        let cleanup_before = mir_live_instructions(nf);
        let tc = Instant::now();
        skull_transform::cleanup_with_nobox(ws, nf)?;
        times.cleanup += tc.elapsed();
        times.cleanup_work |= mir_live_instructions(nf) != cleanup_before;
        let counted_before = nf.blocks.len();
        let tl = Instant::now();
        skull_transform::recover_counted_loops(ws, nf);
        times.counted_loop += tl.elapsed();
        times.counted_loop_work |= nf.blocks.len() > counted_before;
        collect_callee_frefs(nf, deps);
        // A flat variant's text is another declaration's body.
        if let Some(source) = nf.flat_variant_source {
            deps.insert(source);
        }
        statics.extend(nf.statics.iter().cloned());
        skull_codegen_lua::regalloc_time_reset();
        let te = Instant::now();
        // A hoisted nested fn has no declaration of its own, so no call-graph
        // component and no callee binding.
        let emission = emit_named_with_allocation_report(
            ws,
            cg,
            scx.interner(),
            nf.name.as_str(),
            nf,
            inherited_statics,
            None,
        );
        out.push_str(&emission.lua);
        if let Some(body) = crate::allocation_report::collect_emitted_body(
            ws,
            scx.interner(),
            nf.name.as_str(),
            nf.name.as_str(),
            nf,
            emission.allocation_sites,
        ) {
            allocation_bodies.push(body);
        }
        let emit = te.elapsed();
        times.emit += emit;
        times.emit_work |= emit > Duration::ZERO;
        let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(emit);
        times.regalloc += regalloc;
        times.regalloc_work |= regalloc > Duration::ZERO;
        let mut children = std::mem::take(&mut nf.nested_fns);
        let mut next_inherited = inherited_statics.to_vec();
        next_inherited.extend(nf.statics.iter().cloned());
        emit_nested_fns(
            ws,
            cg,
            scx,
            &mut children,
            &next_inherited,
            deps,
            statics,
            allocation_bodies,
            out,
            times,
        )?;
    }
    Ok(())
}

pub(crate) fn convert_named_nested_closures(cg: &CodegenCtx, nested: &mut [MirFunc]) {
    for nf in nested {
        let pre_nested = nf.nested_fns.len();
        let name = nf.name;
        skull_transform::convert_closures(nf, name.as_str());
        for child in &nf.nested_fns[pre_nested..] {
            cg.mint_synthetic(child.name.to_string());
        }
        convert_named_nested_closures(cg, &mut nf.nested_fns[..pre_nested]);
    }
}

/// Emit the flat list of pass-produced hoisted lambda bodies (`convert_closures`
/// appends one `MirFunc` per live lambda, nested ones included) as sibling
/// top-level fns appended to `out`. Used by the decorator/global-init units, which
/// -- unlike [`emit_nested_fns`] -- fold no deps/statics (their `UnitOut` tracks
/// neither) and run no `recover_counted_loops` (their loops recover at codegen,
/// same as the co-fn step).
/// The `static` cells of a hoisted-lambda tree, for the callers that emit it
/// through [`emit_pass_nested_fns`] and so do not fold them along the way.
fn collect_nested_statics(nested: &[MirFunc], out: &mut Vec<skull_mir::mir::StaticCell>) {
    for nf in nested {
        out.extend(nf.statics.iter().cloned());
        collect_nested_statics(&nf.nested_fns, out);
    }
}

fn emit_pass_nested_fns(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    nested: &mut [MirFunc],
    inherited_statics: &[skull_mir::mir::StaticCell],
    mut allocation_bodies: Option<&mut Vec<crate::allocation_report::AllocationBody>>,
    out: &mut String,
    times: &mut PhaseTimes,
) -> Result<(), CompilerError> {
    for nf in nested.iter_mut() {
        let cleanup_before = mir_live_instructions(nf);
        let tc = Instant::now();
        skull_transform::cleanup_with_nobox(ws, nf)?;
        times.cleanup += tc.elapsed();
        times.cleanup_work |= mir_live_instructions(nf) != cleanup_before;
        skull_codegen_lua::regalloc_time_reset();
        let te = Instant::now();
        if let Some(bodies) = allocation_bodies.as_deref_mut() {
            let emission = emit_named_with_allocation_report(
                ws,
                cg,
                scx.interner(),
                nf.name.as_str(),
                nf,
                inherited_statics,
                None,
            );
            out.push_str(&emission.lua);
            if let Some(body) = crate::allocation_report::collect_emitted_body(
                ws,
                scx.interner(),
                nf.name.as_str(),
                nf.name.as_str(),
                nf,
                emission.allocation_sites,
            ) {
                bodies.push(body);
            }
        } else {
            out.push_str(&skull_codegen_lua::emit_named_with_statics(
                ws,
                cg,
                scx.interner(),
                nf.name.as_str(),
                nf,
                inherited_statics,
                None,
            ));
        }
        let emit = te.elapsed();
        times.emit += emit;
        times.emit_work |= emit > Duration::ZERO;
        let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(emit);
        times.regalloc += regalloc;
        times.regalloc_work |= regalloc > Duration::ZERO;
    }
    Ok(())
}

/// Whether a function body is cached as a [`Chunk`] (ordinary or coroutine) vs
/// regenerated inline (extern / `inline(lua)`). An `Iterator`-bounded fn is
/// NEITHER: it is emitted STANDALONE per instantiation, resolving its
/// placeholder bound-calls from the tree that specialization demand carries, so
/// there is no declaration-keyed chunk to cache.
pub fn is_cached_body(f: &bone_semantic::hir::Function) -> bool {
    !f.is_extern && f.inline_body.is_none() && !fn_has_bounds(f) && !is_generic(f)
}

/// Whether THIS EMITTED BODY is a cached [`Chunk`] -- the same question
/// [`is_cached_body`] answers for a declaration, asked of the body actually
/// being emitted.
///
/// A SPECIALIZATION always is. It is a concrete instance with a body of its own,
/// so the two reasons a declaration is not cached under E1 -- it is generic, or
/// it is `Iterator`-bounded -- are precisely the reasons the specialization
/// EXISTS. Only opacity survives the distinction, and an opaque declaration has
/// no specializations to begin with.
///
/// Asking the declaration's predicate of an emitted body is what deleted a
/// generic's specialized chunk on every edit: `is_cached_body` said false, the
/// caller took its "regenerated, drop the chunk" branch, and the overlay then
/// found nothing to redefine.
pub fn is_cached_emitted_body(f: &bone_semantic::hir::Function, body: &Instance) -> bool {
    if body.args.is_empty() {
        return is_cached_body(f);
    }
    !f.is_extern && f.inline_body.is_none()
}

/// Whether the fn declares generics, and so has NO declaration-owned body.
///
/// This is the E1 cut stated as a predicate. A generic declaration is not a
/// definition that happens to be shared across its instantiations -- it is not a
/// definition at all, the way an `extern` or an `inline(lua)` body is not one.
/// Every emitted body comes from an instance, and a declaration nothing
/// instantiates emits nothing.
pub fn is_generic(f: &bone_semantic::hir::Function) -> bool {
    !f.generics.is_empty()
}

/// Whether the fn declares any `Iterator` bound on its generics.
fn fn_has_bounds(f: &bone_semantic::hir::Function) -> bool {
    f.generic_bounds.iter().any(Option::is_some)
}

/// Emit one top-level Lua function per distinct `(callee, instantiation)` a body
/// asked for, under the name its call sites already target (`spec_fn_name`).
///
/// This is what lets specialization reach contexts the inliner cannot enter. A
/// splice needs a caller whose CFG the inliner will walk; a standalone definition
/// needs nothing, so a coroutine's `__step` and a global initializer -- neither of
/// which runs an inline pass -- call the specialized copy exactly as any other
/// body does.
///
/// A worklist, not a loop over the initial set: a specialized body is lowered
/// under its own substitution and can therefore require further specializations
/// that no shared body ever mentions (`Vec<Vec2>.remove_at` calling
/// `__vec_data_set` at `T = Vec2`). `done` keys on the full `Instance`
/// so each is defined exactly once no matter how many bodies requested it.
///
/// A failing specialization does NOT stop the worklist: each instantiation is an
/// independent definition, and bailing at the first one made every census of
/// remaining nobox rejections a floor -- the three members of the column-read
/// class were each found one behind another. The rejections are returned (sorted
/// by specialized name, since the worklist order is a pop order and two
/// instantiations of one generic share its span) and the caller fails the build
/// on all of them; a rejected specialization contributes NO Lua.
/// Build ONE specialized body's chunk -- the same transform sequence and the
/// same rendering the worklist ran inline before it was a chunk.
///
/// `deps` carries the DECLARATION this specializes as well as its splices and
/// residual callees. That edge is what makes editing a generic's body invalidate
/// every specialization of it: nothing else names the connection, since a
/// specialization is not reached through the declaration's own chunk.
///
/// Two fields are deliberately empty, matching what the inline worklist
/// contributed:
/// - `direct_globals` / `direct_calls`: global-init ordering is stated over
///   declarations, and a specialization's ordering facts are its declaration's.
///
/// `statics` is NOT one of them. A specialized body's `static` cells are cells
/// the emitted program uses, so leaving them out would leave them without a
/// reload reset. With emission owned by the
/// instance, every generic body with a `static` is one.
fn emit_spec_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    body: &Instance,
    name: &str,
    inline_cache: &mut skull_transform::InlineCache,
    specs: &mut Vec<skull_mir::bound::SpecDemand>,
) -> Result<Chunk, CompilerError> {
    let fref = body.func;
    let f = &ws.module(fref.module).functions[fref.func];
    let plan = concrete.plan_for(body).ok_or_else(|| {
        CompilerError::new_with_label(f.name_span, format!("no concrete plan for `{name}`"))
    })?;
    let tree = &plan.bound_tree;
    skull_mir::lower::validate_concrete_bound_plan(f, plan)?;
    let tl = Instant::now();
    let canonical_body = inline_cache.canonical_body(body);
    let canonical_reused = canonical_body.is_some();
    let mut mf = match canonical_body {
        Some(body) => (*body).clone(),
        None => skull_mir::lower::lower_instance_with_plans(ws, scx, plan, &concrete.plans)
            .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?,
    };
    let mut times = PhaseTimes {
        lower: tl.elapsed(),
        lower_work: !mf.insts.is_empty(),
        ..PhaseTimes::default()
    };
    let mut attribution = UnitAttribution {
        owner: name.to_string(),
        examined: true,
        hir_nodes: hir_nodes_for_owner(ws, fref.module, BodyOwner::Function(fref.func)),
        mir_instructions: mf.insts.len(),
        lower_facts: mf.lower_attribution.clone(),
        consumer_instance: Some((body.clone(), LoweredInstanceKind::Specialized)),
        lowered_instance: (!canonical_reused)
            .then(|| (body.clone(), LoweredInstanceKind::Specialized)),
        canonical_reused,
        ..UnitAttribution::default()
    };
    // BEFORE every transform: an `Iterator`-bounded body's placeholders are what
    // make it unemittable on its own, and the demand that reached here is what
    // resolves them. The splice path does the same substitution at the moment it
    // clones the body into a caller.
    let tresolve = Instant::now();
    skull_transform::bound::resolve_standalone_bound_calls(
        ws,
        scx,
        scx.interner(),
        &mut mf,
        tree,
        &concrete.plans,
    )
    .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?;
    times.validation += tresolve.elapsed();
    times.validation_work = true;
    let owner = BodyOwner::Function(fref.func);
    let ctx = skull_mir::bound::concrete_body_ctx(ws, fref.module, owner, tree, scx.interner());
    if f.is_coroutine {
        let mut chunk = emit_coroutine_chunk(ws, cg, scx, &ctx, plan, f, &mut mf, times)?;
        attribution.mir_instructions = mf.insts.len();
        attribution.times = chunk.times;
        chunk.attribution = attribution;
        return Ok(chunk);
    }
    let (mut deps, prepared_times, prepared_inline) = prepare_instance_body_with_attribution(
        ws,
        scx,
        fref,
        name,
        &mut mf,
        inline_cache,
        &concrete.plans,
    )
    .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?;
    times.add(&prepared_times);
    let mut prepared_inline = prepared_inline;
    prepared_inline.specialized_splices = prepared_inline.accepted_splices;
    prepared_inline.census.ordinary_keys.push(body.clone());
    attribution.inline.add(prepared_inline);
    let before = specs.len();
    // Discovered BEFORE the verdict, and handed back through `specs` even when
    // the verdict rejects: the inline worklist pushed them into the live queue
    // first, so a failing build reports over that same closure.
    let tv = Instant::now();
    validate_emitted_body(ws, &ctx, &mf, specs)
        .map_err(|r| r.into_error(f.name_span, &format!("cannot emit specialized `{name}`: ")))?;
    times.validation += tv.elapsed();
    times.validation_work = true;
    let specs = specs[before..].to_vec();
    let mut code_artifact =
        skull_mir::code_identity::CodeArtifact::from_mir_tree_with_volatile_vars(
            &mf,
            &ws.comptime_inputs.volatile_vars,
        );
    code_artifact.set_body_names(&mf, name);
    skull_codegen_lua::regalloc_time_reset();
    let te = Instant::now();
    let bind = skull_codegen_lua::callee_binding_plan(ws, cg, scx.interner(), name, fref, &mf);
    let emission =
        emit_named_with_allocation_report(ws, cg, scx.interner(), name, &mf, &[], bind.as_ref());
    let mut lua = emission.lua;
    let mut allocation_bodies = Vec::new();
    if let Some(body) = crate::allocation_report::collect_emitted_body(
        ws,
        scx.interner(),
        name,
        f.name.as_str(),
        &mf,
        emission.allocation_sites,
    ) {
        allocation_bodies.push(body);
    }
    let root_emit = te.elapsed();
    times.emit += root_emit;
    times.emit_work |= root_emit > Duration::ZERO;
    let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(root_emit);
    times.regalloc += regalloc;
    times.regalloc_work |= regalloc > Duration::ZERO;
    let mut statics = mf.statics.clone();
    let literal_globals = skull_mir::reach::global_ref_names(&mf);
    let mut nested_abi_deps = FxHashSet::default();
    collect_abi_dependencies(&mf, &mut nested_abi_deps);
    let mut nested = std::mem::take(&mut mf.nested_fns);
    collect_nested_statics(&nested, &mut statics);
    emit_pass_nested_fns(
        ws,
        cg,
        scx,
        &mut nested,
        &statics,
        Some(&mut allocation_bodies),
        &mut lua,
        &mut times,
    )?;
    collect_callee_frefs(&mf, &mut deps);
    attribution.mir_instructions = mf.insts.len();
    attribution.times = times;
    let mut abi_deps = nested_abi_deps;
    collect_abi_dependencies(&mf, &mut abi_deps);
    deps.extend(abi_deps.iter().copied());
    Ok(Chunk {
        abi_deps,
        lua,
        allocation_bodies,
        embedded_deps: deps.clone(),
        deps,
        literal_globals,
        direct_globals: Vec::new(),
        direct_calls: Vec::new(),
        times,
        attribution,
        statics,
        comptime_reads: mf.comptime_reads.clone(),
        calls: skull_mir::reach::call_targets(&mf),
        specs,
        code_artifact,
        binds: bind.into_iter().collect(),
    })
}

/// Render every specialized body the liveness walk found, in the order the
/// assembled text has always had.
///
/// This ORDERS text; it does not decide or discover. The worklist is still LIFO
/// and still dedupes, because that is what fixes the order, but every body it
/// pops was already reached and built by [`live_bodies`]. A body it cannot find
/// is an invariant failure rather than something to quietly build: two places
/// that can decide a definition into the program is exactly the pair this step
/// removes.
fn emit_specialized_callees(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    pending: &mut Vec<skull_mir::bound::SpecDemand>,
    cached: &FxHashMap<Instance, Result<Chunk, CompilerError>>,
    live: &FxHashMap<Instance, Chunk>,
    rejected: &[(String, CompilerError)],
    built: &mut Vec<(Instance, Chunk)>,
    allocation_bodies: &mut Vec<crate::allocation_report::AllocationBody>,
    statics: &mut Vec<skull_mir::mir::StaticCell>,
    out: &mut LuaDefinitionBuffer,
    binds: &mut Vec<skull_codegen_lua::CalleeBindingPlan>,
    attribution: &mut AttributionAccumulator,
    times: &mut PhaseTimes,
) -> Vec<CompilerError> {
    let mut done: FxHashSet<Instance> = FxHashSet::default();
    let mut errs: Vec<(String, CompilerError)> = rejected.to_vec();
    let rejected_names: FxHashSet<&str> = rejected.iter().map(|(n, _)| n.as_str()).collect();
    while let Some(demand) = pending.pop() {
        let body = demand.inst;
        let fref = body.func;
        if !done.insert(body.clone()) {
            continue;
        }
        let f = &ws.module(fref.module).functions[fref.func];
        let name = body.symbol(ws, scx.interner());
        let Some(chunk) = live
            .get(&body)
            .or_else(|| cached.get(&body).and_then(|c| c.as_ref().ok()))
        else {
            // Not a fallback: the walk either built this body or recorded why it
            // could not, and a rejected one is reported through `rejected`.
            if !rejected_names.contains(name.as_str()) {
                errs.push((
                    name.clone(),
                    CompilerError::new_with_label(
                        f.name_span,
                        format!("internal: specialized `{name}` is emitted but no liveness walk reached it"),
                    ),
                ));
            }
            continue;
        };
        out.push_str(&chunk.lua);
        binds.extend(chunk.binds.iter().cloned());
        allocation_bodies.extend(chunk.allocation_bodies.iter().cloned());
        attribution.add_unit(&chunk.attribution);
        times.add(&chunk.times);
        // A specialized definition's `static` cells are cells the program uses,
        // so they register for hot-reload preservation on the same terms as a
        // declaration's. Folded HERE because this is where a specialized body
        // enters the output -- the declaration loop never sees one.
        statics.extend(chunk.statics.iter().cloned());
        pending.extend(chunk.specs.iter().cloned());
        if !cached.contains_key(&body) {
            built.push((body.clone(), chunk.clone()));
        }
    }
    errs.sort_by(|(a, _), (b, _)| a.cmp(b));
    errs.into_iter().map(|(_, e)| e).collect()
}

/// **THE transform sequence an instance body is emitted through.**
///
/// Extracted so `emit_specialized_callees` and `skull::site_join`'s census
/// consume ONE sequence rather than two that agree today. The census's whole
/// claim is about the body EMISSION sees, so a census that re-listed these
/// passes would be free to drift into measuring a body no emitter renders --
/// and the drift would look like a clean measurement.
///
/// Everything after this point is rendering: `validate_emitted_body`,
/// `emit_named` and the nested-fn emission. Nothing here mints a name or
/// touches a `CodegenCtx`, which is what lets a read-only consumer run it.
pub fn prepare_instance_body_with_plans(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    fref: FuncRef,
    name: &str,
    mf: &mut skull_mir::mir::MirFunc,
    cache: &mut skull_transform::InlineCache,
    concrete_plans: &skull_mir::concrete::ConcretePlans,
) -> Result<FxHashSet<FuncRef>, CompilerError> {
    prepare_instance_body_with_attribution(ws, scx, fref, name, mf, cache, concrete_plans)
        .map(|(spliced, _, _)| spliced)
}

fn prepare_instance_body_with_attribution(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    fref: FuncRef,
    name: &str,
    mf: &mut skull_mir::mir::MirFunc,
    cache: &mut skull_transform::InlineCache,
    concrete_plans: &skull_mir::concrete::ConcretePlans,
) -> Result<(FxHashSet<FuncRef>, PhaseTimes, InlineWork), CompilerError> {
    let mut times = PhaseTimes::default();
    let mut work = InlineWork::default();
    let before = (mf.insts.len(), mf.blocks.len(), mf.cells.len());
    let ti = Instant::now();
    let (spliced, records, scan_stats) =
        skull_transform::inline_functions_with_plans(ws, scx, fref, mf, cache, concrete_plans)?;
    times.inline = ti.elapsed();
    times.inline_work = !records.is_empty();
    work.add_records(&records);
    work.add_scan(scan_stats);
    work.add_allocations(before, (mf.insts.len(), mf.blocks.len(), mf.cells.len()));
    let cleanup_before = mir_live_instructions(mf);
    let tc = Instant::now();
    skull_transform::cleanup_with_nobox(ws, mf)?;
    times.cleanup = tc.elapsed();
    let cleanup_after = mir_live_instructions(mf);
    work.cleanup_live_before += cleanup_before;
    work.cleanup_live_after += cleanup_after;
    times.cleanup_work = cleanup_after != cleanup_before;
    let tclosure = Instant::now();
    let nested_before = mf.nested_fns.len();
    skull_transform::convert_closures(mf, name);
    times.closure = tclosure.elapsed();
    times.closure_work = mf.nested_fns.len() > nested_before;
    let counted_before = mf.blocks.len();
    let tcounted = Instant::now();
    skull_transform::recover_counted_loops(ws, mf);
    times.counted_loop = tcounted.elapsed();
    times.counted_loop_work = mf.blocks.len() > counted_before;
    Ok((spliced.into_iter().collect(), times, work))
}

/// Every rejection one assembly found, across all four kinds of emitted
/// definition (decorator body, global initializer, function chunk, specialized
/// instantiation). Filled in a fixed order -- source order within each kind, the
/// kinds in the order `assemble` visits them -- so the reported list never
/// depends on thread scheduling or hash-map iteration.
///
/// The accumulation boundary is the DEFINITION: each one either emits or does
/// not, and rejections within one body are already collected by
/// [`BodyRejection`]. Aborting at the first definition is what made every count
/// of remaining rejections a floor -- every rejection here is fully computed
/// before any is reported, so the abort hid them rather than preventing them.
/// Accumulating does not soften anything: a non-empty set fails the build, and
/// the emitted Lua is discarded with the `Err`.
#[derive(Default)]
struct Rejections(Vec<CompilerError>);

impl Rejections {
    fn push(&mut self, e: CompilerError) {
        self.0.push(e);
    }

    fn extend(&mut self, es: impl IntoIterator<Item = CompilerError>) {
        self.0.extend(es);
    }

    /// The first rejection, carrying the rest as siblings (the shared sink every
    /// renderer expands). `Ok` only when nothing was rejected.
    fn into_result(self) -> Result<(), CompilerError> {
        let mut it = self.0.into_iter();
        match it.next() {
            Some(primary) => Err(primary.with_siblings(it)),
            None => Ok(()),
        }
    }
}

/// Run the inliner over a hoisted-body tree (named nested `fn`s and the lambda
/// bodies `convert_closures` appends), depth-first so a lambda nested in a lambda
/// is reached too. Splices fold into the enclosing chunk's `deps`: the chunk now
/// contains a copy of each spliced callee's body, so an edit to that callee has to
/// invalidate this chunk.
fn inline_hoisted_bodies(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    self_ref: FuncRef,
    nested: &mut [MirFunc],
    inline_cache: &mut skull_transform::InlineCache,
    concrete_plans: &skull_mir::concrete::ConcretePlans,
    deps: &mut FxHashSet<FuncRef>,
    times: &mut PhaseTimes,
    work: &mut InlineWork,
) -> Result<(), String> {
    for nf in nested.iter_mut() {
        // Flat variants embed source without going through an inline splice.
        if let Some(source) = nf.flat_variant_source {
            deps.insert(source);
        }
        let before = (nf.insts.len(), nf.blocks.len(), nf.cells.len());
        let ti = Instant::now();
        let (spliced, records, scan_stats) = skull_transform::inline_functions_with_plans(
            ws,
            scx,
            self_ref,
            nf,
            inline_cache,
            concrete_plans,
        )
        .map_err(|e| e.message)?;
        times.hoisted_inline += ti.elapsed();
        work.add_records(&records);
        work.hoisted_splices += records.len();
        work.add_scan(scan_stats);
        times.hoisted_inline_work |= !records.is_empty();
        work.add_allocations(before, (nf.insts.len(), nf.blocks.len(), nf.cells.len()));
        deps.extend(spliced);
        let cleanup_before = mir_live_instructions(nf);
        let tc = Instant::now();
        skull_transform::cleanup_with_nobox(ws, nf).map_err(|e| e.message)?;
        times.cleanup += tc.elapsed();
        let cleanup_after = mir_live_instructions(nf);
        work.cleanup_live_before += cleanup_before;
        work.cleanup_live_after += cleanup_after;
        times.cleanup_work |= cleanup_after != cleanup_before;
        let mut children = std::mem::take(&mut nf.nested_fns);
        inline_hoisted_bodies(
            ws,
            scx,
            self_ref,
            &mut children,
            inline_cache,
            concrete_plans,
            deps,
            times,
            work,
        )?;
        nf.nested_fns = children;
    }
    Ok(())
}

/// Post-transform gate: no whole-element access to a column-stored Vec may reach
/// the emitter. Column storage has no element table, so there is no single slot
/// to read or write; lowering decomposes every such access into per-column ones
/// (`column_element_repr` / `column_element_store`). Anything left is a
/// decomposition the lowerer does not implement yet -- today, a stdlib Vec entry
/// point that still goes through the boxed `_data` (`enumerate` is the one real
/// game code hits, in an application's update loop).
///
/// The emitter already refuses these (`write_index`'s `unreachable!`), but a
/// panic in a parallel emit worker is the wrong shape of loud: no span, no source
/// location, and the process exit code depends on the caller. Rejecting here
/// gives the same guarantee as a spanned `CompilerError`.
fn validate_no_whole_element_column_access(mf: &skull_mir::mir::MirFunc) -> Result<(), String> {
    use skull_mir::mir::{
        ContainerKind::{VecValueColumns, VecValueStrided},
        Inst,
    };
    for (_, block) in mf.blocks.iter() {
        for &iid in &block.insts {
            let elem = match &mf.insts[iid] {
                Inst::Index {
                    kind: VecValueColumns(e) | VecValueStrided(e),
                    ..
                }
                | Inst::StoreIndex {
                    kind: VecValueColumns(e) | VecValueStrided(e),
                    ..
                }
                | Inst::ArrayPush {
                    kind: VecValueColumns(e) | VecValueStrided(e),
                    ..
                } => e,
                _ => continue,
            };
            return Err(format!(
                "whole-element access to a flat-stored `[{elem}]` survived lowering \
                 (column and strided storage have no element table, so there is no slot to name)"
            ));
        }
    }
    Ok(())
}

/// [`validate_emitted_body`] over a hoisted-body tree (named nested `fn`s and the
/// lambda bodies `convert_closures` appends), each of which emits as its own
/// top-level Lua function and so needs the checks in its own right.
fn validate_nested_bodies(
    ws: &Workspace,
    ctx: &skull_mir::bound::BoundCtx,
    nested: &[MirFunc],
    specs: &mut Vec<skull_mir::bound::SpecDemand>,
) -> Result<(), BodyRejection> {
    let mut acc = BodyRejection::default();
    for nf in nested {
        if let Err(r) = validate_emitted_body(ws, ctx, nf, specs) {
            acc.absorb(r.prefixed(&format!("in nested body `{}`", nf.name)));
        }
    }
    acc.into_result()
}

/// Why an about-to-be-emitted body was rejected: every INDEPENDENT finding in
/// it, first reported as the primary and the rest as siblings. A body holds
/// several sinks often enough that reporting one turns any count of remaining
/// sinks into a floor, and it can hold several KINDS of finding at once (a
/// column-access gap and a `nobox` sink), which reporting only the first check
/// to fire also hides.
#[derive(Default)]
struct BodyRejection {
    findings: Vec<Finding>,
}

/// One rejection site. `guarantee` separates a LANGUAGE GUARANTEE (today:
/// `nobox`) from a codegen parity gap: the stdlib emit path downgrades a gap to
/// a silent `skipped` -- its known-gap list -- and must never downgrade a
/// guarantee, since dropping the body turns the promise into a nil global at
/// runtime. `site` is the offending instruction's own span when the check could
/// name one; it outranks the enclosing fn's span at [`BodyRejection::into_error`]
/// -- a whole-fn anchor cannot distinguish two violations in one body, which is
/// how one `nobox` sink hid another inside `draw_fractures`.
struct Finding {
    message: String,
    guarantee: bool,
    site: Option<bone_semantic::prelude::Span>,
}

impl BodyRejection {
    fn gap(message: String) -> Self {
        Self {
            findings: vec![Finding {
                message,
                guarantee: false,
                site: None,
            }],
        }
    }

    /// A guarantee violation anywhere in the body makes the whole rejection a
    /// guarantee: the stdlib path may drop a body with only gaps in it, and a
    /// body holding both must not be dropped on the strength of the gap.
    fn guarantee(&self) -> bool {
        self.findings.iter().any(|f| f.guarantee)
    }

    fn absorb(&mut self, other: BodyRejection) {
        self.findings.extend(other.findings);
    }

    fn into_result(self) -> Result<(), BodyRejection> {
        if self.findings.is_empty() {
            Ok(())
        } else {
            Err(self)
        }
    }

    fn prefixed(mut self, what: &str) -> Self {
        for f in &mut self.findings {
            f.message = format!("{what}: {}", f.message);
        }
        self
    }

    /// Spanned rejection at the offending instruction, falling back to `span`
    /// (the enclosing definition), tagged so the stdlib leniency check can tell
    /// a guarantee from a gap without matching on the message text.
    fn into_error(self, span: bone_semantic::prelude::Span, what: &str) -> CompilerError {
        let mut errs = self.findings.into_iter().map(|f| {
            let mut e = CompilerError::new_with_label(
                f.site.unwrap_or(span),
                format!("{what}{}", f.message),
            );
            if f.guarantee {
                e.kind = skull_parser::CompilerErrorKind::NoboxGuaranteeViolated;
            }
            e
        });
        let primary = errs.next().expect("a rejection holds at least one finding");
        primary.with_siblings(errs)
    }
}

/// Every post-transform invariant one about-to-be-emitted body must hold, and its
/// hoisted children with it, plus the specialized callees it needs emitted. The
/// single entry point on purpose: each check below was added because some
/// emitted-body kind was missing it, so a new one joins here rather than at one
/// call site.
fn validate_emitted_body(
    ws: &Workspace,
    ctx: &skull_mir::bound::BoundCtx,
    mf: &MirFunc,
    specs: &mut Vec<skull_mir::bound::SpecDemand>,
) -> Result<(), BodyRejection> {
    let mut acc = BodyRejection::default();
    // A demand this body cannot state in self-contained terms is a rejection,
    // not a demand that is quietly dropped: the callee would then be reached by
    // nothing and its call site would spell a definition the program lacks.
    if let Err(e) = skull_mir::reach::spec_bound_calls(ws, mf, specs) {
        acc.absorb(BodyRejection::gap(e.message));
    }
    if let Err(r) = validate_no_whole_element_column_access(mf).map_err(BodyRejection::gap) {
        acc.absorb(r);
    }
    // Name the function being emitted: the span often points into an INLINED
    // stdlib body (every `xs.push(..)` reports at the same `Vec<T>.push` line),
    // so without the caller the diagnostic cannot tell two sites apart.
    acc.findings.extend(
        skull_transform::nobox_materialization_violations(ws, mf)
            .into_iter()
            .map(|(message, site)| Finding {
                message: format!("{message}, while emitting `{}`", mf.name),
                guarantee: true,
                site,
            }),
    );
    if let Err(r) = validate_nested_bodies(ws, ctx, &mf.nested_fns, specs) {
        acc.absorb(r);
    }
    acc.into_result()
}

/// The spec-binding check for a decorator / global-init unit, whose body and
/// hoisted lambdas are emitted without ever passing through the inliner. Lenient
/// for stdlib modules on the same grounds as the fn path: their emit gaps are the
/// known parity list, a user definition that cannot emit is a hard error.
fn check_unit_spec_bindings(
    ws: &Workspace,
    ctx: &skull_mir::bound::BoundCtx,
    mid: ModuleId,
    name: &str,
    mf: &MirFunc,
    span: bone_semantic::prelude::Span,
    out_specs: &mut Vec<skull_mir::bound::SpecDemand>,
) -> Result<(), CompilerError> {
    let mut specs = Vec::new();
    let r = validate_emitted_body(ws, ctx, mf, &mut specs);
    out_specs.extend(specs);
    match r {
        Ok(()) => Ok(()),
        Err(r) if !r.guarantee() && is_stdlib_module_file(ws.module(mid).file.as_str()) => Ok(()),
        Err(r) => Err(r.into_error(span, &format!("cannot emit `{name}`: "))),
    }
}

/// EVERY [`UnitOut::hard_error`] in a parallel batch, in source order so the
/// reported list does not depend on thread scheduling. Each unit is an
/// independent definition, so a batch of them holds a site table, not one bug.
fn unit_errors(units: &[UnitOut]) -> Vec<CompilerError> {
    units.iter().filter_map(|u| u.hard_error.clone()).collect()
}

/// Assemble the full Lua program: prelude + decorator defs + the function section
/// (cached chunks for ordinary/coroutine bodies, regenerated externs/inline(lua))
/// plus dependency-ordered global inits + decorator applications. Everything except
/// the cached function chunks is regenerated here, so this is shared verbatim by
/// the full and incremental paths and there is no stale prelude/ordering.
///
/// The decorator and global definitions are re-lowered+emitted in PARALLEL
/// ([`par_map`]); cached function chunks are pulled and their captured phase
/// times summed in, so `backend-stats` still sees the full per-stage breakdown.
pub(crate) type DecoMap = FxHashMap<Ustr, (ModuleId, Idx<bone_semantic::hir::DecoratorDef>)>;

/// Name -> definition map for every `deco` in the workspace, plus the def list
/// (an application usually names a decorator declared in another module). Shared
/// by `assemble` (which also emits the def bodies) and `assemble_overlay`.
pub(crate) fn build_deco_map(
    ws: &Workspace,
) -> (
    DecoMap,
    Vec<(ModuleId, Idx<bone_semantic::hir::DecoratorDef>)>,
) {
    let mut deco_map: DecoMap = Default::default();
    let mut deco_items: Vec<(ModuleId, Idx<bone_semantic::hir::DecoratorDef>)> = Vec::new();
    for module in ws.modules.iter() {
        for (did, d) in module.decorators.iter() {
            deco_map.insert(d.def_key, (module.id, did));
            if !skull_mir::definition::is_compiler_owned_decorator(d) {
                deco_items.push((module.id, did));
            }
        }
    }
    (deco_map, deco_items)
}

/// Decorator APPLICATION lines (`__deco_<name>({ name, func }, ...)`) for every
/// decorated fn workspace-wide -- registration calls only, no fn bodies. Shared
/// by `assemble` (full) and `assemble_overlay` (the incremental rebind that
/// re-points system registries at the current globals after a body edit).
fn decorator_application_lines(
    ws: &Workspace,
    deco_map: &DecoMap,
    scx: &skull_mir::lower::ScalarCtx,
    skipped: &mut Vec<(String, String)>,
) -> String {
    let mut deco_lines = String::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (_fid, f) in module.functions.iter() {
            if f.backend == bone_semantic::hir::FnBackend::Js {
                continue;
            }
            for ann in &f.annotations {
                let app_key = ann.resolved_name.unwrap_or(ann.name.name);
                let Some(&(dmid, did)) = deco_map.get(&app_key) else {
                    continue;
                };
                let d = &ws.module(dmid).decorators[did];
                if skull_mir::definition::is_compiler_owned_decorator(d) {
                    continue;
                }
                let identity =
                    skull_mir::code_identity::CodeIdentity::Named(Instance::base(FuncRef {
                        module: module.id,
                        func: _fid,
                    }));
                let key = identity.logical_key(ws, scx.interner());
                let target = format!(
                    "{{ name = {:?}, params = {{}}, func = __rock_code_slot_ref({:?}) }}",
                    f.name.as_str(),
                    key
                );
                match crate::decorator_call_args(ws, dmid, did, ann) {
                    Ok(args) => {
                        let mut call_args = vec![target];
                        call_args.extend(args);
                        let _ = writeln!(
                            deco_lines,
                            "{}({})",
                            crate::decorator_fn_name(app_key.as_str()),
                            call_args.join(", ")
                        );
                    }
                    Err(e) => skipped.push((f.name.as_str().to_string(), e.message)),
                }
            }
        }
    }
    deco_lines
}

/// Every `@sim`-annotated fn in the workspace, paired with its module file and
/// wire name (the `@sim("name")` string arg, else the fn name). The SINGLE
/// source of the `@sim` walk + wire derivation, shared by the codegen registry
/// ([`sim_registration_lines`]) and the rock-cli sim runner
/// (`collect_skull_sims`) so the registered name and the name the runner asks
/// to dispatch cannot drift.
pub fn collect_sim_fns(ws: &Workspace) -> Vec<(Ustr, &bone_semantic::hir::Function, String)> {
    let mut sims = Vec::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (_fid, f) in module.functions.iter() {
            let Some(wire) = skull_mir::lifecycle::sim_wire_name(f) else {
                continue;
            };
            sims.push((module.file, f, wire.as_str().to_string()));
        }
    }
    sims
}

/// `register_sim("<wire>", <mangled_fn>)` for every `@sim`-annotated fn,
/// workspace-wide -- the skull analog of rock-codegen's `sim_registrations.rs`.
/// Emitted at module-init time (after fn + global defs) so a skull-compiled game
/// registers its sims and the runtime's `ROCK_RUN_SIM=<wire>` dispatch resolves.
///
/// `register_sim` is itself a regular Rock fn (the host simulation registry), so it
/// is mangled the same way as its definition (every fn mangles by name +
/// signature, so definition and call agree). Every `@sim` implies the host engine
/// is linked, so a missing
/// `register_sim` is an internal error rather than a silent nil-call (mirrors
/// rock's discipline).
///
/// Full-emit only: sims run from a fresh `main.lua` process via `ROCK_RUN_SIM`,
/// never from the live dev game's overlay, so `assemble_overlay` needs none.
/// The engine's `register_sim`, which [`sim_registration_lines`] calls by MANGLED
/// name from generated top-level text. Shared with the reachability roots so the
/// registration and the root cannot drift: emitting the call while DCE prunes the
/// definition is a nil-call at load, which is exactly what happened.
///
/// Matched by DECLARED name: under modules v1 a function's name is its DefKey.
/// More than one candidate is ambiguous and yields `None`;
/// [`sim_registration_lines`] reports the candidates.
fn register_sim_fn(ws: &Workspace) -> Option<FuncRef> {
    match register_sim_candidates(ws).as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

fn register_sim_candidates(ws: &Workspace) -> Vec<FuncRef> {
    ws.modules
        .iter()
        .flat_map(|m| {
            m.functions
                .iter()
                .filter(|(_, f)| {
                    skull_mir::mangle::declared_name(f.name.as_str()) == "register_sim"
                })
                .map(|(fid, _)| FuncRef {
                    module: m.id,
                    func: fid,
                })
        })
        .collect()
}

/// The game's `@sim_entry` providers. A provider is how a game hands a sim its
/// entry argument while the engine's registry stays game-agnostic: a sim that
/// takes one parameter is registered as `provider(sim)`, and the provider
/// (a `co fn(body: fn(P) -> Coroutine<Wait>) -> Coroutine<Wait>`) resolves the
/// argument when the runner starts the sim and awaits the body with it.
fn sim_entry_providers(ws: &Workspace) -> Vec<FuncRef> {
    ws.modules
        .iter()
        .filter(|m| m.id != ModuleId::builtin())
        .flat_map(|m| {
            m.functions
                .iter()
                .filter(|(_, f)| f.annotations.iter().any(|a| a.is_builtin("sim_entry")))
                .map(|(fid, _)| FuncRef {
                    module: m.id,
                    func: fid,
                })
        })
        .collect()
}

/// Functions the generated sim registration text names: `register_sim` and the
/// `@sim_entry` providers. Empty when the build has no sims.
fn sim_glue_roots(ws: &Workspace) -> Vec<FuncRef> {
    if collect_sim_fns(ws).is_empty() {
        return Vec::new();
    }
    let mut roots: Vec<FuncRef> = register_sim_fn(ws).into_iter().collect();
    roots.extend(sim_entry_providers(ws));
    roots
}

/// Every function this build roots from, by the emitter's own predicate.
///
/// It lives in the DRIVER, not in `skull_graph`: which declarations a build
/// roots from is a production decision (project layout, decorators, `@sim`,
/// `@test`), and the graph takes roots as an INPUT precisely so it cannot make
/// that decision itself. See [`all_roots`], which adds the generated-glue
/// obligations on top.
pub fn emission_roots(ws: &Workspace, project_root: Option<&str>) -> Vec<FuncRef> {
    emission_roots_with_census(ws, project_root, None)
}

/// Harness roots plus the global roots every build keeps. Package tests and
/// selected sim receipts have no public interface to root wholesale.
fn explicit_and_global_roots(ws: &Workspace, explicit: &[FuncRef]) -> Vec<FuncRef> {
    let (deco_map, _) = build_deco_map(ws);
    let mut roots = explicit.to_vec();
    roots.extend(
        ws.modules
            .iter()
            .filter(|m| m.id != ModuleId::builtin())
            .flat_map(|m| {
                m.functions.iter().filter_map(|(fid, f)| {
                    is_global_emission_root(f, &deco_map).then_some(FuncRef {
                        module: m.id,
                        func: fid,
                    })
                })
            }),
    );
    roots.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
    roots.dedup();
    roots
}

fn sim_refs_by_wire(ws: &Workspace) -> FxHashMap<String, FuncRef> {
    ws.modules
        .iter()
        .flat_map(|m| {
            m.functions.iter().filter_map(|(fid, f)| {
                skull_mir::lifecycle::sim_wire_name(f).map(|wire| {
                    (
                        wire.as_str().to_owned(),
                        FuncRef {
                            module: m.id,
                            func: fid,
                        },
                    )
                })
            })
        })
        .collect()
}

fn named_sim_dependencies(
    ws: &Workspace,
    mid: ModuleId,
    owner: BodyOwner,
    body: &bone_semantic::hir::Body,
    sims: &FxHashMap<String, FuncRef>,
) -> Result<Vec<FuncRef>, CompilerError> {
    use bone_semantic::hir::Expr;
    let mut roots = Vec::new();
    for (_, expr) in body.expr_arena.iter() {
        if let Expr::String(name) = expr {
            if let Some(func) = sims.get(name.as_str()) {
                roots.push(*func);
            }
        }
    }
    // The engine dispatcher consumes the runner's resolved entry plan. Other
    // registry reads must name their dependencies statically.
    let engine_dispatch = match owner {
        BodyOwner::Function(fid) => matches!(
            ws.module(mid).functions[fid].name.as_str(),
            "stone::register_sim" | "stone::sim_start_entry" | "stone::clear_all_systems"
        ),
        _ => false,
    };
    if !engine_dispatch {
        for (id, expr) in body.expr_arena.iter() {
            let Expr::Global(name) = expr else { continue };
            if name.as_str() != "stone::SIMS" {
                continue;
            }
            let literal_lookup = body.expr_arena.iter().any(|(_, expr)| {
                let key = match expr {
                    Expr::Index { base, index } if *base == id => Some(*index),
                    Expr::Call { args, family, .. }
                        if skull_mir::mangle::declared_name(family.key().as_str()) == "get" =>
                    {
                        let mut args = *args;
                        if args.next() == Some(id) {
                            args.next()
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                key.is_some_and(|key| matches!(body.expr_arena[key], Expr::String(_)))
            });
            if !literal_lookup {
                return Err(CompilerError::new_with_label(body.expr_spans[id],
                    "scoped sim compile cannot resolve dynamic SIMS access; select the whole suite or use a literal sim name".to_string()));
            }
        }
    }
    Ok(roots)
}

fn emission_roots_with_census(
    ws: &Workspace,
    project_root: Option<&str>,
    mut census: Option<&mut RootEnumerationCensus>,
) -> Vec<FuncRef> {
    let (deco_map, _) = build_deco_map(ws);
    let mut roots = Vec::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        if let Some(census) = census.as_deref_mut() {
            census.modules_examined += 1;
            census.functions_examined += module.functions.len();
        }
        for (fid, f) in module.functions.iter() {
            if is_emission_root(module.file.as_str(), project_root, f, &deco_map) {
                roots.push(FuncRef {
                    module: module.id,
                    func: fid,
                });
            }
        }
    }
    roots
}

/// The roots one Lua build is taken over: the declared emission roots plus the
/// obligations GENERATED GLUE TEXT carries.
///
/// `sim_registration_lines` writes `register_sim("<wire>", <mangled>)` and
/// `shader_registration_lines` writes a program whose stage entries and GLSL
/// helpers are named by its graph-owned view. Neither is a Rock call site, so
/// no amount of ordinary graph reachability can find them -- they are an
/// artifact ROOT (plan S 11: "the target may change roots"), which is what
/// `ArtifactRequest::roots` is for. Emitting the generated call while the
/// definition is pruned is a nil call at load, which is exactly what happened.
///
/// The shader collection is therefore built BEFORE the graph, not beside the
/// assembly that renders it.
#[cfg(test)]
fn all_roots(ws: &Workspace, project_root: Option<&str>, shader_roots: &[FuncRef]) -> Vec<FuncRef> {
    all_roots_with_census(ws, project_root, shader_roots, None)
}

fn all_roots_with_census(
    ws: &Workspace,
    project_root: Option<&str>,
    shader_roots: &[FuncRef],
    mut census: Option<&mut RootEnumerationCensus>,
) -> Vec<FuncRef> {
    let mut roots = emission_roots_with_census(ws, project_root, census.as_deref_mut());
    let mut seen: FxHashSet<FuncRef> = roots.iter().copied().collect();
    let mut glue: Vec<FuncRef> = sim_glue_roots(ws);
    glue.extend(shader_roots.iter().copied());
    // Deterministic: `dependencies` is a hash set, so its iteration order is not.
    glue.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
    roots.extend(glue.into_iter().filter(|f| seen.insert(*f)));
    if let Some(census) = census {
        census.roots_reseeded += roots.len();
    }
    roots
}

/// Compose the retained root set from facts cached by full construction and
/// explicit body-local shader deltas. Unlike [`all_roots`], this never asks the
/// workspace for its declarations or sim functions.
fn cached_roots(
    interface_roots: &[FuncRef],
    sim_roots: &[FuncRef],
    shader_roots: &[FuncRef],
) -> Vec<FuncRef> {
    let mut roots = interface_roots.to_vec();
    let mut seen: FxHashSet<FuncRef> = roots.iter().copied().collect();
    let mut glue = sim_roots.to_vec();
    glue.extend(shader_roots.iter().copied());
    glue.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
    roots.extend(glue.into_iter().filter(|f| seen.insert(*f)));
    roots
}

#[derive(Default)]
struct RootEnumerationCensus {
    modules_examined: usize,
    functions_examined: usize,
    roots_reseeded: usize,
}

fn sim_registration_lines(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    selected: Option<&FxHashSet<Ustr>>,
) -> Result<String, CompilerError> {
    let sims = collect_sim_fns(ws)
        .into_iter()
        .filter(|(_, f, _)| selected.is_none_or(|names| names.contains(&f.name)))
        .collect::<Vec<_>>();
    if sims.is_empty() {
        return Ok(String::new());
    }

    let register_fn = register_sim_fn(ws).ok_or_else(|| {
        let candidates = register_sim_candidates(ws);
        if candidates.len() > 1 {
            let names: Vec<&str> = candidates
                .iter()
                .map(|f| ws.module(f.module).functions[f.func].name.as_str())
                .collect();
            return CompilerError::new_no_location(format!(
                "`@sim` registration is ambiguous: `register_sim` is defined as {}",
                names.join(", ")
            ));
        }
        CompilerError::new_no_location(
            "internal: `register_sim` is not in scope yet `@sim` annotations exist \
             (skull) -- stone/engine must be linked. Please report."
                .to_string(),
        )
    })?;
    let register_name = mangle_fn(
        ws,
        &ws.module(register_fn.module).functions[register_fn.func],
    );

    let mut out = String::new();
    for (_file, f, wire) in sims {
        if f.backend == bone_semantic::hir::FnBackend::Js {
            continue;
        }
        let fref = ws
            .modules
            .iter()
            .find_map(|module| {
                module.functions.iter().find_map(|(fid, candidate)| {
                    std::ptr::eq(candidate, f).then_some(FuncRef {
                        module: module.id,
                        func: fid,
                    })
                })
            })
            .ok_or_else(|| {
                CompilerError::new_no_location(format!("cannot locate @sim `{}`", f.name))
            })?;
        let identity = skull_mir::code_identity::CodeIdentity::Named(Instance::base(fref));
        let key = identity.logical_key(ws, scx.interner());
        let body = format!("__rock_code_slot_ref({:?})", key);
        let entry = match f.param_decls.len() {
            0 => body,
            1 => {
                let provider = sim_entry_provider_for(ws, f, &wire)?;
                // The provider is called when the runner STARTS the sim, so the
                // argument is resolved against the world live at that moment.
                format!("function() return {}({}) end", provider, body)
            }
            n => {
                return Err(CompilerError::new_with_label(
                    f.span,
                    format!(
                        "`@sim` `{wire}` takes {n} parameters; a sim takes none or one entry \
                         argument supplied by the game's `@sim_entry` provider"
                    ),
                ));
            }
        };
        let _ = writeln!(out, "{}({:?}, {})", register_name, wire, entry);
    }
    Ok(out)
}

/// The mangled name of the `@sim_entry` provider that supplies `sim`'s single
/// parameter. The provider must be unique and its `body` parameter must take
/// exactly the sim's parameter type, so a sim cannot be started with an
/// argument of the wrong type.
fn sim_entry_provider_for(
    ws: &Workspace,
    sim: &bone_semantic::hir::Function,
    wire: &str,
) -> Result<String, CompilerError> {
    use skull_mir::monoty::{MonoSubst, MonoTy, convert_decl};
    let providers = sim_entry_providers(ws);
    let provider_ref = match providers.as_slice() {
        [only] => *only,
        [] => {
            return Err(CompilerError::new_with_label(
                sim.span,
                format!(
                    "`@sim` `{wire}` takes an entry argument but no `@sim_entry` provider is \
                     defined; the game declares one \
                     `@sim_entry co fn(body: fn(T) -> Coroutine<Wait>) -> Coroutine<Wait>`"
                ),
            ));
        }
        many => {
            let names: Vec<&str> = many
                .iter()
                .map(|f| ws.module(f.module).functions[f.func].name.as_str())
                .collect();
            return Err(CompilerError::new_no_location(format!(
                "`@sim_entry` is ambiguous: {} are all providers",
                names.join(", ")
            )));
        }
    };
    let provider = &ws.module(provider_ref.module).functions[provider_ref.func];
    let subst = MonoSubst::default();
    let sim_param = convert_decl(ws, sim.param_decls[0].ty, &subst).ok();
    let provider_param = match provider.param_decls.as_slice() {
        [only] => convert_decl(ws, only.ty, &subst).ok(),
        _ => None,
    };
    let accepted = match &provider_param {
        Some(MonoTy::Function { params, .. }) if params.len() == 1 => Some(&params[0]),
        _ => None,
    };
    match (accepted, &sim_param) {
        (Some(accepted), Some(sim_param)) if accepted == sim_param => {}
        _ => {
            return Err(CompilerError::new_with_label(
                sim.span,
                format!(
                    "`@sim` `{wire}` takes `{}`, but the `@sim_entry` provider `{}` supplies `{}`",
                    sim_param
                        .as_ref()
                        .map_or("<unresolved>".to_string(), MonoTy::render),
                    provider.name,
                    accepted.map_or("<not a one-argument body>".to_string(), MonoTy::render),
                ),
            ));
        }
    }
    Ok(mangle_fn(ws, provider))
}

/// A Lua long-bracket string literal (`[==[ ... ]==]`) holding `s`, at the
/// smallest `=` level whose closing bracket does not occur in `s` (never does
/// for generated GLSL, but the guard keeps a `]]` in shader source from
/// terminating early). The leading newline is elided by Lua's long-string rule.
fn lua_long_bracket(s: &str) -> String {
    let mut level = 0usize;
    loop {
        let close = format!("]{}]", "=".repeat(level));
        if !s.contains(&close) {
            let eqs = "=".repeat(level);
            return format!("[{eqs}[\n{s}]{eqs}]");
        }
        level += 1;
    }
}

fn shader_programs(
    collection: &skull_mir::lower::ShaderCollection,
) -> Vec<crate::shader_artifact::ShaderProgram> {
    collection
        .pairs
        .iter()
        .map(|pair| crate::shader_artifact::ShaderProgram {
            vert: pair.vert,
            frag: pair.frag,
        })
        .collect()
}

fn compute_programs(
    collection: &skull_mir::lower::ComputeCollection,
) -> Vec<crate::shader_artifact::ComputeProgram> {
    collection
        .entries
        .iter()
        .map(|entry| crate::shader_artifact::ComputeProgram { entry: entry.entry })
        .collect()
}

/// Every function that lies on a call cycle, read off the whole-program instance
/// graph and projected onto declarations.
///
/// Consumed only by `ROCK_SKULL_TAIL_CALLS=cycles`, which keeps the
/// `do return f(...) end` shape exactly at these callees so an unbounded
/// recursion still gets tail-call elimination. Projecting instances onto their
/// declaration MERGES nodes, which can only add cycles, never remove one, so a
/// function that really recurses is never missed. Every edge kind counts,
/// `EdgeKind::FnValue` included: a function taken as a value is reached by a
/// call the direct-call edges cannot see, and its taker's edge is what closes
/// the cycle through it.
/// Every function's call-graph strongly connected component, from the same
/// whole-program instance graph [`recursive_callees`] reads, projected onto
/// declarations the same way. Projecting MERGES nodes, which can only merge
/// components, so a callee this reports as separate from its caller really is:
/// the binding pass never orders a capture after the closure that needs it.
fn call_sccs(
    graph: &crate::instance_graph::InstanceGraph,
) -> bone_semantic::prelude::FxHashMap<skull_mir::mir::FuncRef, u32> {
    skull_codegen_lua::call_sccs(
        graph
            .contributions
            .values()
            .flat_map(|contribution| contribution.edges.iter())
            .map(|edge| (edge.from.func, edge.to.func)),
    )
}

fn recursive_callees(
    graph: &crate::instance_graph::InstanceGraph,
) -> FxHashSet<skull_mir::mir::FuncRef> {
    skull_codegen_lua::callees_on_cycles(
        graph
            .contributions
            .values()
            .flat_map(|contribution| contribution.edges.iter())
            .map(|edge| (edge.from.func, edge.to.func)),
    )
}

fn realize_shader_view(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    programs: &[crate::shader_artifact::ShaderProgram],
    compute: &[crate::shader_artifact::ComputeProgram],
    scx: &skull_mir::lower::ScalarCtx,
    plans: std::sync::Arc<skull_mir::concrete::ConcretePlans>,
) -> Result<crate::shader_artifact::ShaderArtifactView, CompilerError> {
    crate::shader_artifact::realize_shader_with_context_and_compute(
        ws,
        graph,
        &crate::shader_artifact::ShaderArtifactRequest {
            spec: crate::target_projection::TargetSpec {
                shader: Some(crate::target_projection::ShaderTargetSpec::Glsl(
                    skull_codegen_glsl::GlslProfile::Core330,
                )),
                ..crate::target_projection::LUAJIT_DESKTOP
            },
            programs,
        },
        scx,
        compute,
        Some(plans),
    )
    .ok_or_else(|| CompilerError::new_no_location("GLSL shader view is unavailable".to_string()))
}

fn graph_rejection_error(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
) -> Option<CompilerError> {
    let mut callers: Option<
        FxHashMap<&crate::instance_graph::Instance, Vec<&crate::instance_graph::Edge>>,
    > = None;
    for rejection in graph.rejections() {
        let caller = &ws.module(rejection.at.func.module).functions[rejection.at.func.func];
        if rejection.kind == crate::instance_graph::RejectionKind::IdentityInOrdinaryStorage {
            // A language rejection, not a totality gap: fatal in ablation runs too.
            let callers = callers.get_or_insert_with(|| {
                let mut map: FxHashMap<_, Vec<_>> = FxHashMap::default();
                for edge in graph.edges() {
                    map.entry(&edge.to).or_default().push(edge);
                }
                map
            });
            if let Some(err) = identity_instance_rejection_error(ws, graph, callers, rejection) {
                return Some(err);
            }
            continue;
        }
        if skull_mir::ablation::enabled() {
            let category = match rejection.kind {
                crate::instance_graph::RejectionKind::ChangingInstanceRecursion => {
                    skull_mir::ablation::FatalCategory::PolymorphicGrowth
                }
                crate::instance_graph::RejectionKind::GenericFnValue
                | crate::instance_graph::RejectionKind::UnconstrainedTypeVar => {
                    skull_mir::ablation::FatalCategory::MissingGraphEdge
                }
                crate::instance_graph::RejectionKind::ConcreteTypeUnavailable => {
                    skull_mir::ablation::FatalCategory::CompilerInvariant
                }
                crate::instance_graph::RejectionKind::IdentityInOrdinaryStorage => {
                    unreachable!("identity rejections are handled before ablation")
                }
            };
            skull_mir::ablation::note_examined();
            skull_mir::ablation::record(
                category,
                if rejection.kind == crate::instance_graph::RejectionKind::ChangingInstanceRecursion
                {
                    caller.name.as_str().to_string()
                } else {
                    rejection
                        .message
                        .split('`')
                        .nth(1)
                        .unwrap_or(caller.name.as_str())
                        .to_string()
                },
                caller.name.as_str(),
                rejection.span,
                rejection.message.clone(),
            );
            continue;
        }
        if is_stdlib_module_file(ws.module(rejection.at.func.module).file.as_str()) {
            continue;
        }
        return Some(CompilerError::new_with_label(
            rejection.span,
            rejection.message.clone(),
        ));
    }
    None
}

/// An instance's identity-storage rejection belongs to the package whose call
/// site instantiated it. The walk goes back
/// from the instance through its callers; a caller in an exempt package whose
/// own type arguments carry an identity only forwarded it, so its callers are
/// asked in turn. Any other caller introduced the identity, and the rejection is
/// fatal when that caller's package is not exempt.
fn identity_instance_rejection_error(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    callers: &FxHashMap<&crate::instance_graph::Instance, Vec<&crate::instance_graph::Edge>>,
    rejection: &crate::instance_graph::Rejection,
) -> Option<CompilerError> {
    let exempt = |instance: &crate::instance_graph::Instance| {
        bone_semantic::identity_storage::identity_storage_exempt(
            ws,
            ws.module(instance.func.module),
        )
    };
    let mut seen = FxHashSet::default();
    let mut work = vec![&rejection.at];
    let mut reached_by_edge = false;
    while let Some(instance) = work.pop() {
        if !seen.insert(instance) {
            continue;
        }
        for edge in callers.get(instance).into_iter().flatten() {
            reached_by_edge = true;
            let forwarded = exempt(&edge.from)
                && edge.from.args.iter().any(|arg| {
                    skull_graph::identity_storage::stores_identity(ws, &graph.interner().get(*arg))
                });
            if forwarded {
                work.push(&edge.from);
            } else if !exempt(&edge.from) {
                let callee = &ws.module(rejection.at.func.module).functions[rejection.at.func.func];
                return Some(
                    CompilerError::new_with_label(rejection.span, rejection.message.clone())
                        .with_label(
                            edge.span,
                            format!(
                                "`{}` is instantiated from here with an identity struct",
                                callee.name
                            ),
                        ),
                );
            }
        }
    }
    // An instance no edge reaches is rooted by an always-emitted body; the
    // instance's own package answers for it.
    (!reached_by_edge && !exempt(&rejection.at))
        .then(|| CompilerError::new_with_label(rejection.span, rejection.message.clone()))
}

/// `__rock_shader_register("<key>", <glsl>)` for every distinct `rock_shader`
/// pair. Emitted at module-init time after
/// the `@sim` registrations, at chunk top level, so a reload re-`dofile`s them
/// with the new source (the hot-reload-parity invariant). Deterministic
/// sorted-by-key order (from `collect_shader_pairs`) keeps re-emission
/// byte-identical. The driver realizes the graph-owned shader view and passes
/// it to GLSL; skull-mir still surfaces only the resolved `FuncRef`s. Selected
/// WGPU projects every graph pair independently: GL registration remains
/// unconditional, while a pair whose WGSL projection cannot represent its
/// graph-owned interface is omitted from the WGPU artifact view and rejected
/// by the runtime before effects.
#[cfg(feature = "renderer-wgpu")]
fn concrete_stage_input(
    view: &skull_graph::shader::ShaderStageView,
    concrete_plans: &std::sync::Arc<skull_mir::concrete::ConcretePlans>,
) -> skull_codegen_shader::ConcreteStageInput {
    skull_codegen_shader::ConcreteStageInput {
        stage: view.stage,
        entry: view.entry.clone(),
        retained: view.retained.iter().cloned().collect(),
        helpers: view.helpers.iter().cloned().collect(),
        builtins: view.builtins.iter().cloned().collect(),
        order: view.order.clone(),
        dependencies: view.dependencies.clone(),
        concrete_plans: std::sync::Arc::clone(concrete_plans),
    }
}

fn shader_registration_lines(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    collection: &skull_mir::lower::ShaderCollection,
    compute_collection: &skull_mir::lower::ComputeCollection,
    view: &crate::shader_artifact::ShaderArtifactView,
    renderer_target: RendererTarget,
) -> Result<ShaderRegistrations, CompilerError> {
    let profile = skull_codegen_glsl::GlslProfile::Core330;
    if view.profile != profile || view.program_views.len() != collection.pairs.len() {
        return Err(CompilerError::new_no_location(
            "shader graph/view registration denominator differs from shader collection".to_string(),
        ));
    }
    let mut out = String::new();
    let mut sources = Vec::new();
    let mut dependencies = FxHashSet::default();
    #[cfg(feature = "renderer-wgpu")]
    let mut abi_wires = BTreeMap::new();
    #[cfg(not(feature = "renderer-wgpu"))]
    let abi_wires = BTreeMap::new();
    for (pair, program_view) in collection.pairs.iter().zip(&view.program_views) {
        // vert=None is the A3c single-arg fullscreen form -> the A1 single-entry
        // emitter (PIXEL body + fullscreen VERTEX passthrough).
        let emission = match pair.vert {
            Some(_) => {
                skull_codegen_glsl::emit_shader_pipeline_with_view(ws, scx, program_view, profile)?
            }
            None => {
                skull_codegen_glsl::emit_shader_entry_with_view(ws, scx, program_view, profile)?
            }
        };
        dependencies.extend(emission.dependencies);
        sources.push(ShaderSource {
            key: pair.key.clone(),
            language: ShaderLanguage::Glsl,
            source: emission.source.clone(),
        });
        // Slot order is the engine's whole view of texture binding: it points
        // each sampler at its unit once at link time, then binds by slot.
        let slot_names: Vec<&str> = emission
            .abi
            .texture_units
            .iter()
            .map(|n| n.as_str())
            .collect();
        // The `@uniform` blocks' std140 layouts, in binding order: A1 has at
        // most one, an A2 pipeline has one per stage. Carried as PARALLEL FLAT
        // lists -- one entry per block, plus the members concatenated with a
        // per-block count -- so a hand-written driver never nests. A program
        // with no group sends empty lists, which are still arguments; a short
        // call is what rock-cli/tests/shader_register_arity_gate.rs catches.
        let w = skull_codegen_glsl::BlockWire::of(
            emission.abi.uniform_blocks.iter(),
            emission.abi.uniform_array_blocks.iter(),
        );
        let fullscreen = pair.vert.is_none();
        let semantic_wire = emission.interface.semantic_wire(fullscreen);
        let _ = writeln!(
            out,
            "__rock_shader_register({:?}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            pair.key,
            lua_long_bracket(&emission.source),
            skull_codegen_lua::lua_string_vec(&slot_names),
            skull_codegen_lua::lua_string_vec(&w.names),
            skull_codegen_lua::lua_int_vec(&w.bindings),
            skull_codegen_lua::lua_int_vec(&w.sizes),
            skull_codegen_lua::lua_int_vec(&w.fingerprints),
            skull_codegen_lua::lua_int_vec(&w.counts),
            skull_codegen_lua::lua_string_vec(&w.member_names),
            skull_codegen_lua::lua_int_vec(&w.member_offsets),
            skull_codegen_lua::lua_int_vec(&w.member_kinds),
            skull_codegen_lua::lua_int_vec(&w.member_capacities),
            skull_codegen_lua::lua_int_vec(&w.member_strides),
            fullscreen,
            lua_long_bracket(&semantic_wire),
        );
    }
    let roots: Vec<FuncRef> = collection
        .pairs
        .iter()
        .flat_map(|pair| pair.vert.into_iter().chain(std::iter::once(pair.frag)))
        .collect();
    let keys = collection
        .pairs
        .iter()
        .map(|pair| pair.key.clone())
        .chain(
            compute_collection
                .entries
                .iter()
                .map(|entry| entry.key.clone()),
        )
        .collect();
    #[cfg(feature = "renderer-wgpu")]
    let mut wgpu_projection_census = None;
    #[cfg(not(feature = "renderer-wgpu"))]
    let wgpu_projection_census = None;
    if renderer_target == RendererTarget::Wgpu {
        #[cfg(not(feature = "renderer-wgpu"))]
        return Err(CompilerError::new_no_location(
            "the `wgpu` renderer was omitted from this build; enable the `renderer-wgpu` feature (rebuild with `cargo build -p rock-cli --bin rock --features renderer-wgpu`)".to_string(),
        ));
        #[cfg(feature = "renderer-wgpu")]
        {
            let items =
                collection
                    .pairs
                    .iter()
                    .zip(&view.program_views)
                    .map(|(pair, program_view)| {
                        (pair.key.clone(), (pair.vert.is_none(), program_view))
                    });
            let (artifacts, census) =
                collect_wgpu_projections(items, |key, (fullscreen, program_view)| {
                    (|| -> Result<(String, String, String, Vec<FuncRef>), CompilerError> {
                        if fullscreen {
                            let stage_view = program_view
                                .stages
                                .iter()
                                .find(|stage| {
                                    stage.stage == skull_mir::shader_ir::ShaderStage::Fragment
                                })
                                .ok_or_else(|| {
                                    CompilerError::new_no_location(format!(
                                        "wgsl artifact {key} has no fragment stage view"
                                    ))
                                })?;
                            let input =
                                concrete_stage_input(stage_view, &program_view.concrete_plans);
                            let neutral =
                                skull_codegen_shader::plan_fullscreen_fragment_from_concrete(
                                    ws,
                                    scx,
                                    &input,
                                    &skull_codegen_wgsl::WgslShaderPolicy,
                                )?;
                            let abi = skull_codegen_wgsl::WgslAbiPlan::wgsl(
                                &neutral.interface,
                                bone_semantic::prelude::Span::default(),
                            )?;
                            let emission = skull_codegen_wgsl::emit_fullscreen_shader(
                                ws,
                                &neutral.ir,
                                &neutral.interface,
                                &abi,
                                bone_semantic::prelude::Span::default(),
                            )?;
                            Ok((
                                emission.source,
                                emission.abi.wire()?,
                                neutral.interface.semantic_wire(true),
                                emission.dependencies,
                            ))
                        } else {
                            let glsl = skull_codegen_glsl::emit_shader_pipeline_with_view(
                                ws,
                                scx,
                                program_view,
                                profile,
                            )?;
                            let abi = skull_codegen_wgsl::WgslAbiPlan::explicit(
                                &glsl.interface,
                                "vertex_main",
                                "fragment_main",
                                bone_semantic::prelude::Span::default(),
                            )?;
                            let emission = skull_codegen_wgsl::emit_explicit_shader(
                                ws,
                                &glsl.ir,
                                &glsl.interface,
                                &abi,
                                bone_semantic::prelude::Span::default(),
                            )?;
                            Ok((
                                emission.source,
                                emission.abi.wire()?,
                                glsl.interface.semantic_wire(false),
                                emission.dependencies,
                            ))
                        }
                    })()
                })?;
            for (shader_key, (source, wire, semantic_wire, emission_dependencies)) in artifacts {
                dependencies.extend(emission_dependencies);
                abi_wires.insert(shader_key.clone(), wire.clone());
                sources.push(ShaderSource {
                    key: shader_key.clone(),
                    language: ShaderLanguage::Wgsl,
                    source: source.clone(),
                });
                let _ = writeln!(
                    out,
                    "__rock_wgpu_shader_register({}, {}, {}, {})",
                    skull_codegen_lua::lua_string(&shader_key),
                    skull_codegen_lua::lua_string(&source),
                    skull_codegen_lua::lua_string(&wire),
                    skull_codegen_lua::lua_string(&semantic_wire),
                );
            }
            wgpu_projection_census = Some(census);
        }
    }
    if view.compute_views.len() != compute_collection.entries.len() {
        return Err(CompilerError::new_no_location(format!(
            "compute graph/view registration denominator differs: {} views for {} roots",
            view.compute_views.len(),
            compute_collection.entries.len()
        )));
    }
    let has_compute = !compute_collection.entries.is_empty();
    if renderer_target == RendererTarget::Wgpu && has_compute {
        #[cfg(not(feature = "renderer-wgpu"))]
        return Err(CompilerError::new_no_location(
            "the `wgpu` renderer was omitted from this build; enable the `renderer-wgpu` feature (rebuild with `cargo build -p rock-cli --bin rock --features renderer-wgpu`)".to_string(),
        ));
        #[cfg(feature = "renderer-wgpu")]
        {
            out.push_str("__rock_wgpu_compute_snapshot_begin()\n");
            for (entry, compute_view) in compute_collection.entries.iter().zip(&view.compute_views)
            {
                let input = concrete_stage_input(&compute_view.stage, &compute_view.concrete_plans);
                let neutral = skull_codegen_shader::plan_compute_from_concrete(
                    ws,
                    scx,
                    &input,
                    &skull_codegen_wgsl::WgslShaderPolicy,
                )?;
                dependencies.extend(neutral.dependencies);
                let logical_entry = neutral.entry.inst.symbol(ws, &neutral.ir.interner);
                let abi = skull_codegen_wgsl::WgslAbiPlan::compute(
                    &neutral.interface,
                    &format!("{logical_entry}_compute"),
                    bone_semantic::prelude::Span::default(),
                )?;
                let emission = skull_codegen_wgsl::emit_compute_shader(
                    ws,
                    &neutral.ir,
                    &neutral.interface,
                    &abi,
                    bone_semantic::prelude::Span::default(),
                )?;
                dependencies.extend(emission.dependencies);
                let wire = emission.abi.wire()?;
                abi_wires.insert(entry.key.clone(), wire.clone());
                sources.push(ShaderSource {
                    key: entry.key.clone(),
                    language: ShaderLanguage::Wgsl,
                    source: emission.source.clone(),
                });
                let _ = writeln!(
                    out,
                    "__rock_wgpu_compute_register({}, {}, {}, {})",
                    skull_codegen_lua::lua_string(&entry.key),
                    skull_codegen_lua::lua_string(&emission.source),
                    skull_codegen_lua::lua_string(&wire),
                    skull_codegen_lua::lua_string(&neutral.interface.semantic_wire(false)),
                );
            }
            out.push_str("if not __rock_wgpu_compute_snapshot_end() then error(\"compiler compute snapshot failed\") end\n");
        }
    }
    let mut shader_pair_users = BTreeMap::new();
    for pairs in collection.contributions.values() {
        for pair in pairs {
            *shader_pair_users.entry(pair.key.clone()).or_insert(0) += 1;
        }
    }
    let shader_pairs = collection
        .pairs
        .iter()
        .map(|pair| (pair.key.clone(), pair.clone()))
        .collect();
    let mut compute_entry_users = BTreeMap::new();
    for entries in compute_collection.contributions.values() {
        for entry in entries {
            *compute_entry_users.entry(entry.key.clone()).or_insert(0) += 1;
        }
    }
    let compute_entries = compute_collection
        .entries
        .iter()
        .map(|entry| (entry.key.clone(), entry.clone()))
        .collect();
    let shader_root_set = roots
        .iter()
        .chain(compute_collection.entries.iter().map(|entry| &entry.entry))
        .copied()
        .collect();
    Ok(ShaderRegistrations {
        lua: out,
        sources: sources.into(),
        dependencies,
        callsites: collection
            .callsites
            .iter()
            .copied()
            .chain(compute_collection.callsites.iter().copied())
            .collect(),
        shader_contributions: collection.contributions.clone(),
        compute_contributions: compute_collection.contributions.clone(),
        shader_pair_users,
        shader_pairs,
        compute_entry_users,
        compute_entries,
        shader_root_set,
        roots: roots
            .into_iter()
            .chain(compute_collection.entries.iter().map(|entry| entry.entry))
            .collect(),
        keys,
        abi_wires,
        wgpu_projection_census,
    })
}

/// Comptime reads made by ALWAYS-EMITTED units, which have no Chunk to carry
/// them: per-global (keyed so the overlay can re-init exactly the stale one)
/// plus decorator definitions (files only -- decorators re-apply on full
/// rebuild, so the watch set is all they need).
#[derive(Clone, Default)]
pub struct UnitComptimeReads {
    globals: FxHashMap<
        (ModuleId, Idx<bone_semantic::hir::GlobalDef>),
        Vec<skull_mir::mir::ComptimeRead>,
    >,
    decorators: Vec<skull_mir::mir::ComptimeRead>,
}

fn renderer_fingerprint_tag(target: RendererTarget) -> &'static str {
    match target {
        RendererTarget::Gl => "gl",
        RendererTarget::Wgpu => "wgpu",
    }
}

fn artifact_comptime_reads(
    cache: &SkullEmitCache,
    built_specs: &[(Instance, Chunk)],
    unit_reads: &UnitComptimeReads,
    replace_all_units: bool,
) -> Vec<skull_mir::mir::ComptimeRead> {
    let mut reads = Vec::new();
    for chunk in cache.chunks.values().flatten() {
        reads.extend(chunk.comptime_reads.iter().cloned());
    }
    for (_, chunk) in built_specs {
        reads.extend(chunk.comptime_reads.iter().cloned());
    }
    if replace_all_units {
        reads.extend(unit_reads.globals.values().flatten().cloned());
        reads.extend(unit_reads.decorators.iter().cloned());
    } else {
        for (key, old_reads) in &cache.unit_comptime_reads.globals {
            if !unit_reads.globals.contains_key(key) {
                reads.extend(old_reads.iter().cloned());
            }
        }
        reads.extend(unit_reads.globals.values().flatten().cloned());
        if unit_reads.decorators.is_empty() {
            reads.extend(cache.unit_comptime_reads.decorators.iter().cloned());
        } else {
            reads.extend(unit_reads.decorators.iter().cloned());
        }
    }
    reads
}

fn artifact_fingerprint(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    artifact: &skull_mir::code_identity::CodeArtifact,
    lifecycle: &skull_mir::lifecycle::LifecyclePlan,
    definitions: &skull_mir::definition::DefinitionPlan,
    global_layout: &str,
    reads: &[skull_mir::mir::ComptimeRead],
    renderer_target: RendererTarget,
) -> Result<String, CompilerError> {
    crate::artifact_fingerprint::compute(
        ws,
        scx.interner(),
        artifact,
        lifecycle,
        definitions,
        global_layout,
        reads,
        &ws.comptime_inputs.volatile_vars,
        renderer_fingerprint_tag(renderer_target),
    )
    .map_err(|error| {
        CompilerError::new_no_location(format!("Artifact fingerprint generation failed: {error}"))
    })
}

fn assemble(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    built_specs: &mut Vec<(Instance, Chunk)>,
    lifetime: CacheLifetime,
) -> Result<(MirEmit, FxHashSet<Instance>, UnitComptimeReads), CompilerError> {
    let cg = &cache.cg;
    let scx = &cache.scx;
    let mut release_stats = CompileReleaseStats {
        retained_dependency_edges: cache.dependents.values().map(|keys| keys.len()).sum(),
        retained_static_keys: cache.static_key_baseline.len(),
        ..CompileReleaseStats::default()
    };
    let mut compile_counts = CompileCounts {
        concrete_plans: cache.concrete.plans.len() + cache.concrete.always_plans.len(),
        chunks: cache.chunks.len(),
        dependency_edges: None,
    };
    // Per-stage lower/cleanup/emit/regalloc times: chunk times (captured during the
    // parallel `emit_full`/`emit_incremental` chunk build) summed in as chunks are
    // pulled, plus the decorator/global units re-emitted here. Under parallel emit
    // these are summed per-unit elapsed time across workers (so they exceed the wall-clock emit
    // total `backend-stats` measures). `t_scaffold` is the directly-timed
    // SEQUENTIAL remainder (prelude/value-structs/type-tables + global-init
    // ordering + deco application).
    let mut times = PhaseTimes::default();
    let mut attribution = AttributionAccumulator {
        census_enabled: cache.inline_cache.census_enabled(),
        ..AttributionAccumulator::default()
    };
    if let Some(table) = cache.inline_cache.candidate_table() {
        let stats = table.stats();
        let inline = &mut attribution.result.inline;
        inline.candidate_setup_keys = stats.keys;
        inline.candidate_setup_warmed = stats.warmed;
        inline.candidate_setup_reused = stats.reused;
        inline.candidate_setup_eligible = stats.eligible;
        inline.candidate_setup_rejected = stats.rejected;
        inline.candidate_setup_lower_ns = stats.lower_ns;
        inline.candidate_setup_cleanup_ns = stats.cleanup_ns;
        inline.candidate_setup_eligibility_ns = stats.eligibility_ns;
        inline.candidate_setup_worker_derivations = stats.worker_derivations.clone();
        inline.candidate_setup_rejection_reasons = stats.rejection_reasons.clone();
        if attribution.census_enabled {
            attribution.candidate_published_keys.extend(table.keys());
            let population = &mut inline.population;
            for (key, profile) in &stats.profiles {
                population.candidate_lower_attempted += usize::from(profile.lower_attempted);
                population.candidate_lowered_keys += usize::from(profile.lowered);
                if profile.eligible {
                    attribution.candidate_eligible_keys.insert(key.clone());
                }
                population.candidate_rejected_before_lower +=
                    usize::from(profile.rejected_before_lower);
                population.candidate_full_clone_ns += profile.full_clone_ns;
                population.candidate_compact_ns += profile.compact_ns;
                population.candidate_raw_instructions += profile.raw_instructions;
                population.candidate_raw_cells += profile.raw_cells;
                population.candidate_raw_blocks += profile.raw_blocks;
                population.candidate_cleaned_instructions += profile.cleaned_instructions;
                population.candidate_cleaned_cells += profile.cleaned_cells;
                population.candidate_cleaned_blocks += profile.cleaned_blocks;
                population.candidate_full_clone_capacity.0 += profile.full_clone_inst_capacity;
                population.candidate_full_clone_capacity.1 += profile.full_clone_cell_capacity;
                population.candidate_full_clone_capacity.2 += profile.full_clone_block_capacity;
                population.candidate_compact_capacity.0 += profile.compact_inst_capacity;
                population.candidate_compact_capacity.1 += profile.compact_cell_capacity;
                population.candidate_compact_capacity.2 += profile.compact_block_capacity;
            }
            for (key, lower_ns) in &stats.lower_ns_by_key {
                attribution
                    .candidate_lower_ns_by_key
                    .entry(key.clone())
                    .or_default()
                    .push(*lower_ns);
            }
        }
    }
    let mut t_scaffold = Duration::ZERO;
    let mut needed_specs: Vec<skull_mir::bound::SpecDemand> = Vec::new();
    let mut rejections = Rejections::default();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut direct_globals: FxHashMap<Ustr, Vec<Ustr>> = FxHashMap::default();
    let mut direct_calls: FxHashMap<Ustr, Vec<Ustr>> = FxHashMap::default();
    let mut all_statics: Vec<skull_mir::mir::StaticCell> = Vec::new();
    let mut code_artifact = skull_mir::code_identity::CodeArtifact::default();
    let mut allocation_bodies = Vec::new();

    // IR-agnostic load-time scaffolding (shared with the old backend).
    let ts = Instant::now();
    let mut out = String::from(crate::runtime_prelude());
    if crate::allocation_report::activation_enabled() {
        out.push_str(skull_codegen_lua::ALLOCATION_ACTIVATION_PRELUDE);
    }
    out.push_str(&crate::enum_cdata_defs(ws));
    let rep_arena = scx.rep_arena();
    let (vdefs, value_emitted, value_gc) = crate::value_struct_defs(ws, &rep_arena)?;
    out.push_str(&vdefs);
    out.push_str(&crate::value_struct_gc_defs(ws, &value_gc, &rep_arena)?);
    out.push_str(&crate::value_struct_metatypes(ws, &value_emitted));
    out.push_str(&crate::enum_cdata_metatypes(ws));
    out.push_str(&crate::c_struct_inspect_defs(ws, &value_emitted));
    out.push_str(&crate::type_table_defs(ws, &value_gc)?);
    out.push_str(&crate::enum_runtime_defs(ws));
    t_scaffold += ts.elapsed();

    // Decorator DEFINITIONS, workspace-wide: each `deco <name> { .. }` -> a fn
    // `__deco_<name>(target, <params>)`. `deco_map` is built up front (sequential,
    // cheap) because an application usually names a decorator declared in another
    // module; the definition bodies themselves lower+emit in parallel.
    let (deco_map, deco_items) = build_deco_map(ws);
    let deco_results = par_map(deco_items, |(mid, did)| {
        emit_decorator_unit(ws, scx, cg, &cache.concrete, mid, did)
    });
    rejections.extend(unit_errors(&deco_results));
    needed_specs.extend(deco_results.iter().flat_map(|u| u.specs.iter().cloned()));
    let mut unit_reads = UnitComptimeReads::default();
    for u in &deco_results {
        unit_reads
            .decorators
            .extend(u.comptime_reads.iter().cloned());
    }
    let mut always_emitted_calls: Vec<FuncRef> = Vec::new();
    let mut always_emitted_names: Vec<Ustr> = Vec::new();
    let mut decorator_lua = String::new();
    merge_units(
        deco_results,
        &mut decorator_lua,
        &mut skipped,
        &mut direct_globals,
        &mut direct_calls,
        &mut Vec::new(),
        &mut always_emitted_calls,
        &mut always_emitted_names,
        &mut code_artifact,
        &mut allocation_bodies,
        &mut attribution,
        &mut times,
    );
    let mut decorator_definitions = LuaDefinitionBuffer::new();
    decorator_definitions.push_str(&decorator_lua);
    decorator_definitions.append_to(&mut out);

    // Global initializers are emitted below, in their original slot, but they are
    // LOWERED here: their call edges are reachability roots (a global init is
    // always emitted), so the walk needs them before the fn-def loop runs.
    let mut pending_inits: Vec<(Ustr, Ustr, String)> = Vec::new();
    let global_items: Vec<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)> = ws
        .modules
        .iter()
        .filter(|m| m.id != ModuleId::builtin())
        .flat_map(|m| m.globals.iter().map(move |(gid, _)| (m.id, gid)))
        .collect();
    let global_keys = global_items.clone();
    let global_results = par_map(global_items, |(mid, gid)| {
        emit_global_unit(ws, scx, cg, &cache.concrete, mid, gid)
    });
    // par_map preserves input order, so keys and results zip positionally --
    // the same pairing merge_units relies on for output order.
    for (key, u) in global_keys.iter().zip(global_results.iter()) {
        if !u.comptime_reads.is_empty() {
            unit_reads.globals.insert(*key, u.comptime_reads.clone());
        }
    }
    rejections.extend(unit_errors(&global_results));
    needed_specs.extend(global_results.iter().flat_map(|u| u.specs.iter().cloned()));
    let mut globals_lua = String::new();
    // Held back and appended after the fn-def skips so the reported order is
    // unchanged by the hoist.
    let mut global_skipped: Vec<(String, String)> = Vec::new();
    merge_units(
        global_results,
        &mut globals_lua,
        &mut global_skipped,
        &mut direct_globals,
        &mut direct_calls,
        &mut pending_inits,
        &mut always_emitted_calls,
        &mut always_emitted_names,
        &mut code_artifact,
        &mut allocation_bodies,
        &mut attribution,
        &mut times,
    );

    // **Whole-program population, driven by `cache.lua_view` -- Phase 2.**
    // `should_emit_via_view` decides every ordinary/coroutine/inline
    // definition below; `view_driven_specialized_chunks` decides every
    // specialized one. Corpus-verified before this cutover landed (every real
    // `stick.toml` project, both directions: zero instances the old walk
    // emitted that the view did not retain, and the reverse direction's only
    // difference was instances nothing ever DEMANDS standalone -- see that
    // fn's own doc). `_ = &always_emitted_calls` /`&always_emitted_names`:
    // still collected above (decorator/global lowering needs its own direct
    // calls for OTHER bookkeeping -- global-read ordering, static baselines --
    // untouched by this cutover), but no longer fed to a root-discovery walk.
    // Generated-glue obligations (`register_sim`, shader dependencies) are part
    // of the ROOT INPUT the graph and the view were built from -- see
    // [`all_roots`] -- so there is no second population source here.
    let (spec_chunks, spec_rejected) =
        view_driven_specialized_chunks(ws, cg, scx, cache, &needed_specs);
    if lifetime == CacheLifetime::OneShot {
        let instances = cache.graph.instances.len();
        cache.graph.release_records();
        release_stats.graph_instances = instances - cache.graph.instances.len();
    }

    // **Foreign-body (`inline(lua)`/`extern`) DELIVERY, separate from
    // RETENTION.** `lua_view` (and `should_emit_via_view`'s call-serving half)
    // answers "is this declaration's canonical Instance part of the program",
    // which is not the same question as "does anything call it in RESIDUAL
    // (non-spliced) form" -- `@pure @inline` stdlib operators (`+`, `-`, ...)
    // are retained (something calls `+`) but almost always fully spliced away
    // at every call site, so a naive retention-only gate emitted them
    // standalone anyway (found empirically: `__op_add_int_int` and
    // siblings newly appeared in every project's output the moment retention
    // alone drove the inline-body gate). Splicing eligibility is a LOWERING
    // decision the graph does not model (same class of fact as a generic's
    // standalone-vs-spliced `SpecDemand`, just for a NON-generic body), so
    // this is collected from what already-lowered chunks record they still
    // call after inlining (`Chunk::calls`), exactly as `chunk.specs` seeds
    // `view_driven_specialized_chunks` above.
    let mut needed_foreign: FxHashSet<Instance> = FxHashSet::default();
    needed_foreign.extend(always_emitted_calls.iter().map(|f| shared(*f)));
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            let self_ref = FuncRef {
                module: module.id,
                func: fid,
            };
            if !is_cached_emitted_body(f, &shared(self_ref))
                || !should_emit_via_view(ws, &cache.lua_view, self_ref)
            {
                continue;
            }
            if let Some(Ok(c)) = cache.chunks.get(&shared(self_ref)) {
                needed_foreign.extend(c.calls.iter().map(|f| shared(*f)));
            }
        }
    }
    for c in spec_chunks.values() {
        needed_foreign.extend(c.calls.iter().map(|f| shared(*f)));
    }
    // Factory lowering is the last concrete-plan reader. Keep its text in the
    // original output slot, but release one-shot plans before copying definitions.
    let early_definition_stage = if lifetime == CacheLifetime::OneShot {
        let plan = collect_definition_plan(ws)?;
        let stage = definition_plan_stage_lua(ws, cg, scx, &cache.concrete.plans, &plan)?;
        let plans = cache.concrete.plans.len() + cache.concrete.always_plans.len();
        drop(std::mem::take(&mut cache.concrete));
        release_stats.concrete_plans =
            plans - cache.concrete.plans.len() - cache.concrete.always_plans.len();
        Some((plan, stage))
    } else {
        None
    };
    let mut definitions = LuaDefinitionBuffer::new();
    // Second pass of the callee binding: every factory's
    // `<name> = __rock_mk_<name>(<callees>)` line, emitted after EVERY
    // definition and in ascending call-graph component rank, so each captured
    // callee is already its final closure. Empty under the global-lookup shape.
    let mut callee_binds: Vec<skull_codegen_lua::CalleeBindingPlan> = Vec::new();
    let mut emits_main = false;

    // Function definitions, every module (cached chunk for ordinary/coroutine,
    // regenerated for extern / inline(lua)). The cached chunks were built in
    // parallel by `emit_full`/`emit_incremental`; here we pull their Lua and sum
    // their captured phase times back into the breakdown.
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        let mid = module.id;
        for (fid, f) in module.functions.iter() {
            let self_ref = FuncRef {
                module: mid,
                func: fid,
            };
            // A JS-backend decl (`inline(js)` / `extern(js)`) is inert on luajit:
            // its lua sibling (if any) carries the binding, and a js-only call from
            // lua-reachable code is rejected at call lowering (resolve_funcref).
            if f.backend == bone_semantic::hir::FnBackend::Js {
                continue;
            }
            // Whole-program DCE gates both chunk construction and definition
            // emission. Retained bodies still fail hard when their plan or chunk
            // is missing; pruned dependency bodies have neither by design.
            let emit_def = should_emit_via_view(ws, &cache.lua_view, self_ref);
            if f.is_extern {
                // An `extern(ffi, "lib")` fn needs a thin wrapper binding it to the
                // loaded library handle. A non-ffi extern is a pre-existing global.
                //
                // Gated by the VIEW like every other definition. `is_emission_root`
                // roots every `extern`, so in practice the view retains them all
                // and this changes no output -- but an ungated write is a second
                // population input, and one is the whole point of the cut.
                if let Some(lib) = f.ffi_lib {
                    let nm = mangle_fn(ws, f);
                    let params = f
                        .param_decls
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let body = match extern_ret(ws, f)? {
                        // Returns an OWNING copy, never `__out[0]`. LuaJIT does
                        // not keep the parent array alive through a reference
                        // into it, so handing one back is a use-after-free that
                        // reads fine until the memory is reused
                        // (`tests/extern_value_return_ownership.rs`).
                        ExternRet::ValueStruct(key) => {
                            let sname = skull_mir::mangle::type_symbol(ws, ustr::Ustr::from(&key));
                            let call_args = if params.is_empty() {
                                "__out".to_string()
                            } else {
                                format!("__out, {params}")
                            };
                            format!(
                                "  local __out = ffi.new(\"{sname}[1]\")\n  {lib}.{nm}({call_args})\n{}",
                                skull_codegen_lua::wrapper_return(&format!(
                                    "ffi.new(\"{sname}\", __out[0])"
                                ))
                            )
                        }
                        // `return <call>` is a `CALLT` into C, which a root trace
                        // cannot leave through; the bound local makes it a `RET`
                        // (`wrapper_return`). `Bool` ends in a compare, not a call.
                        ExternRet::Int | ExternRet::Uint => skull_codegen_lua::wrapper_return(
                            &format!("tonumber({lib}.{nm}({params}))"),
                        ),
                        ExternRet::Bool => format!("  return {lib}.{nm}({params}) ~= 0"),
                        ExternRet::AsIs => {
                            skull_codegen_lua::wrapper_return(&format!("{lib}.{nm}({params})"))
                        }
                        ExternRet::None => format!("  {lib}.{nm}({params})"),
                    };
                    // The rejection above runs for EVERY extern; only the WRITE
                    // is gated. Pruning may change which definitions are emitted
                    // and nothing else -- gating the whole arm suppressed
                    // `extern_ret`'s unbound-generic rejection, whose subject is
                    // a generic declaration the view holds no base instance for.
                    if emit_def {
                        let prologue = default_fill_prologue(ws, mid, fid, f);
                        definitions
                            .push_str(&format!("function {nm}({params})\n{prologue}{body}\nend\n"));
                    }
                }
                continue;
            }
            if let Some((_, body)) = f.inline_body {
                // Retention alone (`emit_def`) over-emits here -- see
                // `needed_foreign`'s doc. Three independent reasons still force
                // emission regardless of splicing eligibility: a publication
                // obligation (`@nomangle`), a residual (non-spliced) demand, or
                // being a ROOT in its own right (an own-project inline(lua) fn
                // is unconditionally rooted by `is_emission_root` even with no
                // caller at all -- same reasoning as `main`/`@sim`/`@test`;
                // found empirically, `anvil`'s `sv_close`/`sv_is_eof` etc. have
                // no call site anywhere in the corpus and are rooted purely by
                // living in the project's own file).
                if !emits_foreign_definition(ws, &cache.lua_view, &needed_foreign, self_ref) {
                    continue;
                }
                // inline(lua): emit the hand-written body verbatim, wrapping a
                // value-returning bodyless-`return` and prepending default fills.
                // (Js inline bodies were skipped above via the backend guard.)
                let name = mangle_fn(ws, f);
                let params = match skull_mir::lower::BackendFnAbiPlan::for_boundary_declaration(
                    ws, scx, self_ref,
                )
                .lua_param_names(ws)
                {
                    Some(names) => names.join(", "),
                    None => f
                        .param_decls
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                };
                let body = body.as_str();
                let body = if f.return_ty.is_some() && !body.contains("return") {
                    format!("return {}", body.trim())
                } else {
                    body.to_string()
                };
                let prologue = default_fill_prologue(ws, mid, fid, f);
                definitions.push_str(&format!(
                    "function {name}({params})\n{prologue}{body}\nend\n"
                ));
                continue;
            }
            // An `Iterator`-bounded fn has no standalone emission (it exists
            // only inlined at its call sites) -- deliberately absent.
            if fn_has_bounds(f) {
                continue;
            }
            // A generic declaration has no definition of its own. Its bodies are
            // its instances, rendered below by `emit_specialized_callees`.
            if is_generic(f) {
                continue;
            }
            // Ordinary / coroutine: pull the cached chunk.
            match cache.chunks.get(&shared(self_ref)) {
                Some(Ok(chunk)) => {
                    let key = Ustr::from(&mangle_fn(ws, f));
                    direct_globals.insert(key, chunk.direct_globals.clone());
                    direct_calls.insert(key, chunk.direct_calls.clone());
                    times.add(&chunk.times);
                    attribution.add_unit(&chunk.attribution);
                    if emit_def {
                        emits_main |= key.as_str() == "main";
                        definitions.push_str(&chunk.lua);
                        callee_binds.extend(chunk.binds.iter().cloned());
                        allocation_bodies.extend(chunk.allocation_bodies.iter().cloned());
                        code_artifact.merge_ref(&chunk.code_artifact);
                        // A dropped definition's `static` cells do not exist, so
                        // they must not be registered for hot-reload preservation.
                        all_statics.extend(chunk.statics.iter().cloned());
                        // Only a body that is actually emitted can call one.
                        needed_specs.extend(chunk.specs.iter().cloned());
                    }
                }
                Some(Err(err)) if !emit_def => {}
                Some(Err(err)) => {
                    // A GUARANTEE violation is never a known stdlib gap: silently
                    // dropping the body would leave callers calling a nil global.
                    if is_stdlib_module_file(module.file.as_str())
                        && err.kind != skull_parser::CompilerErrorKind::NoboxGuaranteeViolated
                    {
                        skipped.push((mangle_fn(ws, f), err.message.clone()));
                    } else {
                        rejections.push(err.clone());
                    }
                }
                None if !emit_def => {}
                None => {
                    if is_stdlib_module_file(module.file.as_str()) {
                        skipped.push((
                            mangle_fn(ws, f),
                            "not emitted (no cached chunk)".to_string(),
                        ));
                    } else {
                        rejections.push(CompilerError::new_with_label(
                            f.name_span,
                            format!("internal: `{}` has no emitted chunk", f.name),
                        ));
                    }
                }
            }
        }
    }

    rejections.extend(emit_specialized_callees(
        ws,
        scx,
        &mut needed_specs,
        &cache.chunks,
        &spec_chunks,
        &spec_rejected,
        built_specs,
        &mut allocation_bodies,
        &mut all_statics,
        &mut definitions,
        &mut callee_binds,
        &mut attribution,
        &mut times,
    ));
    for chunk in spec_chunks.values() {
        code_artifact.merge_ref(&chunk.code_artifact);
    }
    for (_, chunk) in built_specs.iter() {
        code_artifact.merge_ref(&chunk.code_artifact);
    }
    let specialized_instances: Vec<_> = spec_chunks.keys().cloned().collect();
    let one_shot_reads = if lifetime == CacheLifetime::OneShot {
        let reads = artifact_comptime_reads(cache, built_specs, &unit_reads, true);
        compile_counts.chunks += built_specs
            .iter()
            .filter(|(key, _)| !cache.chunks.contains_key(key))
            .count();
        compile_counts.dependency_edges = Some(
            cache
                .chunks
                .values()
                .flatten()
                .map(|chunk| chunk.deps.len())
                .sum::<usize>()
                + built_specs
                    .iter()
                    .map(|(_, chunk)| chunk.deps.len())
                    .sum::<usize>(),
        );
        let chunks = cache.chunks.len() + built_specs.len();
        drop(std::mem::take(&mut cache.chunks));
        drop(std::mem::take(built_specs));
        release_stats.chunks = chunks - cache.chunks.len() - built_specs.len();
        Some(reads)
    } else {
        None
    };
    drop(spec_chunks);

    // Every emitted definition has now been visited, so this is the earliest
    // point where the whole rejection list exists. Anything past here is
    // scaffolding (init ordering, deco application) whose own errors are
    // unrelated to a body that cannot emit.
    rejections.into_result()?;
    if let Some(err) = cg.take_rejection() {
        return Err(err);
    }
    definitions.append_to(&mut out);
    out.push_str(&callee_binding_pass_lua(&mut callee_binds));
    mark_generated_code_values(
        ws,
        &mut code_artifact,
        cache.build_kind == BuildKind::Program,
        cache.selected_sim_functions.as_ref(),
    );
    let (definition_plan, early_stage) = match early_definition_stage {
        Some((plan, stage)) => (plan, Some(stage)),
        None => (collect_definition_plan(ws)?, None),
    };
    let lifecycle_plan = skull_mir::lifecycle::collect_with_artifact(ws, &code_artifact)?;
    if lifecycle_plan.census.applications != lifecycle_plan.census.admitted {
        return Err(CompilerError::new_no_location(format!(
            "incomplete LifecyclePlan census: {:?}",
            lifecycle_plan.census
        )));
    }
    let allocation_report = if crate::allocation_report::enabled() {
        let mut report = crate::allocation_report::AllocationReport::from_bodies(allocation_bodies);
        report.populate_lifecycle_routes(&lifecycle_plan, &code_artifact);
        report
    } else {
        crate::allocation_report::AllocationReport::default()
    };
    let has_code_slot_publication = code_artifact.entries.values().any(|entry| entry.escaping)
        || !definition_plan.catalogs.is_empty()
        || lifecycle_plan.census.declarations > 0;
    let global_layout = skull_mir::global_storage::global_layout_id(ws, scx)
        .map_err(CompilerError::new_no_location)?;
    let static_count = all_statics
        .iter()
        .map(|cell| cell.cell)
        .collect::<FxHashSet<_>>()
        .len();
    out.push_str(&globals_open_lua(&global_layout, cg, static_count));
    out.push_str(&wrap_large_lua_section(
        "__rock_globals_directory_part",
        &globals_directory_lua(ws, scx, cg)?,
    ));
    // A host attaches its own directory fields right after this line.
    out.push_str("__rock_globals_install_bridge()\n");

    // Globals: an init fn + a dependency-ordered `<NAME> = __skull_ginit_<NAME>()`.
    // (Lowered above, so the walk could see their call edges; appended here, in
    // the original slot, so the emitted text is unchanged.)
    let mut global_definitions = LuaDefinitionBuffer::new();
    global_definitions.push_str(&globals_lua);
    global_definitions.append_to(&mut out);
    skipped.extend(global_skipped);
    let has_definition_plan = !definition_plan.catalogs.is_empty();
    if has_code_slot_publication {
        out.push_str(skull_codegen_lua::FN_MT_PRELUDE);
    }
    // Global initializer bodies (including their escaping lambdas) are now
    // defined, so slot staging must follow them but precede initializer calls.
    let code_slot_stage = code_slot_registration_lua(ws, scx, &code_artifact)?;
    out.push_str(&wrap_large_lua_section(
        "__rock_code_slot_stage_part",
        &code_slot_stage,
    ));
    if has_code_slot_publication && !code_artifact.entries.values().any(|entry| entry.escaping) {
        out.push_str("__rock_code_slot_begin()\n");
    }
    let definition_plan_stage = match early_stage {
        Some(stage) => stage,
        None => definition_plan_stage_lua(ws, cg, scx, &cache.concrete.plans, &definition_plan)?,
    };
    out.push_str(&wrap_large_lua_section(
        "__rock_definition_plan_stage_part",
        &definition_plan_stage,
    ));
    let lifecycle_plan_stage = lifecycle_plan_stage_lua(ws, scx, &lifecycle_plan);
    out.push_str(&wrap_large_lua_section(
        "__rock_lifecycle_plan_stage_part",
        &lifecycle_plan_stage,
    ));

    // Order the init lines by their transitive global-read closure over the full
    // call graph (now that every fn's direct facts are recorded).
    let ts = Instant::now();
    let reach = skull_mir::reach::transitive_global_reads(&direct_globals, &direct_calls);
    // Init ordering is stated over SOURCE globals, but a `nobox` global is READ
    // through its leaves, so a leaf dep has to resolve back to the group that
    // owns it or the dependent initializer sorts before it. Built from the
    // globals that actually mint leaves -- never a name-shape guess.
    let mut leaf_owner: FxHashMap<Ustr, Ustr> = FxHashMap::default();
    for module in ws.modules.iter() {
        for (_, g) in module.globals.iter() {
            let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, &cache.scx, g.ty)
                .map_err(|e| CompilerError::new_no_location(e.message))?;
            // Leaf names are the emitted symbols; the owner they resolve back
            // to is the global's LOGICAL name, which is what an init line and a
            // plain global read are keyed on.
            if let Some(leaves) = plan.leaf_names(skull_mir::mangle::symbol_of_key(g.name)) {
                for leaf in leaves {
                    leaf_owner.insert(leaf, g.name);
                }
            }
        }
    }
    let inits: Vec<GlobalInit> = pending_inits
        .into_iter()
        .map(|(name, init_name, line)| GlobalInit {
            name,
            deps: reach
                .deps_of(init_name)
                .into_iter()
                .map(|d| leaf_owner.get(&d).copied().unwrap_or(d))
                .collect(),
            line,
        })
        .collect();
    // `static` cells: on reload, RESET (nil the cell + clear the done-flag) when
    // the init/type key changed since last run, so the next reach re-inits with
    // the edited initializer; an unchanged key preserves the live cell. Dedup by
    // cell.
    let mut seen_static: FxHashSet<Ustr> = FxHashSet::default();
    let mut static_resets = String::new();
    for sc in &all_statics {
        if !seen_static.insert(sc.cell) {
            continue;
        }
        let key = static_cell_key(sc);
        let (value_slot, done_slot) =
            skull_mir::global_storage::static_cell_storage_keys(ws, scx.interner(), sc)
                .map_err(CompilerError::new_no_location)?;
        let _ = writeln!(
            static_resets,
            "__rock_static_prepare({value_slot:?}, {done_slot:?}, {key:?})"
        );
    }

    out.push_str(&static_resets);
    let init_lines = sort_global_init_lines(inits)?;
    out.push_str(&state_initializer_lua(&init_lines));

    // Decorator APPLICATIONS (after global inits so a deco body can push a global).
    out.push_str(&decorator_application_lines(
        ws,
        &deco_map,
        scx,
        &mut skipped,
    ));
    // `@sim` registrations (after fn + global defs; same init slot as rock).
    if cache.build_kind == BuildKind::Program {
        out.push_str(&sim_registration_lines(
            ws,
            scx,
            cache.selected_sim_functions.as_ref(),
        )?);
    }
    // `rock_shader` pipeline registrations:
    // one top-level `__rock_shader_register(key, glsl)` per distinct pair, so a
    // reload re-`dofile`s them with the new source (hot-reload parity).
    out.push_str(&cache.shader_registrations.lua);
    if has_code_slot_publication {
        out.push_str(code_slot_publish_lua(
            &code_artifact,
            has_definition_plan,
            lifecycle_plan.census.declarations > 0,
        ));
    }
    // Closes the block the runtime prelude opens (the storage cell becomes a
    // closed upvalue here); anything appended after this line runs outside it.
    out.push_str("end\n");
    t_scaffold += ts.elapsed();

    // `times.emit` brackets emit_function/emit_named/emit_coroutine; `times.regalloc`
    // is the subset spent in `color_values` inside them (per-unit reset/read, exact
    // even under parallel emit). stringify = the remainder.
    let t_stringify = times.emit - times.regalloc;
    let phases = vec![
        ("lower hir->mir", times.lower),
        ("inline plan+splice", times.inline),
        ("cleanup mir", times.cleanup),
        ("closure conversion", times.closure),
        ("counted-loop recovery", times.counted_loop),
        ("hoisted-body inline", times.hoisted_inline),
        ("post-transform validation", times.validation),
        ("emit: regalloc", times.regalloc),
        ("emit: stringify", t_stringify),
        ("scaffold+callgraph+order", t_scaffold),
    ];

    // A synthetic top-level name (`<fn>__lambda<k>`, `<co>__step`/`__on_*`/...)
    // that collides with a user fn/global was recorded during chunk emit (above,
    // and in `emit_incremental`'s re-emit). Reject with a spanned diagnostic
    // rather than emit two clobbering top-level defs. Covers `emit_full` +
    // `emit_incremental` (both funnel through here); the overlay fast path drains
    // in `emit_overlay` and propagates the same error.
    let mut emitted_bodies: FxHashSet<Instance> = FxHashSet::default();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            if f.backend == bone_semantic::hir::FnBackend::Js {
                continue;
            }
            let self_ref = FuncRef {
                module: module.id,
                func: fid,
            };
            let actually_emitted = if f.is_extern {
                should_emit_via_view(ws, &cache.lua_view, self_ref)
            } else if f.inline_body.is_some() {
                emits_foreign_definition(ws, &cache.lua_view, &needed_foreign, self_ref)
            } else {
                should_emit_via_view(ws, &cache.lua_view, self_ref)
            };
            if actually_emitted {
                emitted_bodies.insert(shared(self_ref));
            }
        }
    }
    emitted_bodies.extend(specialized_instances);
    let artifact_reads = one_shot_reads
        .unwrap_or_else(|| artifact_comptime_reads(cache, built_specs, &unit_reads, true));
    let artifact_fingerprint = artifact_fingerprint(
        ws,
        scx,
        &code_artifact,
        &lifecycle_plan,
        &definition_plan,
        &global_layout,
        &artifact_reads,
        cache.renderer_target,
    )?;
    // The file name a host writes the program to; generated code after a
    // numbered definition reports its position in that file.
    if cache.cg.line_directives() {
        out = skull_codegen_lua::resolve_line_directive_resets(&out, "main.lua");
    }
    cg.report_trace_frames(&out);
    Ok((
        MirEmit {
            release_stats,
            compile_counts,
            lua: out,
            allocation_report,
            code_artifact: std::sync::Arc::new(code_artifact),
            lifecycle_plan,
            global_layout,
            definition_plan,
            artifact_fingerprint,
            skipped,
            phases,
            abi_plan_stats: scx.abi_plan_stats(),
            attribution: attribution.finish(),
            arena_traffic: ArenaTraffic::default(),
            setup_lanes: SetupLaneAttribution::default(),
            graph_census: crate::instance_graph::RunStats::default(),
            concrete_stats: crate::concrete_body::ConcreteBuildStats::default(),
            emits_main,
            shaders: std::sync::Arc::clone(&cache.shader_registrations.sources),
        },
        emitted_bodies,
        unit_reads,
    ))
}

fn code_slot_registration_lua(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    artifact: &skull_mir::code_identity::CodeArtifact,
) -> Result<String, CompilerError> {
    if !artifact.census.is_total() {
        return Err(CompilerError::new_no_location(format!(
            "cannot stage incomplete Rock CodeRef census: {:?}",
            artifact.census
        )));
    }
    code_slot_registration_lua_entries(ws, scx, artifact.entries.values(), false)
}

fn code_slot_registration_lua_entries<'a>(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    source: impl IntoIterator<Item = &'a skull_mir::code_identity::CodeEntry>,
    begin_when_empty: bool,
) -> Result<String, CompilerError> {
    let entries: Vec<_> = source.into_iter().filter(|entry| entry.escaping).collect();
    if entries.is_empty() {
        return Ok(if begin_when_empty {
            "__rock_code_slot_begin()\n".to_string()
        } else {
            String::new()
        });
    }
    let mut entries = entries;
    entries.sort_by_key(|entry| entry.identity.logical_key(ws, scx.interner()));
    // Thousands of slots share a few hundred call ABIs, so each distinct ABI is
    // spelled once in a table. `do ... end` keeps that local out of the
    // enclosing function's local budget.
    let mut abi_index: FxHashMap<&str, usize> = FxHashMap::default();
    let mut abis = String::new();
    let mut stages = String::new();
    for entry in entries {
        let Some(body) = entry.body_name else {
            return Err(CompilerError::new_no_location(format!(
                "Rock CodeRef `{}` has no emitted body",
                entry.identity.logical_key(ws, scx.interner())
            )));
        };
        let abi = entry.compatibility.call_abi.as_str();
        let next = abi_index.len() + 1;
        let index = *abi_index.entry(abi).or_insert_with(|| {
            writeln!(abis, "{},", skull_codegen_lua::lua_string(abi))
                .expect("writing a String cannot fail");
            next
        });
        writeln!(
            stages,
            "__rock_code_slot_stage({}, {}, __rock_code_slot_abis[{index}])",
            skull_codegen_lua::lua_string(&entry.identity.logical_key(ws, scx.interner())),
            body,
        )
        .expect("writing a String cannot fail");
    }
    Ok(format!(
        "__rock_code_slot_begin()\ndo\nlocal __rock_code_slot_abis = {{\n{abis}}}\n{stages}end\n"
    ))
}

fn overlay_code_artifact(
    ws: &Workspace,
    cache: &SkullEmitCache,
    bodies: &[Instance],
) -> skull_mir::code_identity::CodeArtifact {
    let mut artifact = skull_mir::code_identity::CodeArtifact::default();
    for body in bodies {
        if let Some(Ok(chunk)) = cache.chunks.get(body) {
            for entry in chunk
                .code_artifact
                .entries
                .values()
                .filter(|entry| entry.escaping)
            {
                let mut entry = entry.clone();
                if entry.body_name.is_none() {
                    // An unchanged named value needs the resolved ABI as well as its body.
                    entry = cache
                        .code_artifact
                        .entries
                        .get(&entry.identity)
                        .expect("every escaping chunk entry belongs to the resolved code artifact")
                        .clone();
                }
                artifact.insert(entry);
            }
        }
        if !body.args.is_empty() {
            continue;
        }
        let fref = body.func;
        let identity = skull_mir::code_identity::CodeIdentity::Named(Instance::base(fref));
        let Some(entry) = cache.code_artifact.entries.get(&identity) else {
            continue;
        };
        let mut entry = entry.clone();
        let body_name = mangle_fn(ws, &ws.module(fref.module).functions[fref.func]);
        entry.body_name = Some(Ustr::from(&body_name));
        artifact.insert(entry);
    }
    artifact
}

fn code_slot_publish_lua(
    artifact: &skull_mir::code_identity::CodeArtifact,
    has_definition_plan: bool,
    has_lifecycle_plan: bool,
) -> &'static str {
    if artifact.entries.values().any(|entry| entry.escaping)
        || has_definition_plan
        || has_lifecycle_plan
    {
        "__rock_code_slot_publish()\n"
    } else {
        ""
    }
}

fn collect_definition_plan(
    ws: &Workspace,
) -> Result<skull_mir::definition::DefinitionPlan, CompilerError> {
    let plan = skull_mir::definition::collect(ws)?;
    if plan.census.applications != plan.census.admitted {
        return Err(CompilerError::new_no_location(format!(
            "incomplete DefinitionPlan census: {:?}",
            plan.census
        )));
    }
    Ok(plan)
}

fn definition_plan_stage_lua(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &skull_mir::concrete::ConcretePlans,
    plan: &skull_mir::definition::DefinitionPlan,
) -> Result<String, CompilerError> {
    if plan.catalogs.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::new();
    let mut entries_by_catalog: std::collections::BTreeMap<Ustr, Vec<_>> =
        std::collections::BTreeMap::new();
    let mut materializers = std::collections::BTreeMap::new();
    for entry in &plan.entries {
        let skull_mir::code_identity::CodeIdentity::Named(factory) = &entry.identity else {
            return Err(CompilerError::new_no_location(
                "definition factory has no named identity".to_string(),
            ));
        };
        let mut mf = skull_mir::lower::lower_definition_factory(ws, scx, concrete, factory)?;
        skull_transform::cleanup_with_nobox(ws, &mut mf)?;
        let body =
            skull_codegen_lua::emit_named_with_statics(ws, cg, scx.interner(), "", &mf, &[], None);
        if let Some(error) = cg.take_rejection() {
            return Err(error);
        }
        materializers.insert(entry.identity.clone(), body);
        entries_by_catalog
            .entry(entry.catalog)
            .or_default()
            .push(entry);
    }
    for catalog_plan in &plan.catalogs {
        if let Some(accessor) = catalog_plan.accessor {
            let function = &ws.module(accessor.module).functions[accessor.func];
            let name = mangle_fn(ws, function);
            let _ = writeln!(
                out,
                "function {}()\n  return __rock_definition_catalog({})\nend",
                name,
                skull_codegen_lua::lua_string(catalog_plan.catalog.as_str()),
            );
        }
    }
    out.push_str("__rock_definition_plan_stage({\n");
    for catalog_plan in &plan.catalogs {
        let entries = entries_by_catalog
            .get(&catalog_plan.catalog)
            .cloned()
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "[{}] = {{ catalog = {}, len = {}, _data = {{",
            skull_codegen_lua::lua_string(catalog_plan.catalog.as_str()),
            skull_codegen_lua::lua_string(catalog_plan.catalog.as_str()),
            entries.len()
        );
        for (index, entry) in entries.into_iter().enumerate() {
            let key = entry.identity.logical_key(ws, scx.interner());
            let _ = writeln!(
                out,
                "[{}] = {{ catalog = {}, key = {}, name = {}, ordinal = {}, factory = __rock_code_slot_pending_ref({}) }},",
                index + 1,
                skull_codegen_lua::lua_string(entry.catalog.as_str()),
                skull_codegen_lua::lua_string(entry.key.as_str()),
                skull_codegen_lua::lua_string(entry.diagnostic_name.as_str()),
                entry.declaration_ordinal,
                skull_codegen_lua::lua_string(&key),
            );
        }
        out.push_str("} },\n");
    }
    out.push_str("}, {\n");
    for (identity, body) in materializers {
        let _ = writeln!(
            out,
            "[{}] = {},",
            skull_codegen_lua::lua_string(&identity.logical_key(ws, scx.interner())),
            body
        );
    }
    out.push_str("})\n");
    Ok(out)
}

/// Generated registration calls are escaping-value sites even though they do
/// not have a MIR instruction. Mark their declaration identities before the
/// artifact publishes slots, so lifecycle and sim registries receive the same
/// CodeRef shape as an ordinary `let f = ...` value.
fn mark_generated_code_values(
    ws: &Workspace,
    artifact: &mut skull_mir::code_identity::CodeArtifact,
    register_sims: bool,
    selected: Option<&FxHashSet<Ustr>>,
) {
    let decorator_names: FxHashSet<Ustr> = ws
        .modules
        .iter()
        .flat_map(|m| m.decorators.iter().map(|(_, d)| d.def_key))
        .collect();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            if f.backend == bone_semantic::hir::FnBackend::Js {
                continue;
            }
            let generated_value = f.annotations.iter().any(|ann| {
                (register_sims
                    && ann.is_builtin("sim")
                    && selected.is_none_or(|names| names.contains(&f.name)))
                    || decorator_names.contains(&ann.resolved_name.unwrap_or(ann.name.name))
            });
            if generated_value {
                artifact.mark_value(skull_mir::code_identity::CodeIdentity::Named(
                    Instance::base(FuncRef {
                        module: module.id,
                        func: fid,
                    }),
                ));
            }
        }
    }
}

fn lifecycle_plan_stage_lua(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    plan: &skull_mir::lifecycle::LifecyclePlan,
) -> String {
    if plan.census.declarations == 0 {
        return String::new();
    }
    let mut out = String::from("__rock_lifecycle_plan_stage({\n");
    let mut phases: std::collections::BTreeMap<Ustr, Vec<_>> = std::collections::BTreeMap::new();
    for entry in &plan.entries {
        phases.entry(entry.phase).or_default().push(entry);
    }
    for (phase, entries) in phases {
        let scene_owned = entries
            .iter()
            .filter(|entry| entry.owner.as_str() == "scene")
            .count();
        let _ = writeln!(
            out,
            "[{}] = {{ len = {}, scene_owned = {}, _data = {{",
            skull_codegen_lua::lua_string(phase.as_str()),
            entries.len(),
            scene_owned
        );
        for (index, entry) in entries.into_iter().enumerate() {
            let key = entry.identity.logical_key(ws, scx.interner());
            let dependencies = entry.qualification.dependencies();
            let root_type = match &entry.root_type {
                Some(root) => skull_codegen_lua::lua_string(
                    &skull_mir::lifecycle::scene_root_type_key(root).unwrap_or_else(|| {
                        panic!("validated scene lifecycle root reached emission as {root:?}")
                    }),
                ),
                None => "nil".to_string(),
            };
            // A compiler-synthesized thunk of one uniform signature
            // (`scene_id: int -> ()`) per entry, process- and scene-owned alike
            // -- never the raw slot. A process thunk ignores its argument; a
            // scene thunk resolves the live root for `scene_id` and calls
            // through, skipping a Scene that closed since the population
            // snapshot was taken (`__scene_root_any` returns nil there). Neither
            // arm asks the value its runtime type: membership in the type's live
            // population, established once at Scene creation
            // (`__scene_type_bucket_insert`), is the only test.
            let sim_guard = (!entry.sim_wires.is_empty()).then(|| {
                entry
                    .sim_wires
                    .iter()
                    .map(|wire| {
                        format!(
                            "_G.__ROCK_ACTIVE_SIM == {}",
                            skull_codegen_lua::lua_string(wire.as_str())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" or ")
            });
            let func_thunk = if entry.owner.as_str() == "scene" {
                let call = "local __root = __scene_root_any(scene_id); if __root ~= nil then __s(__root) end";
                let body = sim_guard
                    .as_ref()
                    .map(|guard| format!("if {guard} then {call} end"))
                    .unwrap_or_else(|| call.to_string());
                format!(
                    "(function() local __s = __rock_code_slot_ref({key}); return function(scene_id) {body} end end)()",
                    key = skull_codegen_lua::lua_string(&key),
                )
            } else {
                let call = "__s()";
                let body = sim_guard
                    .as_ref()
                    .map(|guard| format!("if {guard} then {call} end"))
                    .unwrap_or_else(|| call.to_string());
                format!(
                    "(function() local __s = __rock_code_slot_ref({key}); return function(__scene_id) {body} end end)()",
                    key = skull_codegen_lua::lua_string(&key),
                )
            };
            let _ = writeln!(
                out,
                "[{}] = {{ phase = {}, owner = {}, root_type = {}, key = {}, name = {}, stage = {}, ordinal = {}, func = {}, qualification = {}, body_fingerprint = {}, dependencies = {{ len = {}, _data = {{",
                index + 1,
                skull_codegen_lua::lua_string(entry.phase.as_str()),
                skull_codegen_lua::lua_string(entry.owner.as_str()),
                root_type,
                skull_codegen_lua::lua_string(&key),
                skull_codegen_lua::lua_string(entry.diagnostic_name.as_str()),
                entry.stage,
                entry.declaration_ordinal,
                func_thunk,
                skull_codegen_lua::lua_string(entry.qualification.tag()),
                skull_codegen_lua::lua_string(&format!("{:016x}", entry.body_fingerprint)),
                dependencies.len()
            );
            for (dependency_index, dependency) in dependencies.iter().enumerate() {
                let dependency_key = dependency.identity.logical_key(ws, scx.interner());
                let _ = writeln!(
                    out,
                    "[{}] = {{ key = {}, fingerprint = {} }},",
                    dependency_index + 1,
                    skull_codegen_lua::lua_string(&dependency_key),
                    skull_codegen_lua::lua_string(&format!("{:016x}", dependency.fingerprint))
                );
            }
            out.push_str("} } },\n");
        }
        out.push_str("} },\n");
    }
    out.push_str("})\n");
    out
}

/// Full emit: populate the chunk cache for every retained ordinary/coroutine
/// body, then assemble. Byte-identical to the pre-cache emit. Returns the cache
/// so a later [`emit_incremental`] can reuse the unchanged chunks.
///
/// The per-body chunk emit runs in PARALLEL ([`par_map_with`], one
/// [`InlineCache`](skull_transform::InlineCache) per rayon worker) by default;
/// `--features sequential` forces single-threaded iteration. The cache is keyed by
/// [`FuncRef`], so the (nondeterministic) insertion order never reaches the output
/// — `assemble` walks modules in source order and pulls by key.
pub fn emit_full(
    ws: &Workspace,
    user_mid: ModuleId,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    emit_full_in_for_renderer(ws, user_mid, None, RendererTarget::Gl)
}

/// Emitted-shape overrides for one emit. Every shape switch behind these is read
/// once per PROCESS from its environment variable, so a test that must drive two
/// shapes in one test binary sets them here instead. `Default` -- `None`
/// everywhere -- is the production path.
#[derive(Clone, Copy, Default)]
pub struct ShapeOverrides {
    pub callee_binding: Option<skull_codegen_lua::CalleeBindingMode>,
    pub value_ctor: Option<skull_codegen_lua::ValueCtorShape>,
    pub state_cell: Option<bool>,
    /// `Some(false)` emits a body that breaks a LuaJIT parse-time frame cap
    /// instead of rejecting it. Not a production switch and not readable from
    /// the environment: the boundary-agreement test needs the Lua of a rejected
    /// body so it can ask the bundled LuaJIT whether that Lua really fails to
    /// load (`skull_codegen_lua::CodegenCtx::set_frame_limits`).
    pub frame_limits: Option<bool>,
    /// Overrides `ROCK_SKULL_LINE_DIRECTIVES`
    /// (`skull_codegen_lua::CodegenCtx::set_line_directives`).
    pub line_directives: Option<bool>,
}

/// [`emit_full`] under explicit emitted shapes, overriding every
/// `ROCK_SKULL_*` shape switch the overrides name. For tests that must emit
/// several shapes in one process.
pub fn emit_full_with_shapes(
    ws: &Workspace,
    user_mid: ModuleId,
    shapes: ShapeOverrides,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    let _allocation_environment = crate::allocation_report::environment_scope();
    emit_full_in_with_mutation(
        ws,
        user_mid,
        None,
        BodyMutation::None,
        RendererTarget::Gl,
        FullBuild::Program,
        shapes,
    )
}

/// [`emit_full`] under an explicit emitted value-ctor shape, overriding
/// `ROCK_SKULL_VALUE_CTOR`. The sibling of
/// [`emit_full_with_callee_binding`], for the same reason.
pub fn emit_full_with_value_ctor(
    ws: &Workspace,
    user_mid: ModuleId,
    shape: skull_codegen_lua::ValueCtorShape,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    let _allocation_environment = crate::allocation_report::environment_scope();
    emit_full_in_with_mutation(
        ws,
        user_mid,
        None,
        BodyMutation::None,
        RendererTarget::Gl,
        FullBuild::Program,
        ShapeOverrides {
            value_ctor: Some(shape),
            ..ShapeOverrides::default()
        },
    )
}

/// [`emit_full`] under an explicit emitted callee shape, overriding
/// `ROCK_SKULL_CALLEE_BINDING`. The env var is read once per PROCESS, so this
/// is the seam a test uses to drive both shapes in one test binary -- the same
/// role [`skull_codegen_lua::CodegenCtx::set_tail_call_shape`] plays for the
/// tail-call shape.
pub fn emit_full_with_callee_binding(
    ws: &Workspace,
    user_mid: ModuleId,
    binding: skull_codegen_lua::CalleeBindingMode,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    let _allocation_environment = crate::allocation_report::environment_scope();
    emit_full_in_with_mutation(
        ws,
        user_mid,
        None,
        BodyMutation::None,
        RendererTarget::Gl,
        FullBuild::Program,
        ShapeOverrides {
            callee_binding: Some(binding),
            ..ShapeOverrides::default()
        },
    )
}

/// Absolutize `project_root` so it can be path-component-matched against module file
/// paths, which are absolute for a project build.
///
/// Rejects a root that matches NO module. The caller passing a relative or stale
/// directory would otherwise silently classify the project's own sources as
/// library modules -- pruning MORE than intended, with no signal. That is the
/// failure this whole guard exists for: a path predicate that quietly matches
/// nothing looks exactly like one that works.
fn canonical_project_root(
    ws: &Workspace,
    project_root: Option<&str>,
) -> Result<Option<String>, CompilerError> {
    let Some(root) = project_root else {
        return Ok(None);
    };
    let abs = std::fs::canonicalize(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| root.to_string());
    let matches = ws
        .modules
        .iter()
        .any(|m| !is_library_module(m.file.as_str(), Some(&abs)));
    if !matches {
        return Err(CompilerError::new_no_location(format!(
            "skull: project root `{abs}` matches no module file -- every project source would \
             be classified as a library and pruned. This is a caller bug, not a user error."
        )));
    }
    Ok(Some(abs))
}

/// [`emit_full`] scoped to a project directory; see [`is_library_module`].
pub fn emit_full_in(
    ws: &Workspace,
    user_mid: ModuleId,
    project_root: Option<&str>,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    emit_full_in_for_renderer(ws, user_mid, project_root, RendererTarget::Gl)
}

pub fn emit_full_in_for_renderer(
    ws: &Workspace,
    user_mid: ModuleId,
    project_root: Option<&str>,
    renderer_target: RendererTarget,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    emit_full_in_for_renderer_with_callee_binding(ws, user_mid, project_root, renderer_target, None)
}

/// [`emit_full_in_for_renderer`] under an explicit emitted callee shape.
///
/// `None` reads the process shape (`ROCK_SKULL_CALLEE_BINDING`, the pin, the
/// shipped default); `Some` overrides it for THIS emit and the cache it
/// returns, without touching the one-way process pin. That is what lets one
/// process hold two build sessions of different shapes -- the sim runner's
/// hot-reload lane needs the global-lookup shape so its `incremental.lua`
/// overlay can rebind, while every other sim boots the shipped shape.
pub fn emit_full_in_for_renderer_with_callee_binding(
    ws: &Workspace,
    user_mid: ModuleId,
    project_root: Option<&str>,
    renderer_target: RendererTarget,
    callee_binding: Option<skull_codegen_lua::CalleeBindingMode>,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    let _allocation_environment = crate::allocation_report::environment_scope();
    emit_full_in_with_mutation(
        ws,
        user_mid,
        project_root,
        BodyMutation::None,
        renderer_target,
        FullBuild::Program,
        ShapeOverrides {
            callee_binding,
            ..ShapeOverrides::default()
        },
    )
}

#[cfg(test)]
fn emit_full_in_with_test_mutation(
    ws: &Workspace,
    user_mid: ModuleId,
    mutation: BodyMutation,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    emit_full_in_with_mutation(
        ws,
        user_mid,
        None,
        mutation,
        RendererTarget::Gl,
        FullBuild::Program,
        ShapeOverrides::default(),
    )
}

fn emit_full_in_with_mutation(
    ws: &Workspace,
    user_mid: ModuleId,
    project_root: Option<&str>,
    mutation: BodyMutation,
    renderer_target: RendererTarget,
    build: FullBuild<'_>,
    shapes: ShapeOverrides,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    let mut setup_phases = Vec::new();
    let t_setup = Instant::now();
    let project_root = canonical_project_root(ws, project_root)?;
    let project_root = project_root.as_deref();
    if let Some(span) = skull_mir::lower::string_char_literal_out_of_range(ws, user_mid) {
        return Err(CompilerError::new_with_label(
            span,
            "string_char is portable only for ASCII 0-127; for code points use from_codepoint"
                .to_string(),
        ));
    }
    if let Some(err) = skull_mir::lower::expansion_rejection(ws, user_mid) {
        return Err(err);
    }
    if let Some(err) = skull_mir::lower::comptime_input_rejection(ws, user_mid) {
        return Err(err);
    }
    if let Some(err) = skull_mir::intrinsics::intrinsic_rejection(ws, user_mid) {
        return Err(err);
    }
    if let Some(err) = skull_mir::lower::gpu_only_call_rejection(ws, user_mid) {
        return Err(err);
    }
    if let Some((why, span)) = skull_mir::lower::map_key_rejection(ws, user_mid) {
        return Err(CompilerError::new_with_label(
            span,
            format!(
                "this `Map` key is not value-keyed and would silently miss: {why}. A runtime-built \
                 key does not find the entry stored under an equal-by-value one, so an insert at \
                 an equal key grows the map instead of replacing. Use an int, string, bool or \
                 float key -- or a newtype over one of those -- or derive a string key from the \
                 value's fields."
            ),
        ));
    }
    if let Some((msg, span)) = skull_mir::lower::gc_value_c_boundary(ws, user_mid) {
        return Err(CompilerError::new_with_label(span, msg));
    }
    setup_phases.push(("setup: validation+context", t_setup.elapsed()));

    let t_setup = Instant::now();
    // **The graph is built FIRST, into a fresh arena, and `scx` shares that
    // arena rather than minting its own.** An `Instance` the graph mints has to
    // mean something to lowering, and a second arena would be a second identity
    // for the same instance. See `SkullEmitCache::graph`.
    // Generated glue is part of the ROOT INPUT. Shader declarations are known
    // before the graph; their GLSL bodies are rendered only after the view owns
    // the retained instances.
    let (collections, roots) = parallel_join(
        || {
            let shaders = skull_mir::lower::collect_shader_collection(ws)?;
            let compute = skull_mir::lower::collect_compute_collection(ws)?;
            Ok::<_, CompilerError>((shaders, compute))
        },
        || {
            Ok::<_, CompilerError>(match build {
                FullBuild::Program | FullBuild::OneShotProgram => {
                    (emission_roots(ws, project_root), sim_glue_roots(ws))
                }
                FullBuild::PackageTest(tests) => (explicit_and_global_roots(ws, tests), Vec::new()),
                FullBuild::SelectedSims(names) => {
                    let sims = sim_refs_by_wire(ws);
                    let mut roots = names
                        .iter()
                        .map(|name| {
                            sims.get(name.as_str()).copied().ok_or_else(|| {
                                CompilerError::new_no_location(format!("unknown sim root: {name}"))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    roots.extend(ws.modules.iter().flat_map(|m| {
                        m.functions.iter().filter_map(|(fid, f)| {
                            (skull_mir::mangle::declared_name(f.name.as_str()) == "main").then_some(
                                FuncRef {
                                    module: m.id,
                                    func: fid,
                                },
                            )
                        })
                    }));
                    (explicit_and_global_roots(ws, &roots), sim_glue_roots(ws))
                }
            })
        },
    );
    let (shader_collection, compute_collection) = collections?;
    let shader_programs = shader_programs(&shader_collection);
    let compute_programs = compute_programs(&compute_collection);
    let shader_roots: Vec<FuncRef> = shader_programs
        .iter()
        .flat_map(|program| {
            program
                .vert
                .into_iter()
                .chain(std::iter::once(program.frag))
        })
        .chain(compute_programs.iter().map(|program| program.entry))
        .collect();
    let (mut interface_roots, sim_roots) = roots?;
    let mut emission_roots = cached_roots(&interface_roots, &sim_roots, &shader_roots);
    #[cfg(test)]
    if matches!(build, FullBuild::Program | FullBuild::OneShotProgram) {
        assert_eq!(
            emission_roots,
            all_roots(ws, project_root, &shader_roots),
            "cached full-build roots diverged from the wholesale root oracle"
        );
    }
    setup_phases.push(("setup: root collection", t_setup.elapsed()));

    let t_setup = Instant::now();
    let mut graph = crate::instance_graph::build(ws, emission_roots.clone(), false);
    let selected_sim_functions = if matches!(build, FullBuild::SelectedSims(_)) {
        let sims = sim_refs_by_wire(ws);
        let mut scanned = FxHashSet::default();
        loop {
            let reached = graph
                .instances
                .iter()
                .map(|i| i.func)
                .collect::<FxHashSet<_>>();
            let mut added = Vec::new();
            for module in &ws.modules {
                for (owner, body) in &module.bodies {
                    if let BodyOwner::Function(fid) = owner {
                        let func = FuncRef {
                            module: module.id,
                            func: *fid,
                        };
                        if !reached.contains(&func) {
                            continue;
                        }
                    }
                    if !scanned.insert((module.id, *owner)) {
                        continue;
                    }
                    added.extend(named_sim_dependencies(ws, module.id, *owner, body, &sims)?);
                }
            }
            added.retain(|func| !emission_roots.contains(func));
            if added.is_empty() {
                break;
            }
            interface_roots.extend(added);
            interface_roots.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
            interface_roots.dedup();
            emission_roots = cached_roots(&interface_roots, &sim_roots, &shader_roots);
            graph = crate::instance_graph::build(ws, emission_roots.clone(), false);
        }
        let reached = graph
            .instances
            .iter()
            .map(|i| i.func)
            .collect::<FxHashSet<_>>();
        Some(
            sims.values()
                .filter(|func| reached.contains(func))
                .map(|func| ws.module(func.module).functions[func.func].name)
                .collect(),
        )
    } else {
        None
    };
    if let Some(err) = graph_rejection_error(ws, &graph) {
        return Err(err);
    }
    setup_phases.push(("setup: graph", t_setup.elapsed()));

    let t_setup = Instant::now();
    #[allow(unused_mut)]
    let mut concrete = {
        #[cfg(test)]
        if let BodyMutation::ConcreteResultOrder(order) = mutation {
            crate::concrete_body::build_with_result_order(ws, &graph, order).0
        } else {
            crate::concrete_body::build(ws, &graph)
        }
        #[cfg(not(test))]
        {
            crate::concrete_body::build(ws, &graph)
        }
    };
    #[cfg(test)]
    if let BodyMutation::ClearBoundPlan { body } = mutation {
        let mut matched = 0;
        for (instance, plan) in std::sync::Arc::get_mut(&mut concrete.plans)
            .expect("concrete plans are unshared before the setup join")
            .iter_mut()
        {
            if instance.func != body || instance.args.is_empty() {
                continue;
            }
            assert!(
                !plan.bound_tree.is_empty(),
                "bound-plan mutation found a specialized body with no witness tree"
            );
            let plan = std::sync::Arc::make_mut(plan);
            plan.bound_tree = Default::default();
            matched += 1;
        }
        assert!(
            matched > 0,
            "bound-plan mutation found no specialized instance for {body:?}; plans={:?}",
            concrete
                .plans
                .keys()
                .map(|instance| (instance.func, instance.args.len()))
                .collect::<Vec<_>>()
        );
    }
    #[cfg(test)]
    if let BodyMutation::ClearCoroutineResultPlan { body } = mutation {
        let mut matched = 0;
        for (instance, plan) in std::sync::Arc::get_mut(&mut concrete.plans)
            .expect("concrete plans are unshared before the setup join")
            .iter_mut()
        {
            if instance.func == body {
                assert!(
                    plan.signature.result.is_some(),
                    "coroutine-plan mutation found a body with no concrete result witness"
                );
                let plan = std::sync::Arc::make_mut(plan);
                std::sync::Arc::make_mut(&mut plan.signature).result = None;
                matched += 1;
            }
        }
        assert!(
            matched > 0,
            "coroutine-plan mutation found no concrete instance for {body:?}"
        );
    }
    #[cfg(test)]
    if let BodyMutation::ClearIteratorWitnessInstance { body } = mutation {
        let mut matched = 0;
        for (instance, plan) in std::sync::Arc::get_mut(&mut concrete.plans)
            .expect("concrete plans are unshared before the setup join")
            .iter_mut()
        {
            if instance.func != body {
                continue;
            }
            let witness = std::sync::Arc::make_mut(plan)
                .bound_tree
                .next
                .values_mut()
                .find_map(|next| match next {
                    skull_mir::bound::ResolvedBoundNext::Concrete { instance, .. }
                        if !instance.args.is_empty() =>
                    {
                        Some(instance)
                    }
                    _ => None,
                })
                .expect("iterator-plan mutation found no concrete witness instance");
            witness.args.clear();
            matched += 1;
        }
        assert!(
            matched > 0,
            "iterator-plan mutation found no concrete instance for {body:?}"
        );
    }
    if let Some((instance, reason)) = concrete.first_plan_failure() {
        let f = &ws.module(instance.func.module).functions[instance.func.func];
        return Err(CompilerError::new_with_label(
            f.name_span,
            format!(
                "cannot build concrete lowering plan for `{}`: {:?}",
                f.name, reason
            ),
        ));
    }
    if let Some((owner, reason)) = concrete.first_always_plan_failure() {
        return Err(CompilerError::new_no_location(format!(
            "cannot build concrete always-emitted lowering plan for {owner:?}: {reason:?}"
        )));
    }
    setup_phases.push(("setup: concrete plans", t_setup.elapsed()));

    #[cfg(test)]
    let skip_abi_plan_publish = matches!(mutation, BodyMutation::SkipAbiPlanPublish);
    #[cfg(not(test))]
    let skip_abi_plan_publish = false;

    #[cfg(all(test, not(feature = "sequential")))]
    let setup_scheduler = match mutation {
        BodyMutation::SetupLaneSchedule(schedule) => {
            Some(std::sync::Arc::new(SetupLaneScheduler::new(schedule)))
        }
        _ => None,
    };

    let t_setup = Instant::now();
    // The graph is complete before these lanes start; shared interning remains
    // synchronized. ABI plans carry their own representation arena, so the
    // warmed context is adopted after join.
    let shader_plans = std::sync::Arc::clone(&concrete.plans);
    let abi_plans = std::sync::Arc::clone(&concrete.plans);
    let abi_always_plans = &concrete.always_plans;
    let mono_before_lanes = graph.interner().stats();
    let run_setup_lanes = || {
        parallel_join(
            || {
                parallel_join(
                    || {
                        #[cfg(all(test, not(feature = "sequential")))]
                        if let Some(scheduler) = setup_scheduler.as_deref() {
                            scheduler.wait(SetupLaneKind::CpuArtifact);
                        }
                        let started = Instant::now();
                        let scx = skull_mir::lower::ScalarCtx::new(ws)
                            .sharing(std::sync::Arc::clone(graph.interner()));
                        let lua_view = crate::artifact::realize(
                            ws,
                            &graph,
                            &crate::artifact::ArtifactRequest::new(
                                crate::target_projection::LUAJIT_DESKTOP,
                                &emission_roots,
                            )
                            .with_scalar_context(&scx),
                        );
                        let population = lua_view.instances.len() + lua_view.boundaries.len();
                        let finished = Instant::now();
                        #[cfg(all(test, not(feature = "sequential")))]
                        if let Some(scheduler) = setup_scheduler.as_deref() {
                            scheduler.finish(SetupLaneKind::CpuArtifact);
                        }
                        (
                            lua_view,
                            scx,
                            SetupLaneWindow {
                                started,
                                finished,
                                population,
                            },
                        )
                    },
                    || {
                        #[cfg(all(test, not(feature = "sequential")))]
                        if let Some(scheduler) = setup_scheduler.as_deref() {
                            scheduler.wait(SetupLaneKind::Shader);
                        }
                        let started = Instant::now();
                        // The overload set lives in `cg` (computed once there);
                        // borrow it rather than rescanning the workspace.
                        let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
                            .sharing(std::sync::Arc::clone(graph.interner()));
                        let result = (|| {
                            let shader_view = realize_shader_view(
                                ws,
                                &graph,
                                &shader_programs,
                                &compute_programs,
                                &shader_scx,
                                shader_plans,
                            )?;
                            let population = shader_view.roots.len()
                                + shader_view.helpers.len()
                                + shader_view.builtins.len();
                            let shader_registrations = shader_registration_lines(
                                ws,
                                &shader_scx,
                                &shader_collection,
                                &compute_collection,
                                &shader_view,
                                renderer_target,
                            )?;
                            Ok::<_, CompilerError>((shader_view, shader_registrations, population))
                        })();
                        let population = result
                            .as_ref()
                            .map(|(_, _, population)| *population)
                            .unwrap_or(0);
                        let finished = Instant::now();
                        #[cfg(all(test, not(feature = "sequential")))]
                        if let Some(scheduler) = setup_scheduler.as_deref() {
                            scheduler.finish(SetupLaneKind::Shader);
                        }
                        (
                            result.map(|(view, registrations, _)| (view, registrations)),
                            SetupLaneWindow {
                                started,
                                finished,
                                population,
                            },
                        )
                    },
                )
            },
            || {
                #[cfg(all(test, not(feature = "sequential")))]
                if let Some(scheduler) = setup_scheduler.as_deref() {
                    scheduler.wait(SetupLaneKind::AbiWarm);
                }
                let started = Instant::now();
                let mut abi_scx = skull_mir::lower::ScalarCtx::new(ws)
                    .sharing(std::sync::Arc::clone(graph.interner()));
                if !skip_abi_plan_publish {
                    abi_scx.warm_abi_plans(ws, &abi_plans, abi_always_plans);
                }
                let population = abi_scx.abi_plan_stats().warm_keys as usize;
                let finished = Instant::now();
                #[cfg(all(test, not(feature = "sequential")))]
                if let Some(scheduler) = setup_scheduler.as_deref() {
                    scheduler.finish(SetupLaneKind::AbiWarm);
                }
                (
                    abi_scx,
                    SetupLaneWindow {
                        started,
                        finished,
                        population,
                    },
                )
            },
        )
    };
    #[cfg(all(test, not(feature = "sequential")))]
    let ((cpu_result, shader_result), (abi_scx, abi_lane)) = if matches!(
        mutation,
        BodyMutation::SetupLaneSchedule(SetupLaneSchedule::Handshake)
    ) {
        rayon::ThreadPoolBuilder::new()
            .num_threads(3)
            .build()
            .expect("setup handshake test pool")
            .install(run_setup_lanes)
    } else {
        run_setup_lanes()
    };
    #[cfg(any(not(test), feature = "sequential"))]
    let ((cpu_result, shader_result), (abi_scx, abi_lane)) = run_setup_lanes();
    let mono_after_lanes = graph.interner().stats();
    #[cfg(test)]
    let setup_handshake = {
        #[cfg(not(feature = "sequential"))]
        {
            setup_scheduler
                .as_deref()
                .map(|scheduler| scheduler.handshake())
                .unwrap_or((0, 0, false))
        }
        #[cfg(feature = "sequential")]
        {
            (0, 0, false)
        }
    };
    let (lua_view, mut scx, cpu_lane) = cpu_result;
    let (shader_result, shader_lane) = shader_result;
    let (shader_view, shader_registrations) = shader_result?;
    scx.adopt_warmed_abi(abi_scx);
    let state_slots = lua_global_state_slots(ws, &scx)?.into_iter().collect();
    let mut cg = CodegenCtx::new_with_state(ws, state_slots);
    cg.set_recursive_callees(recursive_callees(&graph));
    cg.set_call_sccs(call_sccs(&graph));
    if let Some(binding) = shapes.callee_binding {
        cg.set_callee_binding(binding);
    }
    if let Some(shape) = shapes.value_ctor {
        cg.set_value_ctor_shape(shape);
    }
    if let Some(on) = shapes.state_cell {
        cg.set_state_cell(on);
    }
    if let Some(on) = shapes.frame_limits {
        cg.set_frame_limits(on);
    }
    if let Some(on) = shapes.line_directives {
        cg.set_line_directives(on);
    }
    let setup_lanes = SetupLaneAttribution {
        cpu_population: cpu_lane.population,
        shader_population: shader_lane.population,
        abi_population: abi_lane.population,
        cpu_shader_overlap: setup_lanes_overlap(cpu_lane, shader_lane),
        abi_cpu_overlap: setup_lanes_overlap(abi_lane, cpu_lane),
        abi_shader_overlap: setup_lanes_overlap(abi_lane, shader_lane),
        first_started_lane: Some(first_setup_lane(cpu_lane, shader_lane, abi_lane)),
        mono_write_acqs_during_join: mono_after_lanes
            .write_acqs
            .saturating_sub(mono_before_lanes.write_acqs),
        mono_intern_attempts_during_join: mono_after_lanes
            .intern_attempts
            .saturating_sub(mono_before_lanes.intern_attempts),
        mono_intern_new_during_join: mono_after_lanes
            .intern_new
            .saturating_sub(mono_before_lanes.intern_new),
        #[cfg(test)]
        setup_handshake_entered: setup_handshake.0,
        #[cfg(test)]
        setup_handshake_released: setup_handshake.1,
        #[cfg(test)]
        setup_handshake_released_after_both: setup_handshake.2,
    };
    setup_phases.push(("setup: artifact+shader+ABI lanes", t_setup.elapsed()));
    setup_phases.push((
        "join: cpu artifact branch",
        cpu_lane.finished - cpu_lane.started,
    ));
    setup_phases.push((
        "join: shader view branch",
        shader_lane.finished - shader_lane.started,
    ));
    setup_phases.push((
        "join: ABI plan warm branch",
        abi_lane.finished - abi_lane.started,
    ));

    // The emitted param list must be injective. Checked per SIGNATURE, over every
    // function rather than just the emitted set: a duplicate Lua param is illegal
    // regardless of which filter would have reached the body.
    let t_setup = Instant::now();
    for m in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (fid, _) in m.functions.iter() {
            let fref = FuncRef {
                module: m.id,
                func: fid,
            };
            for (key, err) in skull_mir::lower::scalar_abi_param_collisions(ws, &scx, fref) {
                cg.reject(key, err);
            }
        }
    }
    setup_phases.push(("setup: ABI validation", t_setup.elapsed()));

    let setup_traffic = (scx.rep_arena_stats(), scx.interner().stats());

    let fn_items: Vec<(FuncRef, &bone_semantic::hir::Function)> = ws
        .modules
        .iter()
        .filter(|m| m.id != ModuleId::builtin())
        .flat_map(|m| {
            let view = &lua_view;
            m.functions.iter().filter_map(move |(fid, f)| {
                let self_ref = FuncRef {
                    module: m.id,
                    func: fid,
                };
                (is_cached_body(f) && should_emit_via_view(ws, view, self_ref))
                    .then_some((self_ref, f))
            })
        })
        .collect();
    let t_setup = Instant::now();
    let inline_candidate_table = build_inline_candidate_table(
        ws,
        &scx,
        &concrete,
        inline_candidate_keys(&concrete),
        None,
        None,
        None,
        None,
        None,
    )?;
    setup_phases.push(("setup: inline candidate table", t_setup.elapsed()));
    let chunks = par_map_with(
        fn_items,
        || {
            skull_transform::InlineCache::with_candidate_table(std::sync::Arc::clone(
                &inline_candidate_table,
            ))
        },
        |inline_cache, (self_ref, f)| {
            (
                shared(self_ref),
                emit_body_chunk_with_mutation(
                    ws,
                    &cg,
                    &scx,
                    &concrete,
                    self_ref,
                    f,
                    inline_cache,
                    mutation,
                ),
            )
        },
    );
    let chunks: FxHashMap<Instance, Result<Chunk, CompilerError>> = chunks.into_iter().collect();
    // Seed the reverse dependency index from every chunk's codegen deps.
    let mut dependents: FxHashMap<FuncRef, FxHashSet<Instance>> = FxHashMap::default();
    let mut dependency_edges = 0;
    for (body, chunk) in &chunks {
        if let Ok(c) = chunk {
            dependency_edges += c.deps.len();
            if build.lifetime() == CacheLifetime::Retained {
                for d in &c.deps {
                    dependents.entry(*d).or_default().insert(body.clone());
                }
            }
        }
    }
    let concrete_stats = if concrete_stats_requested() {
        concrete.snapshot_build_stats()
    } else {
        crate::concrete_body::ConcreteBuildStats::default()
    };
    let mut cache = SkullEmitCache {
        chunks,
        concrete,
        graph,
        lua_view,
        changed_since_full: FxHashSet::default(),
        cg,
        scx,
        inline_cache: skull_transform::InlineCache::with_candidate_table(inline_candidate_table),
        dependents,
        changed_consts_since_full: FxHashSet::default(),
        static_key_baseline: FxHashMap::default(),
        roots: RootCache {
            project_root: project_root.map(str::to_string),
            interface_roots,
            sim_roots,
            shader_roots,
            roots: emission_roots.clone(),
        },
        shader_registrations,
        shader_view,
        emitted: FxHashSet::default(),
        code_artifact: std::sync::Arc::new(skull_mir::code_identity::CodeArtifact::default()),
        lifecycle_plan: None,
        global_layout: None,
        definition_plan: None,
        artifact_fingerprint: None,
        fingerprint_code_rows: None,
        unit_comptime_reads: UnitComptimeReads::default(),
        renderer_target,
        build_kind: build.kind(),
        selected_sim_functions,
        repair_stats: IncrementalRepairStats::default(),
        body_edit_rollback: None,
    };
    let mut built_specs: Vec<(Instance, Chunk)> = Vec::new();
    let graph_census = cache.graph.run_stats;
    let (mut emit, emitted, unit_reads) =
        assemble(ws, &mut cache, &mut built_specs, build.lifetime())?;
    emit.concrete_stats = concrete_stats;
    emit.setup_lanes = setup_lanes;
    let mut phases = setup_phases;
    phases.extend(emit.phases);
    emit.phases = phases;
    emit.arena_traffic = ArenaTraffic {
        rep_setup: setup_traffic.0,
        rep_final: cache.scx.rep_arena_stats(),
        mono_setup: setup_traffic.1,
        mono_final: cache.scx.interner().stats(),
    };
    if build.lifetime() == CacheLifetime::OneShot {
        emit.graph_census = graph_census;
        return Ok((emit, cache));
    }
    cache.emitted = emitted;
    cache.code_artifact = emit.code_artifact.clone();
    cache.lifecycle_plan = Some(std::sync::Arc::new(emit.lifecycle_plan.clone()));
    cache.global_layout = Some(emit.global_layout.clone());
    cache.definition_plan = Some(std::sync::Arc::new(emit.definition_plan.clone()));
    cache.artifact_fingerprint = Some(emit.artifact_fingerprint.clone());
    cache.fingerprint_code_rows = Some(
        crate::artifact_fingerprint::CodeRows::collect(
            ws,
            cache.scx.interner(),
            &emit.code_artifact,
        )
        .map_err(|error| CompilerError::new_no_location(error.to_string()))?,
    );
    cache.unit_comptime_reads = unit_reads;
    dependency_edges += cache.absorb_spec_chunks(built_specs);
    // Baseline every `static` cell's reload key so the overlay can detect an
    // initializer edit (changed key) vs an unrelated body edit (unchanged key),
    // and its storage keys so the overlay can drop a static an edit removed.
    // After the specializations are absorbed: a generic's static lives in its
    // specialization's chunk.
    cache.advance_static_baseline(ws)?;
    emit.compile_counts.chunks = cache.chunks.len();
    emit.compile_counts.dependency_edges = Some(dependency_edges);
    emit.graph_census = graph_census;
    emit.release_stats.retained_dependency_edges =
        cache.dependents.values().map(|keys| keys.len()).sum();
    emit.release_stats.retained_static_keys = cache.static_key_baseline.len();
    Ok((emit, cache))
}

/// Incremental emit: re-emit only the chunks affected by `reinferred` (the bodies
/// bone re-inferred, from `Session::update_file`'s `UpdateResult`), reusing the
/// rest from `cache`, then assemble. `reinferred` is `(ModuleId, BodyOwner)`;
/// non-function owners (globals, decorators) need no chunk work — they are
/// regenerated by `assemble` — but still trigger reassembly.
///
/// Affected = the re-inferred FUNCTION bodies, plus every cached chunk whose
/// codegen deps intersect them (a body inlined into it, or a callee whose inline
/// eligibility could flip). One pass — each chunk's deps already encode its full
/// inline closure, so no fixpoint.
/// Update the per-function chunk cache for the re-inferred bodies and return the
/// affected set (changed bodies + cached chunks whose deps touch a changed body).
/// Prunes dangling keys first (an interface edit shifts `Idx<Function>`, so a
/// cached `FuncRef` can dangle; the Session re-infers the whole edited module, so
/// every shifted fn is in `reinferred` and re-emitted here). Shared by
/// `emit_incremental` (full reassembly) and `emit_overlay` (the incremental.lua
/// split).
/// Whether any of `reads` no longer matches the CURRENT inputs. File ids are
/// the ABSOLUTE path the resolver landed on; an unreadable input hashes as
/// None != recorded, forcing a re-lower whose spanned rejection reports the
/// real cause. `file_hashes` memoizes reads within one staleness sweep.
fn comptime_reads_stale(
    ws: &Workspace,
    reads: &[skull_mir::mir::ComptimeRead],
    file_hashes: &mut FxHashMap<Ustr, Option<u64>>,
) -> bool {
    use skull_mir::mir::{ComptimeInputId, comptime_value_hash};
    reads.iter().any(|read| {
        let current = match &read.input {
            ComptimeInputId::Var(name) => ws
                .comptime_inputs
                .vars
                .get(name)
                .map(|v| comptime_value_hash(&v.value)),
            ComptimeInputId::File(path) => *file_hashes.entry(*path).or_insert_with(|| {
                let text = ws.comptime_inputs.embed_files.reread(path.as_str()).ok()?;
                Some(comptime_value_hash(&text))
            }),
        };
        current != Some(read.value_hash)
    })
}

impl SkullEmitCache {
    pub fn registered_sim_names(&self, ws: &Workspace) -> std::collections::HashSet<String> {
        collect_sim_fns(ws)
            .into_iter()
            .filter(|(_, f, _)| {
                self.selected_sim_functions
                    .as_ref()
                    .is_none_or(|names| names.contains(&f.name))
            })
            .map(|(_, _, wire)| wire)
            .collect()
    }

    pub fn repair_stats(&self) -> IncrementalRepairStats {
        self.repair_stats
    }

    /// The emitted callee shape THIS cache's chunks were built with. Read it
    /// rather than the process shape wherever a decision has to agree with the
    /// bodies already on disk: a second build session in the same process can
    /// hold the other shape (`emit_full_in_for_renderer_with_callee_binding`),
    /// and the process pin is one-way and cannot describe both.
    pub fn callee_binding(&self) -> skull_codegen_lua::CalleeBindingMode {
        self.cg.callee_binding()
    }

    pub fn begin_body_edit_transaction(
        &mut self,
        reinferred: &[(ModuleId, BodyOwner)],
        edits: &[crate::instance_graph::SpanEdit],
    ) {
        assert!(
            self.body_edit_rollback.is_none(),
            "body edit transaction already active"
        );
        self.body_edit_rollback = Some(BodyEditRollback {
            reinferred: reinferred.to_vec(),
            edits: edits.to_vec(),
            changed_since_full: self.changed_since_full.clone(),
            changed_consts_since_full: self.changed_consts_since_full.clone(),
            static_key_baseline: self.static_key_baseline.clone(),
            emitted: self.emitted.clone(),
            code_artifact: self.code_artifact.clone(),
            lifecycle_plan: self.lifecycle_plan.clone(),
            global_layout: self.global_layout.clone(),
            definition_plan: self.definition_plan.clone(),
            artifact_fingerprint: self.artifact_fingerprint.clone(),
            fingerprint_code_rows: self.fingerprint_code_rows.clone(),
            unit_comptime_reads: self.unit_comptime_reads.clone(),
            repair_stats: self.repair_stats,
        });
    }

    pub fn commit_body_edit_transaction(&mut self) {
        self.body_edit_rollback = None;
    }

    pub fn rollback_body_edit(&mut self, ws: &Workspace) -> Result<(), CompilerError> {
        let Some(snapshot) = self.body_edit_rollback.take() else {
            return Ok(());
        };
        let BodyEditRollback {
            reinferred,
            edits,
            changed_since_full,
            changed_consts_since_full,
            static_key_baseline,
            emitted,
            code_artifact,
            lifecycle_plan,
            global_layout,
            definition_plan,
            artifact_fingerprint,
            fingerprint_code_rows,
            unit_comptime_reads,
            repair_stats,
        } = snapshot;
        let inverse_edits: Vec<_> = edits
            .iter()
            .map(|edit| crate::instance_graph::SpanEdit {
                file: edit.file,
                at: edit.at,
                byte_delta: -edit.byte_delta,
                line_delta: -edit.line_delta,
            })
            .collect();
        let result = (|| {
            let shader_changed = self.update_shader_registrations(ws, &reinferred)?;
            let (_affected, update, added_errors, _fingerprint_identities) =
                self.update_chunk_cache(ws, &reinferred, &inverse_edits)?;
            if let Some(error) = Self::combine_errors(added_errors) {
                return Err(error);
            }
            if shader_changed || self.shader_delta_affects_view(&update) {
                self.refresh_shader_registrations(ws)?;
            }
            Ok(())
        })();
        self.changed_since_full = changed_since_full;
        self.changed_consts_since_full = changed_consts_since_full;
        self.static_key_baseline = static_key_baseline;
        self.emitted = emitted;
        self.code_artifact = code_artifact;
        self.lifecycle_plan = lifecycle_plan;
        self.global_layout = global_layout;
        self.definition_plan = definition_plan;
        self.artifact_fingerprint = artifact_fingerprint;
        self.fingerprint_code_rows = fingerprint_code_rows;
        self.unit_comptime_reads = unit_comptime_reads;
        self.repair_stats = repair_stats;
        result
    }

    fn candidate_index_by_func(&self) -> &FxHashMap<FuncRef, FxHashSet<Instance>> {
        self.concrete.candidate_keys_by_func()
    }

    /// The absolute path of every `embed_string` input baked into the program,
    /// with the hash of the bytes the lowering baked in: the files a watcher
    /// must treat as sources. Covers function chunks and the always-emitted
    /// units (global initializers, decorator defs); the units matter because
    /// all in-tree production embeds are global initializers (the cimgui
    /// prelude).
    pub fn comptime_file_hashes(&self) -> Vec<(Ustr, u64)> {
        let mut out: FxHashMap<Ustr, u64> = FxHashMap::default();
        let chunk_reads = self
            .chunks
            .values()
            .filter_map(|chunk| chunk.as_ref().ok())
            .flat_map(|chunk| chunk.comptime_reads.iter());
        let unit_reads = self
            .unit_comptime_reads
            .globals
            .values()
            .flatten()
            .chain(self.unit_comptime_reads.decorators.iter());
        for read in chunk_reads.chain(unit_reads) {
            if let skull_mir::mir::ComptimeInputId::File(path) = read.input {
                out.insert(path, read.value_hash);
            }
        }
        out.into_iter().collect()
    }

    /// The authoritative instance graph this cache emits against. Always
    /// present -- there is no build in which production has no graph.
    pub fn graph(&self) -> &crate::instance_graph::InstanceGraph {
        &self.graph
    }

    /// The shader artifact view used by the current renderer registration.
    pub fn shader_view(&self) -> &crate::shader_artifact::ShaderArtifactView {
        &self.shader_view
    }

    /// The Lua artifact's retained realization of [`Self::graph`], from the
    /// same roots the graph was built/updated from. Body edits apply a local
    /// delta; full construction remains the independent oracle.
    pub fn lua_view(&self) -> &crate::artifact::ArtifactView {
        &self.lua_view
    }

    pub fn global_layout(&self) -> Option<&str> {
        self.global_layout.as_deref()
    }

    pub fn lifecycle_plan(&self) -> Option<&skull_mir::lifecycle::LifecyclePlan> {
        self.lifecycle_plan.as_deref()
    }

    pub fn body_edit_baseline_lifecycle_plan(
        &self,
    ) -> Option<&skull_mir::lifecycle::LifecyclePlan> {
        self.body_edit_rollback
            .as_ref()
            .and_then(|snapshot| snapshot.lifecycle_plan.as_deref())
    }

    pub fn artifact_fingerprint(&self) -> Option<&str> {
        self.artifact_fingerprint.as_deref()
    }

    /// A live shader registry cannot remove a key from the running process:
    /// registration lines only add or replace entries. A key-set change must
    /// therefore take the process-restarting reload path before any overlay is
    /// written.
    pub fn shader_registration_keys_changed(&self, ws: &Workspace) -> Result<bool, CompilerError> {
        let graphics = skull_mir::lower::collect_shader_pairs(ws)?
            .into_iter()
            .map(|pair| pair.key);
        let compute = skull_mir::lower::collect_compute_collection(ws)?
            .entries
            .into_iter()
            .map(|entry| entry.key);
        let current: BTreeSet<String> = graphics.chain(compute).collect();
        Ok(current != self.shader_registrations.keys)
    }

    /// Compare the registration key set after replacing only the contributions
    /// of the re-inferred bodies. The full-workspace method above remains the
    /// full-build/reload oracle; BodyOnly must not use it.
    pub fn shader_registration_keys_changed_for_bodies(
        &self,
        ws: &Workspace,
        reinferred: &[(ModuleId, BodyOwner)],
    ) -> Result<bool, CompilerError> {
        let mut users = self.shader_registrations.shader_pair_users.clone();
        let mut compute_users = self.shader_registrations.compute_entry_users.clone();
        for &(mid, owner) in reinferred {
            if let Some(previous) = self
                .shader_registrations
                .shader_contributions
                .get(&(mid, owner))
            {
                for pair in previous {
                    let count = users
                        .get_mut(&pair.key)
                        .expect("shader key index lost a body contribution");
                    *count -= 1;
                    if *count == 0 {
                        users.remove(&pair.key);
                    }
                }
            }
            if let Some(previous) = self
                .shader_registrations
                .compute_contributions
                .get(&(mid, owner))
            {
                for entry in previous {
                    let count = compute_users
                        .get_mut(&entry.key)
                        .expect("compute key index lost a body contribution");
                    *count -= 1;
                    if *count == 0 {
                        compute_users.remove(&entry.key);
                    }
                }
            }
            let (shader, _) = skull_mir::lower::collect_shader_body(ws, mid, owner)?;
            for pair in shader {
                *users.entry(pair.key).or_insert(0) += 1;
            }
            let (compute, _) = skull_mir::lower::collect_compute_body(ws, mid, owner)?;
            for entry in compute {
                *compute_users.entry(entry.key).or_insert(0) += 1;
            }
        }
        let current: BTreeSet<String> = users
            .keys()
            .cloned()
            .chain(compute_users.keys().cloned())
            .collect();
        Ok(current != self.shader_registrations.keys)
    }

    /// The compiler-owned ABI contract for every WGPU artifact registered by
    /// this cache. Body edits may replace source under the same contract; a
    /// changed wire cannot be applied to a live registry/pipeline safely.
    pub fn shader_registration_contracts(&self) -> BTreeMap<String, String> {
        self.shader_registrations.abi_wires.clone()
    }

    pub fn wgpu_projection_census(&self) -> Option<&WgpuProjectionCensus> {
        self.shader_registrations.wgpu_projection_census.as_ref()
    }

    fn update_shader_registrations(
        &mut self,
        ws: &Workspace,
        reinferred: &[(ModuleId, BodyOwner)],
    ) -> Result<bool, CompilerError> {
        self.repair_stats.shader_delta_requests = reinferred.len();
        let affected = reinferred.iter().any(|&(mid, owner)| {
            self.shader_registrations.callsites.contains(&(mid, owner))
                || matches!(
                    owner,
                    BodyOwner::Function(fid)
                        if self
                            .shader_registrations
                            .dependencies
                            .contains(&FuncRef { module: mid, func: fid })
                )
                || skull_mir::lower::body_has_shader_pipeline_call(ws, mid, owner)
                || skull_mir::lower::body_has_compute_artifact_call(ws, mid, owner)
        });
        if !affected {
            return Ok(false);
        }
        let mut roots_examined = 0;
        let mut roots_derived = 0;
        for &(mid, owner) in reinferred {
            let key = (mid, owner);
            let old_shader = self
                .shader_registrations
                .shader_contributions
                .remove(&key)
                .unwrap_or_default();
            let old_compute = self
                .shader_registrations
                .compute_contributions
                .remove(&key)
                .unwrap_or_default();
            roots_examined += old_shader
                .iter()
                .map(|pair| usize::from(pair.vert.is_some()) + 1)
                .sum::<usize>();
            roots_examined += old_compute.len();
            for pair in old_shader {
                let users = self
                    .shader_registrations
                    .shader_pair_users
                    .get_mut(&pair.key)
                    .expect("shader contribution index lost an existing key");
                *users -= 1;
                if *users == 0 {
                    self.shader_registrations
                        .shader_pair_users
                        .remove(&pair.key);
                    self.shader_registrations.shader_pairs.remove(&pair.key);
                    if let Some(vert) = pair.vert {
                        self.shader_registrations.shader_root_set.remove(&vert);
                    }
                    self.shader_registrations.shader_root_set.remove(&pair.frag);
                }
            }
            for entry in old_compute {
                let users = self
                    .shader_registrations
                    .compute_entry_users
                    .get_mut(&entry.key)
                    .expect("compute contribution index lost an existing key");
                *users -= 1;
                if *users == 0 {
                    self.shader_registrations
                        .compute_entry_users
                        .remove(&entry.key);
                    self.shader_registrations.compute_entries.remove(&entry.key);
                    self.shader_registrations
                        .shader_root_set
                        .remove(&entry.entry);
                }
            }

            let (shader, shader_callsite) = skull_mir::lower::collect_shader_body(ws, mid, owner)?;
            let (compute, compute_callsite) =
                skull_mir::lower::collect_compute_body(ws, mid, owner)?;
            roots_derived += shader
                .iter()
                .map(|pair| usize::from(pair.vert.is_some()) + 1)
                .sum::<usize>();
            roots_derived += compute.len();
            for pair in &shader {
                let users = self
                    .shader_registrations
                    .shader_pair_users
                    .entry(pair.key.clone())
                    .or_insert(0);
                *users += 1;
                if *users == 1 {
                    self.shader_registrations
                        .shader_pairs
                        .insert(pair.key.clone(), pair.clone());
                    if let Some(vert) = pair.vert {
                        self.shader_registrations.shader_root_set.insert(vert);
                    }
                    self.shader_registrations.shader_root_set.insert(pair.frag);
                }
            }
            for entry in &compute {
                let users = self
                    .shader_registrations
                    .compute_entry_users
                    .entry(entry.key.clone())
                    .or_insert(0);
                *users += 1;
                if *users == 1 {
                    self.shader_registrations
                        .compute_entries
                        .insert(entry.key.clone(), entry.clone());
                    self.shader_registrations
                        .shader_root_set
                        .insert(entry.entry);
                }
            }
            self.shader_registrations
                .shader_contributions
                .insert(key, shader);
            self.shader_registrations
                .compute_contributions
                .insert(key, compute);
            let callsite = shader_callsite || compute_callsite;
            if callsite {
                self.shader_registrations.callsites.insert(key);
            } else {
                self.shader_registrations.callsites.remove(&key);
            }
        }
        let mut shader_roots: Vec<FuncRef> = self
            .shader_registrations
            .shader_root_set
            .iter()
            .copied()
            .collect();
        shader_roots.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
        self.shader_registrations.roots = shader_roots;
        self.shader_registrations.keys = self
            .shader_registrations
            .shader_pairs
            .keys()
            .cloned()
            .chain(self.shader_registrations.compute_entries.keys().cloned())
            .collect();
        self.repair_stats.shader_roots_examined = roots_examined;
        self.repair_stats.shader_roots_derived = roots_derived;
        Ok(true)
    }

    fn refresh_shader_registrations(&mut self, ws: &Workspace) -> Result<(), CompilerError> {
        let collection = skull_mir::lower::ShaderCollection {
            pairs: self
                .shader_registrations
                .shader_pairs
                .values()
                .cloned()
                .collect(),
            callsites: self
                .shader_registrations
                .callsites
                .iter()
                .copied()
                .collect(),
            contributions: self.shader_registrations.shader_contributions.clone(),
        };
        let programs = shader_programs(&collection);
        let compute_collection = skull_mir::lower::ComputeCollection {
            entries: self
                .shader_registrations
                .compute_entries
                .values()
                .cloned()
                .collect(),
            callsites: Vec::new(),
            contributions: self.shader_registrations.compute_contributions.clone(),
        };
        let compute = compute_programs(&compute_collection);
        let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
            .sharing(std::sync::Arc::clone(self.graph.interner()));
        let view = realize_shader_view(
            ws,
            &self.graph,
            &programs,
            &compute,
            &shader_scx,
            std::sync::Arc::clone(&self.concrete.plans),
        )?;
        let registrations = shader_registration_lines(
            ws,
            &shader_scx,
            &collection,
            &compute_collection,
            &view,
            self.renderer_target,
        )?;
        self.shader_view = view;
        self.shader_registrations = registrations;
        Ok(())
    }

    fn shader_delta_affects_view(&self, update: &crate::instance_graph::UpdateStats) -> bool {
        let shader_decls: FxHashSet<FuncRef> = self
            .shader_view
            .roots
            .iter()
            .chain(self.shader_view.helpers.iter())
            .chain(self.shader_view.builtins.iter())
            .map(|instance| instance.func)
            .collect();
        update
            .added
            .iter()
            .chain(update.removed.iter())
            .any(|instance| shader_decls.contains(&instance.func))
    }

    /// Take the specialized chunks an assembly BUILT into the cache, with their
    /// reverse-dependency edges, so the next build reuses them and an edit to any
    /// declaration they depend on -- their own included -- invalidates them.
    /// Bring the static baseline up to the current chunks: a changed body's cells
    /// take their current key, a cell not yet baselined joins, and a cell no chunk
    /// holds any more leaves.
    fn advance_static_baseline(&mut self, ws: &Workspace) -> Result<(), CompilerError> {
        let mut advanced: Vec<(Ustr, StaticBaseline)> = Vec::new();
        let mut current: FxHashSet<Ustr> = FxHashSet::default();
        for (body, chunk) in &self.chunks {
            let Ok(chunk) = chunk else { continue };
            let changed = self.changed_since_full.contains(body);
            for sc in &chunk.statics {
                current.insert(sc.cell);
                if !changed && self.static_key_baseline.contains_key(&sc.cell) {
                    continue;
                }
                let (value_slot, done_slot) = skull_mir::global_storage::static_cell_storage_keys(
                    ws,
                    self.scx.interner(),
                    sc,
                )
                .map_err(CompilerError::new_no_location)?;
                advanced.push((
                    sc.cell,
                    StaticBaseline {
                        key: static_cell_key(sc),
                        value_slot,
                        done_slot,
                    },
                ));
            }
        }
        self.static_key_baseline.extend(advanced);
        self.static_key_baseline
            .retain(|cell, _| current.contains(cell));
        Ok(())
    }

    fn absorb_spec_chunks(&mut self, built: Vec<(Instance, Chunk)>) -> usize {
        let mut dependency_edges = 0;
        for (body, chunk) in built {
            dependency_edges += chunk.deps.len();
            for d in &chunk.deps {
                self.dependents.entry(*d).or_default().insert(body.clone());
            }
            self.chunks.insert(body, Ok(chunk));
        }
        dependency_edges
    }

    /// Drop every trace of an instance the graph pruned: its chunk, its
    /// reverse-dependency edges, and its membership in the overlay/emitted sets.
    ///
    /// Leaving the chunk behind is not harmless -- `emit_incremental` reassembles
    /// from the cache, so a stale entry re-emits a definition no call reaches, and
    /// `changed_since_full` would keep offering it to the overlay forever.
    fn forget_instance(&mut self, gone: &Instance) {
        if let Some(Ok(c)) = self.chunks.get(gone) {
            let deps: Vec<FuncRef> = c.deps.iter().copied().collect();
            for d in deps {
                if let Some(set) = self.dependents.get_mut(&d) {
                    set.remove(gone);
                }
            }
        }
        self.chunks.remove(gone);
        self.changed_since_full.remove(gone);
        self.emitted.remove(gone);
    }

    /// Materialize the bodies a graph update newly reached. The graph owns both
    /// the concrete substitution and the resolved iterator witnesses; the cache
    /// only stores the chunks those facts produce.
    fn build_graph_added_chunks(
        cache: &mut SkullEmitCache,
        ws: &Workspace,
        added: &std::collections::BTreeSet<Instance>,
    ) -> (
        Vec<CompilerError>,
        FxHashSet<skull_mir::code_identity::CodeIdentity>,
    ) {
        let mut fingerprint_identities = FxHashSet::default();
        let mut pending: Vec<Instance> = added.iter().cloned().collect();
        let mut seen: FxHashSet<Instance> = FxHashSet::default();
        let mut built: Vec<(Instance, Chunk)> = Vec::new();
        let mut errs = Vec::new();

        while let Some(body) = pending.pop() {
            if !seen.insert(body.clone()) {
                continue;
            }
            let f = &ws.module(body.func.module).functions[body.func.func];
            if !is_cached_emitted_body(f, &body) {
                continue;
            }
            if let Some(Ok(chunk)) = cache.chunks.get(&body) {
                if !cache.emitted.contains(&body) {
                    cache.changed_since_full.insert(body.clone());
                    fingerprint_identities.extend(chunk.code_artifact.entries.keys().cloned());
                }
                continue;
            }
            let chunk = if body.args.is_empty() {
                match rebuild_chunk(
                    ws,
                    &cache.cg,
                    &cache.scx,
                    &cache.concrete,
                    &body,
                    f,
                    &mut cache.inline_cache,
                ) {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        errs.push(err);
                        continue;
                    }
                }
            } else {
                let name = body.symbol(ws, cache.scx.interner());
                let mut specs = Vec::new();
                match emit_spec_chunk(
                    ws,
                    &cache.cg,
                    &cache.scx,
                    &cache.concrete,
                    &body,
                    &name,
                    &mut cache.inline_cache,
                    &mut specs,
                ) {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        errs.push(err);
                        continue;
                    }
                }
            };
            let mut valid = true;
            for demand in &chunk.specs {
                if cache.graph.concreteness(&demand.inst)
                    == crate::instance_graph::Concreteness::Missing
                {
                    errs.push(CompilerError::new_with_label(
                        f.name_span,
                        format!(
                            "graph omitted added specialization `{}` demanded by `{}`",
                            demand.inst.symbol(ws, cache.scx.interner()),
                            body.symbol(ws, cache.scx.interner())
                        ),
                    ));
                    valid = false;
                    continue;
                }
                pending.push(demand.inst.clone());
            }
            if valid {
                built.push((body, chunk));
            }
        }

        for (body, chunk) in built {
            fingerprint_identities.extend(chunk.code_artifact.entries.keys().cloned());
            for dep in &chunk.deps {
                cache
                    .dependents
                    .entry(*dep)
                    .or_default()
                    .insert(body.clone());
            }
            cache.chunks.insert(body.clone(), Ok(chunk));
            cache.changed_since_full.insert(body);
        }
        (errs, fingerprint_identities)
    }

    fn combine_errors(mut errors: Vec<CompilerError>) -> Option<CompilerError> {
        let first = errors.first()?.clone();
        errors.remove(0);
        Some(first.with_siblings(errors))
    }

    fn update_chunk_cache(
        &mut self,
        ws: &Workspace,
        reinferred: &[(ModuleId, BodyOwner)],
        edits: &[crate::instance_graph::SpanEdit],
    ) -> Result<
        (
            FxHashSet<Instance>,
            crate::instance_graph::UpdateStats,
            Vec<CompilerError>,
            FxHashSet<skull_mir::code_identity::CodeIdentity>,
        ),
        CompilerError,
    > {
        let cache = self;
        let changed_literals = if reinferred
            .iter()
            .any(|(_, owner)| matches!(owner, BodyOwner::Global(_)))
        {
            cache.cg.refresh_scalar_constants(ws)
        } else {
            FxHashSet::default()
        };
        let mut fingerprint_identities = FxHashSet::default();
        let shader_delta_requests = cache.repair_stats.shader_delta_requests;
        let shader_roots_examined = cache.repair_stats.shader_roots_examined;
        let shader_roots_derived = cache.repair_stats.shader_roots_derived;
        cache.repair_stats = IncrementalRepairStats {
            body_only_edits: 1,
            shader_delta_requests,
            shader_roots_examined,
            shader_roots_derived,
            ..IncrementalRepairStats::default()
        };
        // Fold each re-inferred body's value-reference contribution into the cached
        // ScalarCtx. A name whose taken-by-ref membership flips changes scalar-ABI
        // eligibility, so the named fn(s) -- and, via the deps scan below, every
        // direct caller -- must be re-emitted. `flipped_frefs` seeds `changed`.
        let mut flipped_frefs: FxHashSet<FuncRef> = FxHashSet::default();
        for (mid, owner) in reinferred {
            for name in cache.scx.update_body(ws, *mid, *owner) {
                if let Some(list) = ws.functions_by_name.get(&name) {
                    for (m, fid) in list.iter() {
                        flipped_frefs.insert(FuncRef {
                            module: *m,
                            func: *fid,
                        });
                    }
                }
            }
        }
        // **THE GRAPH IS MAINTAINED BEFORE THE CHUNK SET IS DECIDED.**
        //
        // A body edit can add an instantiation, strand one, or both, and only the
        // graph knows which -- the chunk cache is downstream of that answer, not a
        // second derivation of it. `update_bodies` re-walks the owners the edit
        // invalidated against the SAME monotonic arena every key here indexes, so
        // ids stay stable and the added/removed sets below are directly usable as
        // cache keys.
        //
        // Rebuilding the graph instead of updating it is what the arc measured and
        // rejected: 15-33 ms release against a sub-millisecond edit.
        let t_graph = Instant::now();
        let graph_builds_before = crate::instance_graph::build_invocations();
        cache.repair_stats.root_requests = 1;
        let previous_shader_roots = cache.roots.shader_roots.clone();
        let current_shader_roots = cache.shader_registrations.roots.clone();
        let shader_roots_changed = previous_shader_roots != current_shader_roots;
        let mut root_added = Vec::new();
        let mut root_removed = Vec::new();
        if shader_roots_changed {
            let mut current_roots = cache.roots.roots.clone();
            for root in previous_shader_roots
                .iter()
                .filter(|root| !current_shader_roots.contains(root))
            {
                if cache.roots.interface_roots.contains(root)
                    || cache.roots.sim_roots.contains(root)
                {
                    continue;
                }
                if let Some(position) = current_roots.iter().position(|current| current == root) {
                    current_roots.remove(position);
                    root_removed.push(*root);
                }
            }
            for root in current_shader_roots
                .iter()
                .filter(|root| !previous_shader_roots.contains(root))
            {
                if !current_roots.contains(root) {
                    current_roots.push(*root);
                    root_added.push(*root);
                }
            }
            current_roots.sort_by_key(|root| (root.module.index(), root.func.into_raw()));
            cache.roots.roots = current_roots;
        }
        cache.roots.shader_roots = current_shader_roots;
        // The full root enumerator remains an oracle for tests and verification;
        // the body route consumes the retained root cache above.
        let forced_roots = if std::env::var_os("SKULL_FORCE_ROOT_SCAN").is_some() {
            let mut census = RootEnumerationCensus::default();
            let roots = all_roots_with_census(
                ws,
                cache.roots.project_root.as_deref(),
                &cache.roots.shader_roots,
                Some(&mut census),
            );
            cache.repair_stats.root_functions_examined = census.functions_examined;
            cache.repair_stats.root_modules_examined = census.modules_examined;
            cache.repair_stats.root_reseeds = census.roots_reseeded;
            let previous_roots = cache.roots.roots.clone();
            root_added = roots
                .iter()
                .filter(|root| !previous_roots.contains(root))
                .copied()
                .collect();
            root_removed = previous_roots
                .iter()
                .filter(|root| !roots.contains(root))
                .copied()
                .collect();
            Some(roots)
        } else {
            None
        };
        let update = cache.graph.update_bodies_with_root_delta(
            ws,
            &root_added,
            &root_removed,
            reinferred,
            edits,
        );
        cache.repair_stats.graph_update_ns = t_graph.elapsed().as_nanos() as u64;
        cache.repair_stats.full_graph_rebuilds =
            crate::instance_graph::build_invocations().saturating_sub(graph_builds_before) as usize;
        cache.repair_stats.owners_revisited = update.revisited_owners.len();
        cache.repair_stats.instances_revisited = update.revisited.len();
        cache.repair_stats.graph_contributions_retracted = update.retracted_owners.len();
        cache.repair_stats.graph_contributions_installed = update.revisited_owners.len();
        cache.repair_stats.broad_owner_scans = update.broad_scans;
        cache.repair_stats.root_hole_names_examined = update.census.root_hole_names_examined;
        cache.repair_stats.root_hole_rows_examined = update.census.root_hole_rows_examined;
        cache.repair_stats.root_hole_decls_examined = update.census.root_hole_decls_examined;
        cache.repair_stats.root_hole_reason_buckets_examined =
            update.census.root_hole_reason_buckets_examined;
        cache.repair_stats.root_instances_checked = update.census.root_instances_checked;
        cache.repair_stats.erased_catalogue_entries_examined =
            update.census.erased_catalogue_entries_examined;
        let concrete_builds_before = crate::concrete_body::build_invocations();
        let t_concrete = Instant::now();
        let concrete_update = cache.concrete.update_local_with_span_edits(
            ws,
            &cache.graph,
            &update.revisited,
            &update.removed,
            &update.revisited_owners,
            edits,
        );
        cache.repair_stats.concrete_entries_derived = concrete_update.derived;
        cache.repair_stats.concrete_entries_reused = concrete_update.reused;
        cache.repair_stats.concrete_entries_retracted = concrete_update.retracted;
        cache.repair_stats.full_concrete_rebuilds = crate::concrete_body::build_invocations()
            .saturating_sub(concrete_builds_before)
            as usize;
        cache.repair_stats.broad_concrete_scans = concrete_update.broad_scans;
        cache.repair_stats.concrete_update_ns = t_concrete.elapsed().as_nanos() as u64;
        let _graph_ms = Duration::from_nanos(
            cache.repair_stats.graph_update_ns
                + cache.repair_stats.concrete_update_ns
                + cache.repair_stats.artifact_update_ns,
        );

        // Before any borrow of the cache's own fields, because it mutates several.
        for gone in &update.removed {
            cache.forget_instance(gone);
        }

        // The graph moved; retract and install only the affected target subjects.
        // The retained view owns the predecessor/successor indexes needed to
        // update reachability without realizing the full graph again.
        let t_artifact = Instant::now();
        cache.repair_stats.artifact_iteration_requests = update
            .revisited
            .len()
            .saturating_add(update.added.len())
            .saturating_add(update.removed.len());
        let artifact_realize_before = crate::artifact::realize_invocations();
        if std::env::var_os("SKULL_FORCE_ARTIFACT_REALIZE").is_some() {
            let _ = crate::artifact::realize(
                ws,
                &cache.graph,
                &crate::artifact::ArtifactRequest::new(
                    crate::target_projection::LUAJIT_DESKTOP,
                    forced_roots
                        .as_deref()
                        .unwrap_or(cache.roots.roots.as_slice()),
                )
                .with_scalar_context(&cache.scx),
            );
        }
        let artifact_update = cache.lua_view.update_local_with_root_delta(
            ws,
            &cache.graph,
            &crate::artifact::ArtifactRequest::new(
                crate::target_projection::LUAJIT_DESKTOP,
                forced_roots
                    .as_deref()
                    .unwrap_or(cache.roots.roots.as_slice()),
            )
            .with_scalar_context(&cache.scx),
            &update.revisited,
            &update.added,
            &update.removed,
            &root_added,
            &root_removed,
        );
        cache.repair_stats.artifact_chunks_revisited = artifact_update.revisited;
        cache.repair_stats.artifact_chunks_added = artifact_update.added;
        cache.repair_stats.artifact_chunks_removed = artifact_update.removed;
        cache.repair_stats.artifact_iterations = artifact_update.iterations;
        cache.repair_stats.artifact_root_entries_examined = artifact_update.root_entries_examined;
        cache.repair_stats.artifact_index_rows_examined = artifact_update.local_index_rows_examined;
        cache.repair_stats.artifact_scalar_context_builds = artifact_update.scalar_context_builds;
        cache.repair_stats.artifact_realize_invocations =
            crate::artifact::realize_invocations().saturating_sub(artifact_realize_before) as usize;
        cache.repair_stats.full_artifact_view_realizations =
            cache.repair_stats.artifact_realize_invocations;
        cache.repair_stats.broad_artifact_scans = artifact_update.broad_scans;
        cache.repair_stats.artifact_update_ns = t_artifact.elapsed().as_nanos() as u64;

        let scx = &cache.scx;
        let cg = &cache.cg;

        let _t_aff = Instant::now();
        let mut changed: FxHashSet<FuncRef> = reinferred
            .iter()
            .filter_map(|(mid, owner)| match owner {
                BodyOwner::Function(fid) => Some(FuncRef {
                    module: *mid,
                    func: *fid,
                }),
                _ => None,
            })
            .collect();
        // Fns whose scalar ABI flipped (a body now takes them by ref, or stopped) must
        // re-emit even if their own body didn't change.
        changed.extend(flipped_frefs.iter().copied());

        // A chunk whose baked comptime input (build_var value / embedded file
        // content) no longer matches the CURRENT inputs is a body edit bone cannot
        // see -- reinferred hashes only source text. Seed
        // the decl so the chunk and every dependent (a caller that inlined the
        // value) re-emits fresh.
        {
            let mut file_hashes: FxHashMap<Ustr, Option<u64>> = FxHashMap::default();
            for (body, chunk) in cache.chunks.iter() {
                let Ok(chunk) = chunk else { continue };
                if comptime_reads_stale(ws, &chunk.comptime_reads, &mut file_hashes) {
                    changed.insert(body.func);
                }
            }
        }

        let mut invalidated_candidate_frefs = changed.clone();
        invalidated_candidate_frefs.extend(update.added.iter().map(|instance| instance.func));
        invalidated_candidate_frefs.extend(update.removed.iter().map(|instance| instance.func));
        let mut warm_keys = concrete_update.candidate_keys_added.clone();
        for fref in &invalidated_candidate_frefs {
            if let Some(keys) = cache.candidate_index_by_func().get(fref) {
                warm_keys.extend(keys.iter().cloned());
            }
        }
        let candidate_entries_requested = warm_keys.len();
        let candidate_entries_invalidated = concrete_update.candidate_keys_removed.len()
            + warm_keys
                .iter()
                .filter(|key| {
                    !concrete_update.candidate_keys_added.contains(*key)
                        && cache.concrete.candidate_keys().contains(*key)
                })
                .count();
        let warm_keys: Vec<Instance> = warm_keys.into_iter().collect();
        let t_candidate = Instant::now();
        let previous_candidate_table = cache.inline_cache.candidate_table();
        let force_candidate_scan = std::env::var_os("SKULL_FORCE_CANDIDATE_SCAN").is_some();
        let mut candidate_census = CandidateBuildStats::default();
        let candidate_table = build_inline_candidate_table_with_census(
            ws,
            scx,
            &cache.concrete,
            if force_candidate_scan {
                cache.concrete.candidate_keys().iter().cloned().collect()
            } else {
                Vec::new()
            },
            previous_candidate_table.as_ref(),
            Some(&invalidated_candidate_frefs),
            (!force_candidate_scan).then_some(warm_keys),
            Some(&concrete_update.candidate_keys_added),
            Some(&concrete_update.candidate_keys_removed),
            &mut candidate_census,
        )?;
        // A residual call embeds no callee body. It can keep its chunk when every
        // concrete candidate for that declaration has the same decision inputs.
        // Unknown/rejected candidates and graph/ABI transitions stay conservative.
        let mut stable_residual_frefs = FxHashSet::default();
        if let Some(previous) = previous_candidate_table.as_ref() {
            for fref in &changed {
                if flipped_frefs.contains(fref)
                    || update
                        .added
                        .iter()
                        .chain(&update.removed)
                        .any(|key| key.func == *fref)
                    || concrete_update
                        .candidate_keys_added
                        .iter()
                        .chain(&concrete_update.candidate_keys_removed)
                        .any(|key| key.func == *fref)
                {
                    continue;
                }
                if let Some(keys) = cache.candidate_index_by_func().get(fref) {
                    if !keys.is_empty()
                        && keys
                            .iter()
                            .all(|key| previous.residual_eligibility_matches(&candidate_table, key))
                    {
                        stable_residual_frefs.insert(*fref);
                    }
                }
            }
        }
        cache.inline_cache.replace_candidate_table(candidate_table);
        cache.repair_stats.candidate_entries_requested = candidate_entries_requested;
        cache.repair_stats.candidate_entries_invalidated = candidate_entries_invalidated;
        cache.repair_stats.candidate_entries_derived = candidate_census.warmed;
        cache.repair_stats.candidate_entries_reused =
            candidate_entries_requested.saturating_sub(candidate_census.warmed);
        cache.repair_stats.broad_candidate_scans = candidate_census.keys_examined;
        cache.repair_stats.candidate_update_ns = t_candidate.elapsed().as_nanos() as u64;
        // **A changed declaration maps to its LIVE INSTANCES through the graph**,
        // not to one declaration-shaped key. Editing a generic `co fn` with two
        // instantiations must re-emit both bodies, and editing one specialization's
        // declaration must not be indistinguishable from editing the other's.
        // `Instance::base` is still taken for every changed fn: a non-generic one
        // IS its base instance, and a declaration the graph never reached still has
        // a chunk here that an edit can invalidate.
        let mut affected: FxHashSet<Instance> = changed.iter().copied().map(shared).collect();
        if !changed_literals.is_empty() {
            affected.extend(cache.chunks.iter().filter_map(|(body, chunk)| {
                chunk
                    .as_ref()
                    .ok()
                    .filter(|chunk| {
                        chunk
                            .literal_globals
                            .iter()
                            .any(|name| changed_literals.contains(name))
                    })
                    .map(|_| body.clone())
            }));
        }
        cache.repair_stats.declaration_lookup_requests = changed.len();
        for c in &changed {
            let instances: Vec<Instance> = cache.graph.instances_of(*c).cloned().collect();
            cache.repair_stats.declaration_instance_lookups += instances.len();
            affected.extend(instances);
        }
        // ...plus every chunk that DEPENDS on a changed fn. This is the CODEGEN
        // reverse graph, which the instance graph does not hold and must not be
        // confused with `Indices::preds`: `preds` is call edges between instances,
        // while `deps` records that a chunk's TEXT contains another declaration's
        // body (an inline splice) or specializes it. Neither is derivable from the
        // other, so both survive.
        for c in &changed {
            if let Some(deps) = cache.dependents.get(c) {
                cache.repair_stats.dependent_requests += 1;
                cache.repair_stats.dependents_visited += deps.len();
                affected.extend(
                    deps.iter()
                        .filter(|body| {
                            !stable_residual_frefs.contains(c)
                                || !matches!(cache.chunks.get(*body), Some(Ok(chunk))
                        if !chunk.embedded_deps.contains(c) && !chunk.abi_deps.contains(c))
                        })
                        .cloned(),
                );
            }
        }
        // REMOVAL is the graph's to report and emission's alone cannot see it:
        // nothing on this side knows an instance was stranded, so a surviving chunk
        // makes `emit_incremental` re-emit a definition no call reaches. That is
        // handled above, before the borrows.
        //
        // Construction of graph additions happens after invalidation below, so a
        // newly reached body cannot reuse an inline decision made for the old body.
        cache.repair_stats.affected_selection_ns = _t_aff.elapsed().as_nanos() as u64;
        cache.repair_stats.chunk_rebuild_requests = affected.len();
        let _aff_ms = Duration::from_nanos(cache.repair_stats.affected_selection_ns);

        // The persistent InlineCache keeps unchanged callee bodies across edits; the
        // affected entries are exactly those whose lowered body could have changed
        // (own body edit, own ABI flip, or a callee's ABI flip -- all within
        // `affected`), so invalidate them before re-lowering. Everything else is
        // provably unchanged and reused.
        let _t_relow = Instant::now();
        let mut chunks_rebuilt = 0;
        for body in &affected {
            cache.inline_cache.invalidate(body.func);
        }
        for body in &affected {
            // Drop this chunk's OLD reverse-dep edges -- re-emitting may change its
            // deps, and a cached->regenerated flip removes the chunk entirely.
            let old_deps: Vec<FuncRef> = match cache.chunks.get(body) {
                Some(Ok(c)) => c.deps.iter().copied().collect(),
                _ => Vec::new(),
            };
            for d in &old_deps {
                if let Some(set) = cache.dependents.get_mut(d) {
                    set.remove(body);
                }
            }
            let fref = body.func;
            let f = &ws.module(fref.module).functions[fref.func];
            // An edit can flip a body between cached (ordinary/coroutine) and
            // regenerated (extern / inline(lua)); only the former has a chunk.
            if is_cached_emitted_body(f, body) {
                chunks_rebuilt += 1;
                let chunk = rebuild_chunk(
                    ws,
                    cg,
                    scx,
                    &cache.concrete,
                    body,
                    f,
                    &mut cache.inline_cache,
                );
                if let Ok(c) = &chunk {
                    fingerprint_identities.extend(c.code_artifact.entries.keys().cloned());
                    let new_deps: Vec<FuncRef> = c.deps.iter().copied().collect();
                    for d in new_deps {
                        cache.dependents.entry(d).or_default().insert(body.clone());
                    }
                }
                cache.chunks.insert(body.clone(), chunk);
            } else {
                cache.chunks.remove(body);
            }
        }
        cache.repair_stats.chunks_rebuilt = chunks_rebuilt;
        let (added_errors, added_fingerprint_identities) =
            Self::build_graph_added_chunks(cache, ws, &update.added);
        fingerprint_identities.extend(added_fingerprint_identities);
        cache.repair_stats.relowering_ns = _t_relow.elapsed().as_nanos() as u64;
        if std::env::var("SKULL_EMIT_PROFILE").is_ok() {
            eprintln!(
                "    update_chunk_cache: graph {:.3}ms (+{} -{} instances) | affected-scan {:.3}ms (O(edit), {} chunks) | re-lower {:.3}ms ({} affected)",
                _graph_ms.as_secs_f64() * 1000.0,
                update.added.len(),
                update.removed.len(),
                _aff_ms.as_secs_f64() * 1000.0,
                cache.chunks.len(),
                _t_relow.elapsed().as_secs_f64() * 1000.0,
                affected.len(),
            );
        }
        Ok((affected, update, added_errors, fingerprint_identities))
    }
}

pub fn emit_incremental(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> Result<MirEmit, CompilerError> {
    let t_reemit = Instant::now();
    // Shader registrations FIRST: their dependencies are graph/view roots, so
    // `update_chunk_cache`'s re-walk must see this edit's set, not the last
    // build's.
    let shader_changed = cache.update_shader_registrations(ws, reinferred)?;
    let (_affected, update, added_errors, _fingerprint_identities) =
        cache.update_chunk_cache(ws, reinferred, &[])?;
    if let Some(err) = SkullEmitCache::combine_errors(added_errors) {
        return Err(err);
    }
    if shader_changed || cache.shader_delta_affects_view(&update) {
        cache.refresh_shader_registrations(ws)?;
    }
    let reemit = t_reemit.elapsed();

    // Reassembly: the SAME work `emit_full` does (concat every cached chunk +
    // re-emit globals/decorators/prelude/entry), scaling with total output size.
    // The two-file split (`emit_overlay`) avoids it entirely on a body edit.
    let t_asm = Instant::now();
    let mut built_specs: Vec<(Instance, Chunk)> = Vec::new();
    let (mut emit, emitted, unit_reads) =
        assemble(ws, cache, &mut built_specs, CacheLifetime::Retained)?;
    if concrete_stats_requested() {
        emit.concrete_stats = cache.concrete.snapshot_build_stats();
    }
    cache.emitted = emitted;
    cache.code_artifact = emit.code_artifact.clone();
    cache.lifecycle_plan = Some(std::sync::Arc::new(emit.lifecycle_plan.clone()));
    cache.global_layout = Some(emit.global_layout.clone());
    cache.definition_plan = Some(std::sync::Arc::new(emit.definition_plan.clone()));
    cache.artifact_fingerprint = Some(emit.artifact_fingerprint.clone());
    cache.fingerprint_code_rows = Some(
        crate::artifact_fingerprint::CodeRows::collect(
            ws,
            cache.scx.interner(),
            &emit.code_artifact,
        )
        .map_err(|error| CompilerError::new_no_location(error.to_string()))?,
    );
    cache.unit_comptime_reads = unit_reads;
    cache.absorb_spec_chunks(built_specs);
    emit.compile_counts.chunks = cache.chunks.len();
    let reassemble = t_asm.elapsed();
    emit.phases.push(("incr: re-emit affected bodies", reemit));
    emit.phases
        .push(("incr: reassemble full output", reassemble));
    // A codegen rejection (a newly-colliding synthetic name, a body an edit
    // pushed over a LuaJIT frame limit) is recorded on the shared `CodegenCtx`
    // rather than returned by `emit_named`, so it only surfaces where the sink
    // is drained. `emit_full` and `emit_overlay` both drain; without this the
    // incremental path would publish the rejected body and the diagnostic would
    // appear at some later, unrelated emit
    //.
    if let Some(err) = cache.cg.take_rejection() {
        return Err(err);
    }
    Ok(emit)
}

/// Recreate only factories whose captures changed, in the same order as a full
/// build. Rebound callers must also publish their escaping CodeRef slots.
fn overlay_callee_bindings(
    ws: &Workspace,
    cache: &SkullEmitCache,
    changed: &[Instance],
) -> (
    Vec<skull_codegen_lua::CalleeBindingPlan>,
    FxHashSet<Instance>,
) {
    let mut names: FxHashSet<String> = changed
        .iter()
        .map(|body| body.symbol(ws, cache.scx.interner()))
        .collect();
    for body in changed {
        if let Some(Ok(chunk)) = cache.chunks.get(body) {
            names.extend(chunk.binds.iter().map(|plan| plan.name.clone()));
        }
    }
    let mut candidates: Vec<_> = cache
        .chunks
        .iter()
        .filter(|(body, _)| cache.lua_view.instances.contains(*body))
        .filter_map(|(body, chunk)| chunk.as_ref().ok().map(|chunk| (body, chunk)))
        .flat_map(|(body, chunk)| chunk.binds.iter().map(move |plan| (body, plan)))
        .collect();
    candidates.sort_by(|(_, a), (_, b)| a.rank.cmp(&b.rank).then_with(|| a.name.cmp(&b.name)));
    let mut bindings = Vec::new();
    let mut bodies = FxHashSet::default();
    for (body, plan) in candidates {
        if names.contains(&plan.name) || plan.captures.iter().any(|name| names.contains(name)) {
            names.insert(plan.name.clone());
            bodies.insert(body.clone());
            bindings.push(plan.clone());
        }
    }
    (bindings, bodies)
}

/// Assemble the incremental OVERLAY (`incremental.lua`): the cached chunks of the
/// functions changed since the last full build, then slot publication for their
/// current CodeRefs. Registries retain those logical values across the overlay.
/// Loaded AFTER `main.lua`; redefined globals override their stale defs. Emits NO
/// scaffold/globals -- those are unchanged on a body edit and live in main.lua.
fn assemble_overlay(
    ws: &Workspace,
    cache: &SkullEmitCache,
    shader_changed: bool,
    initialize_code_slots: bool,
    mut fingerprint_identities: FxHashSet<skull_mir::code_identity::CodeIdentity>,
) -> Result<
    (
        MirEmit,
        UnitComptimeReads,
        crate::artifact_fingerprint::CodeRows,
        usize,
    ),
    CompilerError,
> {
    let t_clone_artifact = Instant::now();
    let mut out = String::from("-- skull incremental overlay (loaded after main.lua)\n");
    if initialize_code_slots {
        out.push_str(skull_codegen_lua::FN_MT_PRELUDE);
    }
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut candidate_code_artifact = cache.code_artifact.clone();
    let d_clone_artifact = t_clone_artifact.elapsed();
    out.push_str("do\nlocal __rock_state_cell = __rock_globals\n");
    let mut unit_reads = UnitComptimeReads::default();

    // Redefine the changed functions, deterministic order for a stable diff.
    let t_definitions = Instant::now();
    let mut order: Vec<Instance> = cache.changed_since_full.iter().cloned().collect();
    order.sort_by_key(|b| overlay_order_key(&cache.scx, b));
    order.dedup();
    let mut definitions = LuaDefinitionBuffer::new();
    for body in &order {
        match cache.chunks.get(body) {
            Some(Ok(chunk)) => definitions.push_str(&chunk.lua),
            Some(Err(err)) => {
                let fref = body.func;
                let f = &ws.module(fref.module).functions[fref.func];
                skipped.push((mangle_fn(ws, f), err.message.clone()));
            }
            // extern / inline(lua) edits are interface-tier -> full rebuild path.
            None => {}
        }
    }
    definitions.append_to(&mut out);
    let (mut bindings, rebound_bodies) = overlay_callee_bindings(ws, cache, &order);
    out.push_str(&callee_binding_pass_lua(&mut bindings));
    let mut published_bodies = order.clone();
    published_bodies.extend(rebound_bodies);
    let overlay_code_artifact = overlay_code_artifact(ws, cache, &published_bodies);

    let d_definitions = t_definitions.elapsed();
    let t_code_slot_stage = Instant::now();
    let has_code_slots = cache
        .code_artifact
        .entries
        .values()
        .any(|entry| entry.escaping);
    let code_slot_stage = code_slot_registration_lua_entries(
        ws,
        &cache.scx,
        overlay_code_artifact.entries.values(),
        has_code_slots,
    )?;
    out.push_str(&code_slot_stage);
    let d_code_slot_stage = t_code_slot_stage.elapsed();
    // Reset `static` cells whose initializer MEANING changed since the last full
    // build (`type@init_hash` key differs from the baseline). A static is
    // name-keyed and lazy, so redefining its fn above leaves the warmed cell +
    // set done-flag intact -- the redefined fn would re-read the stale value.
    // Re-run the initializer transition so the next reach re-inits with the
    // edited initializer; an unchanged key is skipped (an unrelated body edit
    // preserves the live cell). Only the changed fns' statics are considered --
    // a static's key can't change without its enclosing fn being re-lowered into
    // `changed_since_full`. The resets run in the publication step, so an
    // overlay that fails to load leaves every static as it was.
    let t_static_resets = Instant::now();
    let mut static_resets = String::new();
    let mut seen_static: FxHashSet<Ustr> = FxHashSet::default();
    for body in &order {
        let Some(Ok(chunk)) = cache.chunks.get(body) else {
            continue;
        };
        for sc in &chunk.statics {
            if !seen_static.insert(sc.cell) {
                continue;
            }
            let changed = cache
                .static_key_baseline
                .get(&sc.cell)
                .is_none_or(|base| base.key != static_cell_key(sc));
            if changed {
                let (value_slot, done_slot) = skull_mir::global_storage::static_cell_storage_keys(
                    ws,
                    cache.scx.interner(),
                    sc,
                )
                .map_err(CompilerError::new_no_location)?;
                let _ = writeln!(
                    static_resets,
                    "__rock_static_prepare({value_slot:?}, {done_slot:?}, {:?})",
                    static_cell_key(sc),
                );
            }
        }
    }
    // A static an edit removed is in the baseline but in no current chunk (a
    // generic's static can sit in several callers' chunks, so absence from the
    // edited body alone does not remove it). Its cells are dropped so they stop
    // holding their last value.
    if !cache.static_key_baseline.is_empty() {
        let current: FxHashSet<Ustr> = cache
            .chunks
            .values()
            .flatten()
            .flat_map(|chunk| chunk.statics.iter().map(|sc| sc.cell))
            .collect();
        let mut dropped: Vec<&StaticBaseline> = cache
            .static_key_baseline
            .iter()
            .filter(|(cell, _)| !current.contains(*cell))
            .map(|(_, base)| base)
            .collect();
        dropped.sort_by(|a, b| a.value_slot.cmp(&b.value_slot));
        for base in dropped {
            let _ = writeln!(
                static_resets,
                "__rock_static_drop({:?}, {:?})",
                base.value_slot, base.done_slot
            );
        }
    }
    let d_static_resets = t_static_resets.elapsed();

    // Re-emit only the AFFECTED `const` inits when a const changed since the last
    // full build: the edited const(s) PLUS their transitive DEPENDENTS (consts
    // whose init reads a changed const, directly or through a function). An
    // unrelated const is left untouched in main.lua. The dependency graph is the
    // SAME symbol-reference info goto-def / references / the re-inference cascade
    // use -- `BodyInference.deps` (the names each body references). Globals are
    // ALWAYS re-inferred on a body edit, so these deps are fresh (no caching, no
    // staleness). Each affected const emits an UNGUARDED
    // `<NAME> = __skull_ginit_<NAME>()` line in dependency order. A dependent's
    // ginit can embed an upstream scalar literal, so its definition is refreshed
    // along with the edited const. Mutable globals are never re-emitted:
    // their guard preserves the live value across reload.
    let t_const_inits = Instant::now();
    if !cache.changed_consts_since_full.is_empty() {
        // Forward symbol-reference graph (name -> names it references) from the
        // CURRENT inference. Overloads merge under one name (conservative: at worst
        // re-emits an extra const, never misses one).
        let mut deps: FxHashMap<Ustr, FxHashSet<Ustr>> = FxHashMap::default();
        for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
            let Some(inf) = ws.inference(module.id) else {
                continue;
            };
            for (fid, f) in module.functions.iter() {
                if let Some(bi) = inf.per_fn.get(&BodyOwner::Function(fid)) {
                    deps.entry(f.name)
                        .or_default()
                        .extend(bi.deps.iter().copied());
                }
            }
            for (gid, g) in module.globals.iter() {
                if let Some(bi) = inf.per_fn.get(&BodyOwner::Global(gid)) {
                    deps.entry(g.name)
                        .or_default()
                        .extend(bi.deps.iter().copied());
                }
            }
        }

        let changed_names: FxHashSet<Ustr> = cache
            .changed_consts_since_full
            .iter()
            .map(|&(mid, gid)| ws.module(mid).globals[gid].name)
            .collect();

        // Every const; affected = itself changed OR it transitively references a
        // changed const. `reach` (its transitive refs) is reused for the sort.
        struct Affected {
            mid: ModuleId,
            gid: Idx<bone_semantic::hir::GlobalDef>,
            name: Ustr,
            reach: FxHashSet<Ustr>,
        }
        let mut const_items: Vec<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)> = ws
            .modules
            .iter()
            .filter(|m| m.id != ModuleId::builtin())
            .flat_map(|m| {
                m.globals
                    .iter()
                    .filter(|(_, g)| g.is_const)
                    .map(move |(gid, _)| (m.id, gid))
            })
            .collect();
        const_items.sort_by_key(|(m, g)| (m.index(), u32::from(g.into_raw())));

        let mut affected: Vec<Affected> = Vec::new();
        for (mid, gid) in const_items {
            let name = ws.module(mid).globals[gid].name;
            let reach = forward_reach(&deps, name);
            let changed = changed_names.contains(&name);
            if changed || reach.iter().any(|n| changed_names.contains(n)) {
                affected.push(Affected {
                    mid,
                    gid,
                    name,
                    reach,
                });
            }
        }
        let affected_names: FxHashSet<Ustr> = affected.iter().map(|a| a.name).collect();

        // Emit each affected const's ginit fn now; build its init
        // line + dependency edges (restricted to affected consts) for the sort.
        let mut inits: Vec<GlobalInit> = Vec::new();
        let mut definitions = LuaDefinitionBuffer::new();
        for a in &affected {
            let line = {
                let unit =
                    emit_global_unit(ws, &cache.scx, &cache.cg, &cache.concrete, a.mid, a.gid);
                fingerprint_identities.extend(unit.code_artifact.entries.keys().cloned());
                candidate_code_artifact = std::sync::Arc::new(
                    candidate_code_artifact.with_replacements(&[&unit.code_artifact]),
                );
                if let Some(err) = unit.hard_error {
                    return Err(err);
                }
                unit_reads
                    .globals
                    .insert((a.mid, a.gid), unit.comptime_reads.clone());
                definitions.push_str(&unit.lua);
                if let Some(s) = unit.skipped {
                    skipped.push(s);
                }
                unit.pending_init.map(|(_, _, line)| line).ok_or_else(|| {
                    CompilerError::new_no_location(format!(
                        "changed global `{}` produced no initializer plan",
                        a.name
                    ))
                })?
            };
            let edges: Vec<Ustr> = a
                .reach
                .iter()
                .copied()
                .filter(|n| *n != a.name && affected_names.contains(n))
                .collect();
            inits.push(GlobalInit {
                name: a.name,
                deps: edges,
                line,
            });
        }
        definitions.append_to(&mut out);

        match sort_global_init_lines(inits) {
            Ok(lines) => out.push_str(&lines.concat()),
            Err(e) => skipped.push(("const init ordering".to_string(), e.message)),
        }
    }
    let d_const_inits = t_const_inits.elapsed();

    if shader_changed {
        out.push_str(&cache.shader_registrations.lua);
    }
    let t_lifecycle = Instant::now();
    let lifecycle_plan = skull_mir::lifecycle::collect_with_artifact(ws, &candidate_code_artifact)?;
    let d_lifecycle = t_lifecycle.elapsed();
    if !has_code_slots && lifecycle_plan.census.declarations > 0 {
        out.push_str("__rock_code_slot_begin()\n");
    }
    let t_publication = Instant::now();
    out.push_str(&lifecycle_plan_stage_lua(ws, &cache.scx, &lifecycle_plan));
    out.push_str(&static_resets);
    out.push_str(code_slot_publish_lua(
        &candidate_code_artifact,
        false,
        lifecycle_plan.census.declarations > 0,
    ));
    let d_publication = t_publication.elapsed();
    let t_metadata = Instant::now();
    let definition_plan = cache.definition_plan.as_deref().cloned().ok_or_else(|| {
        CompilerError::new_no_location(
            "Artifact fingerprint generation requires a complete DefinitionPlan".to_string(),
        )
    })?;
    let global_layout = cache.global_layout.clone().ok_or_else(|| {
        CompilerError::new_no_location(
            "Artifact fingerprint generation requires a global layout".to_string(),
        )
    })?;
    let artifact_reads = artifact_comptime_reads(cache, &[], &unit_reads, false);
    let d_metadata = t_metadata.elapsed();
    let t_fingerprint = Instant::now();
    let mut fingerprint_code_rows = cache.fingerprint_code_rows.clone().ok_or_else(|| {
        CompilerError::new_no_location(
            "overlay requires a complete code fingerprint inventory".to_string(),
        )
    })?;
    let t_fingerprint_rows = Instant::now();
    let fingerprint_rows_derived = fingerprint_code_rows
        .replace(
            ws,
            cache.scx.interner(),
            &candidate_code_artifact,
            &fingerprint_identities,
        )
        .map_err(|error| CompilerError::new_no_location(error.to_string()))?;
    let d_fingerprint_rows = t_fingerprint_rows.elapsed();
    let artifact_fingerprint = crate::artifact_fingerprint::compute_cached(
        ws,
        cache.scx.interner(),
        &candidate_code_artifact,
        &lifecycle_plan,
        &definition_plan,
        &global_layout,
        &artifact_reads,
        &ws.comptime_inputs.volatile_vars,
        renderer_fingerprint_tag(cache.renderer_target),
        &fingerprint_code_rows,
    )
    .map_err(|error| {
        CompilerError::new_no_location(format!("Artifact fingerprint generation failed: {error}"))
    })?;
    if std::env::var_os("SKULL_FORCE_FINGERPRINT_SCAN").is_some() {
        let fresh = self::artifact_fingerprint(
            ws,
            &cache.scx,
            &candidate_code_artifact,
            &lifecycle_plan,
            &definition_plan,
            &global_layout,
            &artifact_reads,
            cache.renderer_target,
        )?;
        if artifact_fingerprint != fresh {
            return Err(CompilerError::new_no_location(
                "cached code fingerprint differs from fresh inventory".to_string(),
            ));
        }
    }
    let d_fingerprint = t_fingerprint.elapsed();
    out.push_str("end\n");
    if cache.cg.line_directives() {
        out = skull_codegen_lua::resolve_line_directive_resets(&out, "=[overlay]");
    }

    Ok((
        MirEmit {
            release_stats: CompileReleaseStats::default(),
            compile_counts: CompileCounts {
                concrete_plans: cache.concrete.plans.len() + cache.concrete.always_plans.len(),
                chunks: cache.chunks.len(),
                dependency_edges: None,
            },
            lua: out,
            allocation_report: crate::allocation_report::AllocationReport::default(),
            code_artifact: candidate_code_artifact,
            lifecycle_plan,
            global_layout,
            definition_plan,
            artifact_fingerprint,
            skipped,
            phases: vec![
                ("  overlay artifact clone", d_clone_artifact),
                ("  overlay definitions", d_definitions),
                ("  overlay code-slot stage", d_code_slot_stage),
                ("  overlay static resets", d_static_resets),
                ("  overlay const inits", d_const_inits),
                ("  overlay lifecycle collection", d_lifecycle),
                ("  overlay publication", d_publication),
                ("  overlay metadata", d_metadata),
                ("  overlay fingerprint", d_fingerprint),
                ("  overlay fingerprint code rows", d_fingerprint_rows),
            ],
            abi_plan_stats: cache.scx.abi_plan_stats(),
            attribution: EmitAttribution::default(),
            arena_traffic: ArenaTraffic::default(),
            setup_lanes: SetupLaneAttribution::default(),
            graph_census: crate::instance_graph::RunStats::default(),
            concrete_stats: crate::concrete_body::ConcreteBuildStats::default(),
            // An overlay redefines changed bodies only; the entry point is
            // already installed by the `main.lua` it loads over.
            emits_main: false,
            shaders: std::sync::Arc::clone(&cache.shader_registrations.sources),
        },
        unit_reads,
        fingerprint_code_rows,
        fingerprint_rows_derived,
    ))
}

/// Incremental OVERLAY emit for a body edit: update the chunk cache for the
/// re-inferred bodies, accumulate them into `changed_since_full`, and assemble
/// `incremental.lua` from the WHOLE set (so an earlier edit isn't reverted to
/// main.lua's stale copy. The caller writes the result to
/// `tmp/incremental.lua` (loaded after `tmp/main.lua`); on an interface/full
/// rebuild the caller uses `emit_full` and clears `changed_since_full`.
pub fn emit_overlay(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> Result<MirEmit, CompilerError> {
    emit_overlay_with_span_edits(ws, cache, reinferred, &[])
}

pub fn emit_overlay_with_span_edits(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
    edits: &[crate::instance_graph::SpanEdit],
) -> Result<MirEmit, CompilerError> {
    cache.concrete.detach_plan_store();
    let t0 = Instant::now();
    // The retained context refreshes scalar values inside update_chunk_cache.
    let t_cg = t0.elapsed();
    let t1 = Instant::now();
    // `scx` is maintained incrementally inside `update_chunk_cache` (per-body diff)
    // -- no whole-workspace rebuild.
    let t_scx = t1.elapsed();
    let t2 = Instant::now();
    // Shader registrations FIRST -- their dependencies are graph/view roots, so
    // `update_chunk_cache`'s re-walk must see this edit's set.
    let t_shader = Instant::now();
    let shader_changed = cache.update_shader_registrations(ws, reinferred)?;
    let d_shader = t_shader.elapsed();
    let t_chunk = Instant::now();
    let (affected, update, added_errors, fingerprint_identities) =
        cache.update_chunk_cache(ws, reinferred, edits)?;
    let d_chunk = t_chunk.elapsed();
    if let Some(err) = SkullEmitCache::combine_errors(added_errors) {
        return Err(err);
    }
    let shader_changed = shader_changed || cache.shader_delta_affects_view(&update);
    if shader_changed {
        cache.refresh_shader_registrations(ws)?;
    }
    cache.changed_since_full.extend(affected);
    // Accumulate changed `const` globals so the overlay re-emits their inits
    // (unguarded). A mutable `global` value edit is interface-tier (full rebuild),
    // so it never reaches here; even so, gate on `is_const` defensively.
    for &(mid, owner) in reinferred {
        if let BodyOwner::Global(gid) = owner {
            if ws.module(mid).globals[gid].is_const {
                cache.changed_consts_since_full.insert((mid, gid));
            }
        }
    }
    let t_replacements = Instant::now();
    let replacements: Vec<&skull_mir::code_identity::CodeArtifact> = cache
        .changed_since_full
        .iter()
        .filter_map(|body| cache.chunks.get(body).and_then(|chunk| chunk.as_ref().ok()))
        .map(|chunk| &chunk.code_artifact)
        .collect();
    let next_code_artifact = cache.code_artifact.with_replacements(&replacements);
    let initialize_code_slots = !cache
        .code_artifact
        .entries
        .values()
        .any(|entry| entry.escaping)
        && next_code_artifact
            .entries
            .values()
            .any(|entry| entry.escaping);
    let d_replacements = t_replacements.elapsed();
    if !cache.code_artifact.compatible_overlay(&next_code_artifact) {
        return Err(CompilerError::new_no_location(
            "Rock CodeRef compatibility requires a safe restart".to_string(),
        ));
    }
    // A body edit cannot change the mutable-global layout; any other reinferred
    // owner is checked, since the overlay runs over the live storage.
    let t_layout = Instant::now();
    let body_only = reinferred
        .iter()
        .all(|(_, owner)| matches!(owner, BodyOwner::Function(_)));
    if !body_only {
        let layout = skull_mir::global_storage::global_layout_id(ws, &cache.scx)
            .map_err(CompilerError::new_no_location)?;
        if cache.global_layout.as_deref() != Some(layout.as_str()) {
            return Err(CompilerError::new_no_location(
                "Rock global layout changed; restart required".to_string(),
            ));
        }
    }
    let d_layout = t_layout.elapsed();
    cache.code_artifact = std::sync::Arc::new(next_code_artifact);
    // A comptime input read by a GLOBAL initializer can go stale with no
    // reinferred body at all (bone hashes only source text). Route a stale
    // const through the same changed-consts machinery -- its ginit re-lowers
    // fresh, re-reading the file / new var value. A stale MUTABLE global is
    // deliberately left alone: its guarded init preserves the live value on
    // reload, exactly as a source edit to its initializer does.
    let t_stale = Instant::now();
    {
        let mut file_hashes: FxHashMap<Ustr, Option<u64>> = FxHashMap::default();
        let stale_consts: Vec<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)> = cache
            .unit_comptime_reads
            .globals
            .iter()
            .filter(|((mid, gid), reads)| {
                ws.module(*mid).globals[*gid].is_const
                    && comptime_reads_stale(ws, reads, &mut file_hashes)
            })
            .map(|(key, _)| *key)
            .collect();
        cache.changed_consts_since_full.extend(stale_consts);
    }
    let d_stale = t_stale.elapsed();
    let t_relower = t2.elapsed();
    let t3 = Instant::now();
    let published_code_artifact = cache.code_artifact.clone();
    let (mut emit, overlay_unit_reads, fingerprint_code_rows, fingerprint_rows_derived) =
        assemble_overlay(
            ws,
            cache,
            shader_changed,
            initialize_code_slots,
            fingerprint_identities,
        )?;
    cache.fingerprint_code_rows = Some(fingerprint_code_rows);
    cache.repair_stats.fingerprint_code_rows_derived = fingerprint_rows_derived;
    cache.repair_stats.code_artifact_assembly_reused =
        std::sync::Arc::ptr_eq(&published_code_artifact, &emit.code_artifact);
    cache.code_artifact = emit.code_artifact.clone();
    cache.lifecycle_plan = Some(std::sync::Arc::new(emit.lifecycle_plan.clone()));
    cache.definition_plan = Some(std::sync::Arc::new(emit.definition_plan.clone()));
    cache.artifact_fingerprint = Some(emit.artifact_fingerprint.clone());
    for (key, reads) in overlay_unit_reads.globals {
        if reads.is_empty() {
            cache.unit_comptime_reads.globals.remove(&key);
        } else {
            cache.unit_comptime_reads.globals.insert(key, reads);
        }
    }
    if concrete_stats_requested() {
        emit.concrete_stats = cache.concrete.snapshot_build_stats();
    }
    // Advance the static baseline to the keys now in effect. A reset line fires on
    // the reload that dofiles the overlay carrying it, so once THIS overlay resets a
    // cell (or leaves it unchanged), a LATER overlay must not reset it again -- else
    // an unrelated follow-up edit would re-run the initializer and wipe accumulated
    // state. Bring each changed fn's cells up to their current key so the next
    // overlay only resets cells whose key changes AGAIN. A cell this overlay
    // dropped leaves the baseline, so a later overlay does not drop it again.
    // A specialization first built by this overlay joins the baseline here.
    cache.advance_static_baseline(ws)?;
    let t_asm = t3.elapsed();
    // A body edit that mints a newly-colliding synthetic name is not safe to
    // deliver as an overlay.
    if let Some(err) = cache.cg.take_rejection() {
        return Err(err);
    }
    emit.phases.extend([
        ("CodegenCtx::new", t_cg),
        ("ScalarCtx::new", t_scx),
        ("update_chunk_cache (re-lower changed)", t_relower),
        ("  shader registrations", d_shader),
        ("  owner-local chunk cache", d_chunk),
        ("  CodeArtifact replacement", d_replacements),
        ("  global layout check", d_layout),
        ("  stale comptime scan", d_stale),
        (
            "  graph update",
            Duration::from_nanos(cache.repair_stats.graph_update_ns),
        ),
        (
            "  concrete update",
            Duration::from_nanos(cache.repair_stats.concrete_update_ns),
        ),
        (
            "  ArtifactView update",
            Duration::from_nanos(cache.repair_stats.artifact_update_ns),
        ),
        (
            "  candidate update",
            Duration::from_nanos(cache.repair_stats.candidate_update_ns),
        ),
        (
            "  affected selection",
            Duration::from_nanos(cache.repair_stats.affected_selection_ns),
        ),
        (
            "  relowering",
            Duration::from_nanos(cache.repair_stats.relowering_ns),
        ),
        ("assemble_overlay (+build_deco_map)", t_asm),
    ]);
    Ok(emit)
}

#[cfg(test)]
mod lua_definition_scope_tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn luajit_loads(lua: &str) -> Option<(bool, String)> {
        let mut child = Command::new("luajit")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        if let Err(error) = child.stdin.take().unwrap().write_all(lua.as_bytes()) {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        let output = child.wait_with_output().unwrap();
        Some((
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }

    #[test]
    fn definition_scopes_cross_the_luajit_main_constant_limit() {
        let mut raw = String::new();
        let mut definitions = LuaDefinitionBuffer::new();
        for i in 0..32_769 {
            let function = format!("function generated_{i}() return {i} end\n");
            raw.push_str(&function);
            definitions.push_str(&function);
        }
        let Some((raw_loaded, raw_error)) = luajit_loads(&raw) else {
            return;
        };
        assert!(!raw_loaded, "the unpartitioned control unexpectedly loaded");
        assert!(
            raw_error.contains("more than 65536 constants"),
            "the control failed for the wrong reason: {raw_error}"
        );

        let mut scoped = String::new();
        definitions.append_to(&mut scoped);
        scoped.push_str("assert(generated_0() == 0 and generated_32768() == 32768)\n");
        let (scoped_loaded, scoped_error) = luajit_loads(&scoped).unwrap();
        assert!(
            scoped_loaded,
            "partitioned definitions did not load: {scoped_error}"
        );
    }

    #[test]
    fn small_definition_population_keeps_its_exact_text() {
        let source = "function one() return 1 end\nfunction two() return one() + 1 end\n";
        let mut definitions = LuaDefinitionBuffer::new();
        definitions.push_str(source);
        let mut out = String::new();
        definitions.append_to(&mut out);
        assert_eq!(out, source);
    }
}

#[cfg(all(test, feature = "renderer-wgpu"))]
mod wgpu_projection_tests {
    use super::*;

    #[test]
    fn unexpected_emitter_failure_aborts_after_admission() {
        let unexpected = CompilerError::new_no_location("unexpected WGSL emitter failure");
        let mut visited = Vec::new();
        let result = collect_wgpu_projections(
            vec![
                ("admitted".to_string(), true),
                ("unexpected".to_string(), false),
            ],
            |key, _| {
                visited.push(key.to_string());
                if key == "admitted" {
                    Ok("artifact")
                } else {
                    Err(unexpected.clone())
                }
            },
        );
        let error = result.expect_err("an unexpected emitter error must fail the collection");
        assert_eq!(error.message, "unexpected WGSL emitter failure");
        assert_eq!(error.kind, skull_parser::CompilerErrorKind::Generic);
        assert_eq!(visited, ["admitted", "unexpected"]);
    }
}

#[cfg(all(test, not(feature = "sequential")))]
mod setup_parallel_tests {
    use super::*;
    use bone_core::prelude::Instant;
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    /// How long a setup branch waits for its peer to start before calling the
    /// branches serialized. Generous so a loaded gate host still passes.
    const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(5);

    /// A two-party rendezvous. Each branch announces that it started, then waits
    /// (bounded) for the other to announce the same, so overlap is proven by both
    /// sides observing each other rather than by the host happening to schedule
    /// the peer before the first branch finished.
    struct StartRendezvous {
        started: Mutex<usize>,
        both_started: Condvar,
    }

    impl StartRendezvous {
        fn new() -> Self {
            Self {
                started: Mutex::new(0),
                both_started: Condvar::new(),
            }
        }

        /// Returns once both branches have arrived; panics with a clear message
        /// if the peer did not start within `RENDEZVOUS_TIMEOUT`, which can only
        /// happen if the branches were serialized.
        fn wait_for_peer(&self, branch: &str) {
            let mut started = self.started.lock().expect("rendezvous state");
            *started += 1;
            self.both_started.notify_all();
            let deadline = Instant::now() + RENDEZVOUS_TIMEOUT;
            while *started < 2 {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (guard, _) = self
                    .both_started
                    .wait_timeout(started, remaining)
                    .expect("rendezvous wait");
                started = guard;
            }
            let peer_started = *started >= 2;
            drop(started);
            assert!(
                peer_started,
                "setup branch {branch} started but its peer never did within {RENDEZVOUS_TIMEOUT:?}: \
                 the setup branches ran one after the other instead of concurrently"
            );
        }
    }

    #[cfg(not(feature = "sequential"))]
    #[test]
    fn independent_setup_branches_can_overlap() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("two-thread test pool");
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let rendezvous = Arc::new(StartRendezvous::new());
        pool.install(|| {
            parallel_join(
                {
                    let active = Arc::clone(&active);
                    let peak = Arc::clone(&peak);
                    let rendezvous = Arc::clone(&rendezvous);
                    move || {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        rendezvous.wait_for_peer("left");
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                },
                {
                    let active = Arc::clone(&active);
                    let peak = Arc::clone(&peak);
                    let rendezvous = Arc::clone(&rendezvous);
                    move || {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        rendezvous.wait_for_peer("right");
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                },
            );
        });
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "both setup branches must be active at once through parallel_join"
        );
    }
}

#[cfg(test)]
mod missing_owner_tests {
    use super::*;
    use bone_semantic::inference::Workspace;
    use skull_mir::mir::FuncRef;
    use skull_mir::monoty::MonoTy;
    use skull_parser::CompilerError;
    use ustr::Ustr;

    const SRC: &str = r#"
@inline
fn wrap<T>(x: T) -> T { return x }

fn top() -> int { return wrap(7) }
fn main() { let result = top() }
global RESULT: int = top()
"#;

    const CENSUS_SRC: &str = r#"
@inline
fn straight(flag: bool) -> bool {
    let unused = 99
    return flag
}

@inline
fn general(flag: bool) -> int {
    if flag { return 1 }
    return 2
}

fn top() -> int {
    let flag = straight(true)
    return general(flag)
}

fn main() { let result = top() }
global RESULT: int = top()
"#;

    const CROSS_BLOCK_SRC: &str = r#"
@inline
fn identity(flag: bool) -> bool { return flag }

fn run(select: bool) -> bool {
    let value = identity(true)
    if select { return value }
    return value
}

fn main() { let result = run(true) }
"#;

    const BOUNDED_SRC: &str = r#"
fn total<I: Iterator<int>>(it: I) -> int {
    let sum = 0
    for value in it { sum = sum + value }
    return sum
}

fn top() -> int { return total([1, 2, 3].iter()) }
fn main() { let result = top() }
"#;

    const COROUTINE_PLAN_SRC: &str = r#"
enum Ev { Tick(string) }
global R: int = 0

co fn worker() -> Coroutine<Ev> {
    let counter = 100
    let add = (n: int) => n + counter
    R = add(5)
    yield Ev::Tick("x")
}

fn main() {
    let c = worker()
    let _ = c.resume()
}
"#;

    const CLOSURE_SRC: &str = r#"
@inline
fn bump(x: int) -> int { return x }

fn top() -> int {
    let counter = 100
    let add = (n: int) => bump(n) + counter
    return add(5)
}

fn main() { let result = top() }
"#;

    const SPECIALIZED_SRC: &str = r#"
@inline
fn bump(x: int) -> int { return x }

fn generic<T>(x: T) -> int { return bump(1) }
fn top() -> int { return generic(7) }
fn main() { let result = top() }
"#;

    // This publisher-only type first reaches the shared interner in the setup
    // join; using it in a Rock body would intern it during concrete planning.
    const SETUP_SRC: &str = r#"
struct SetupIdentity<T> { value: T }

@nomangle inline(lua)
fn publish_setup_identity(value: SetupIdentity<int>) -> SetupIdentity<int> = "return value"

@nomangle inline(lua) fn publish(value: int) -> int = "return value"

@shader(fragment)
fn frag(tc: Vec2) -> Vec4 { return v(tc.x, 0.0, 0.0, 1.0) }

fn main() {
    let shader = rock_shader(frag)
    let published = publish(7)
}
"#;

    static STDLIB: std::sync::LazyLock<Vec<skull_parser::Ast>> = std::sync::LazyLock::new(|| {
        rock_stdlib::stdlib_files()
            .iter()
            .map(|file| {
                let id = Ustr::from(format!("<stdlib:{}>", file.name).as_str());
                skull_parser::parse_module_clean(file.contents, id).expect("stdlib parse")
            })
            .collect()
    });

    static INLINE_OFF: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static INLINE_CENSUS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    #[allow(dead_code)]
    struct InlineOff(skull_transform::InlineOffGuard, skull_mir::ablation::Scope);

    impl InlineOff {
        fn on() -> Self {
            Self(
                skull_transform::inline_off_guard(),
                skull_mir::ablation::scope(true),
            )
        }
    }

    #[allow(dead_code)]
    struct InlineOn(std::sync::MutexGuard<'static, ()>, Option<String>);

    impl InlineOn {
        fn on() -> Self {
            let guard = INLINE_OFF.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("SKULL_INLINE_OFF").ok();
            unsafe { std::env::remove_var("SKULL_INLINE_OFF") };
            skull_mir::ablation::clear();
            skull_mir::ablation::set_enabled(false);
            Self(guard, previous)
        }
    }

    impl Drop for InlineOn {
        fn drop(&mut self) {
            skull_mir::ablation::clear();
            match &self.1 {
                Some(value) => unsafe { std::env::set_var("SKULL_INLINE_OFF", value) },
                None => unsafe { std::env::remove_var("SKULL_INLINE_OFF") },
            }
        }
    }

    #[allow(dead_code)]
    struct InlineCensusOn(std::sync::MutexGuard<'static, ()>, Option<String>);

    impl InlineCensusOn {
        fn on() -> Self {
            let guard = INLINE_CENSUS.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("ROCK_INLINE_CENSUS").ok();
            unsafe { std::env::set_var("ROCK_INLINE_CENSUS", "1") };
            Self(guard, previous)
        }
    }

    impl Drop for InlineCensusOn {
        fn drop(&mut self) {
            match &self.1 {
                Some(value) => unsafe { std::env::set_var("ROCK_INLINE_CENSUS", value) },
                None => unsafe { std::env::remove_var("ROCK_INLINE_CENSUS") },
            }
        }
    }

    fn workspace() -> Workspace {
        let mut modules = vec![
            skull_parser::parse_module_clean(SRC, Ustr::from("<missing-owner-test>"))
                .expect("parse"),
        ];
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn census_workspace() -> Workspace {
        let mut modules = vec![
            skull_parser::parse_module_clean(CENSUS_SRC, Ustr::from("<inline-census-test>"))
                .expect("parse"),
        ];
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn cross_block_workspace() -> Workspace {
        let mut modules = vec![
            skull_parser::parse_module_clean(
                CROSS_BLOCK_SRC,
                Ustr::from("<inline-cross-block-test>"),
            )
            .expect("parse"),
        ];
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    #[test]
    fn global_init_target_rejects_an_unresolved_storage_plan() {
        let mut modules = vec![
            skull_parser::parse_module_clean(
                "global G: int = 0\nfn main() {}",
                Ustr::from("<global-plan-test>"),
            )
            .expect("parse"),
        ];
        let mut ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = ws
            .modules
            .iter()
            .find(|module| module.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let gid = ws
            .module(mid)
            .globals
            .iter()
            .find(|(_, global)| global.name == Ustr::from("G"))
            .map(|(gid, _)| gid)
            .expect("global");
        let ty = ws.module(mid).globals[gid].ty;
        let bone_semantic::prelude::TyId::Static(_, idx) = ty else {
            panic!("fixture global must have a static type id")
        };
        ws.module_mut(mid).globals[gid].ty = bone_semantic::prelude::TyId::Local(idx);
        let scx = skull_mir::lower::ScalarCtx::new(&ws);
        let cg = CodegenCtx::new(&ws);
        let global = &ws.module(mid).globals[gid];
        let err = global_init_target(&ws, &scx, &cg, global)
            .expect_err("a missing global representation plan must reject");
        assert!(
            err.message.contains("global type cannot be planned"),
            "{err}"
        );
    }

    fn bounded_workspace() -> Workspace {
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(BOUNDED_SRC, Ustr::from("<bound-plan-test>"))
                .expect("parse"),
        );
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn coroutine_workspace() -> Workspace {
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(
                COROUTINE_PLAN_SRC,
                Ustr::from("<coroutine-plan-test>"),
            )
            .expect("parse"),
        );
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn closure_workspace() -> Workspace {
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(CLOSURE_SRC, Ustr::from("<closure-test>"))
                .expect("parse"),
        );
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn specialized_workspace() -> Workspace {
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(SPECIALIZED_SRC, Ustr::from("<specialized-test>"))
                .expect("parse"),
        );
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn setup_workspace() -> Workspace {
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(SETUP_SRC, Ustr::from("<setup-lane-test>"))
                .expect("parse"),
        );
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    #[test]
    fn candidate_repair_reads_the_concrete_publisher_index() {
        let (_, cache, _) = emit_with_mutation(BodyMutation::None).expect("intact emission");
        let canonical = cache.concrete.candidate_keys_by_func();
        let population: usize = canonical.values().map(|entries| entries.len()).sum();
        assert!(
            population > 0,
            "candidate-index fixture has no published entries"
        );
        assert_eq!(
            cache.candidate_index_by_func() as *const _,
            canonical as *const _,
            "repair must borrow the concrete publisher's index rather than a copied map"
        );
    }

    #[test]
    fn body_overlay_shares_the_published_code_artifact_handle() {
        let parse = |source: &str| {
            skull_parser::parse_module_clean(source, Ustr::from("<artifact-handle-test>"))
                .expect("parse")
        };
        let mut modules = vec![parse(SRC)];
        let ws1 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v1");
        let mid = *ws1
            .module_ids
            .get(&Ustr::from("<artifact-handle-test>"))
            .unwrap();
        // The overlay path is the global shape's (see
        // `an_overlay_is_refused_under_the_bound_shape_and_produced_under_global`).
        let (_, mut cache) =
            emit_full_with_callee_binding(&ws1, mid, skull_codegen_lua::CalleeBindingMode::Global)
                .expect("full v1");

        let changed = SRC.replace("return x", "let y = x\n return y");
        let mut modules = vec![parse(&changed)];
        let ws2 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v2");
        let wrap = ref_for(&ws2, "wrap");
        let overlay = emit_overlay(
            &ws2,
            &mut cache,
            &[(wrap.module, BodyOwner::Function(wrap.func))],
        )
        .expect("overlay");
        assert!(
            std::sync::Arc::ptr_eq(&overlay.code_artifact, &cache.code_artifact),
            "the overlay result and cache must share one immutable CodeArtifact publication"
        );
        assert!(
            cache.repair_stats().code_artifact_assembly_reused,
            "overlay assembly duplicated the already-published replacement artifact"
        );
    }

    #[test]
    fn overlays_are_produced_under_both_callee_shapes() {
        let parse = |source: &str| {
            skull_parser::parse_module_clean(source, Ustr::from("<overlay-binding-test>"))
                .expect("parse")
        };
        let edit = SRC.replace("return wrap(7)", "return wrap(9)");
        let overlay_for = |binding: skull_codegen_lua::CalleeBindingMode| {
            let mut modules = vec![parse(SRC)];
            let ws1 =
                bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v1");
            let mid = *ws1
                .module_ids
                .get(&Ustr::from("<overlay-binding-test>"))
                .unwrap();
            let (_, mut cache) =
                emit_full_with_callee_binding(&ws1, mid, binding).expect("full v1");

            let mut modules = vec![parse(&edit)];
            let ws2 =
                bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v2");
            let top = ref_for(&ws2, "top");
            emit_overlay(
                &ws2,
                &mut cache,
                &[(top.module, BodyOwner::Function(top.func))],
            )
        };

        for binding in [
            skull_codegen_lua::CalleeBindingMode::Upvalue,
            skull_codegen_lua::CalleeBindingMode::Global,
        ] {
            let produced = overlay_for(binding).expect("overlay under either callee shape");
            assert!(
                produced.lua.contains("top_0 = __rock_mk_top_0")
                    || produced.lua.contains("function top_0(")
            );
        }
    }

    fn ref_for(ws: &Workspace, name: &str) -> FuncRef {
        ws.modules
            .iter()
            .filter(|module| module.id != ModuleId::builtin())
            .find_map(|module| {
                module
                    .functions
                    .iter()
                    .find(|(_, f)| f.name.as_str() == name)
                    .map(|(func, _)| FuncRef {
                        module: module.id,
                        func,
                    })
            })
            .expect("function")
    }

    fn concrete_symbol(ws: &Workspace, cache: &SkullEmitCache, fref: FuncRef) -> String {
        Instance::intern(cache.graph.interner(), fref, vec![MonoTy::Int])
            .symbol(ws, cache.scx.interner())
    }

    fn assert_residual_definition_and_call(
        ws: &Workspace,
        cache: &SkullEmitCache,
        emit: &MirEmit,
        callee: FuncRef,
    ) {
        let symbol = concrete_symbol(ws, cache, callee);
        // Definition headers in whichever callee shape the build emits: the
        // bound shape spells one `__rock_mk_<symbol> = function(`, and names the
        // symbol again as a factory parameter, which is a binding and not a
        // call.
        let factory = skull_codegen_lua::CALLEE_FACTORY_PREFIX;
        assert!(
            emit.lua.contains(&format!("function {symbol}("))
                || emit.lua.contains(&format!("{factory}{symbol} = function(")),
            "the concrete residual definition is missing: {symbol}\n{}",
            emit.lua
        );
        assert!(
            emit.lua.lines().any(|line| {
                !line.starts_with("function ")
                    && !line.starts_with(factory)
                    && line.contains(&format!("{symbol}("))
            }),
            "the residual call does not name the concrete definition: {symbol}\n{}",
            emit.lua
        );
    }

    fn emit_with_mutation(
        mutation: BodyMutation,
    ) -> Result<(Workspace, SkullEmitCache, MirEmit), CompilerError> {
        let ws = workspace();
        let mid = ws
            .modules
            .iter()
            .find(|m| m.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let (emit, cache) = emit_full_in_with_test_mutation(&ws, mid, mutation)?;
        Ok((ws, cache, emit))
    }

    fn assert_rejection_or_residual(
        result: Result<(Workspace, SkullEmitCache, MirEmit), CompilerError>,
        callee: FuncRef,
    ) {
        match result {
            Err(err) => assert!(
                err.message
                    .contains("no concrete owner agreeing with its delivery instance"),
                "owner loss must be an explicit rejection, not an unrelated emission gap: {err:?}"
            ),
            Ok((ws, cache, emit)) => {
                assert_residual_definition_and_call(&ws, &cache, &emit, callee)
            }
        }
    }

    #[test]
    fn missing_generic_callee_owner_rejects_or_emits_concrete_residual() {
        let ws = workspace();
        let caller = ref_for(&ws, "top");
        let callee = ref_for(&ws, "wrap");
        assert_rejection_or_residual(
            emit_with_mutation(BodyMutation::RemoveCalleeOwner { caller, callee }),
            callee,
        );
    }

    #[test]
    fn incomplete_generic_callee_owner_key_rejects_or_emits_concrete_residual() {
        let ws = workspace();
        let caller = ref_for(&ws, "top");
        let callee = ref_for(&ws, "wrap");
        assert_rejection_or_residual(
            emit_with_mutation(BodyMutation::ClearCalleeOwnerKey { caller, callee }),
            callee,
        );
    }

    #[test]
    fn emission_rejects_a_poisoned_concrete_bound_plan() {
        let _inline = InlineOff::on();
        let ws = bounded_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<bound-plan-test>"))
            .expect("test module");
        let body = ref_for(&ws, "total");
        let err = match emit_full_in_with_test_mutation(
            &ws,
            mid,
            BodyMutation::ClearBoundPlan { body },
        ) {
            Ok(_) => panic!("emission ignored the poisoned concrete bound plan"),
            Err(err) => err,
        };
        assert!(
            err.message
                .contains("concrete callee bound plan is empty for its site"),
            "the poisoned plan must fail at the plan consumer, got: {err:?}"
        );
    }

    #[test]
    fn emission_consumes_a_concrete_coroutine_result_plan() {
        let ws = coroutine_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<coroutine-plan-test>"))
            .expect("test module");
        let body = ref_for(&ws, "worker");
        let (intact, intact_cache) =
            emit_full_in_with_test_mutation(&ws, mid, BodyMutation::None).expect("intact emission");
        let symbol = Instance::base(body).symbol(&ws, intact_cache.scx.interner());
        assert!(
            intact.lua.contains(&format!("function {symbol}__advance(")),
            "the intact coroutine fixture must exercise the concrete yield channel"
        );
        let err = match emit_full_in_with_test_mutation(
            &ws,
            mid,
            BodyMutation::ClearCoroutineResultPlan { body },
        ) {
            Ok(_) => panic!("emission ignored the poisoned coroutine result plan"),
            Err(err) => err,
        };
        assert!(
            err.message
                .contains("concrete callee signature disagrees with its body plan"),
            "the poisoned coroutine plan must fail closed at the concrete-plan join, got: {err:?}"
        );
    }

    #[test]
    fn emission_rejects_a_missing_concrete_iterator_witness_instance() {
        let _inline = InlineOff::on();
        let ws = bounded_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<bound-plan-test>"))
            .expect("test module");
        let body = ref_for(&ws, "total");
        let err = match emit_full_in_with_test_mutation(
            &ws,
            mid,
            BodyMutation::ClearIteratorWitnessInstance { body },
        ) {
            Ok(_) => panic!("emission ignored the missing concrete iterator witness instance"),
            Err(err) => err,
        };
        assert!(
            err.message
                .contains("concrete ABI plan is missing for callee `next`"),
            "the missing iterator bound plan must fail at its production consumer, got: {err:?}"
        );
    }

    #[test]
    fn full_emission_deduplicates_abi_plan_derivations() {
        // Its own fixture: `helper` must be CALLED from more than one site, or
        // the callee question is asked once and a zero hit count cannot
        // distinguish "no duplicates" from "memo not consulted".
        const DEDUP_SRC: &str = r#"
fn helper(x: int) -> int { return x }
fn first() -> int { return helper(1) }
fn second() -> int { return helper(2) }
fn main() { let result = first() }
"#;
        let mut modules = vec![
            skull_parser::parse_module_clean(DEDUP_SRC, Ustr::from("<abi-dedup-test>"))
                .expect("parse"),
        ];
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = ws
            .modules
            .iter()
            .find(|m| m.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let (emit, cache) =
            emit_full_in_with_test_mutation(&ws, mid, BodyMutation::None).expect("intact emission");
        let stats = cache.scx.abi_plan_stats();
        let (hits, misses) = (stats.hits(), stats.requests() - stats.hits());
        assert!(
            misses > 0,
            "no ABI-plan question consulted the memo (hits={hits})"
        );
        assert!(
            hits > 0,
            "three helper call sites repeat the callee question; zero hits means the \
             production path is not consulting the memo (misses={misses})"
        );
        assert!(
            stats.stores > 0 && stats.unique_keys > 0,
            "derivations must commit to the table: {stats:?}"
        );
        assert!(
            stats.shared_lookups > 0,
            "the production population must reach the shared ABI table: {stats:?}"
        );
        assert_eq!(
            stats.shared_lookups,
            stats.shared_hits + stats.shared_misses,
            "shared lookup accounting lost a route: {stats:?}"
        );
        assert!(
            stats.published_hits > 0,
            "the production consumers must reach the immutable published plan table: {stats:?}"
        );
        assert_eq!(
            stats.published_misses, 0,
            "a published-table miss would fall back to the old shared route: {stats:?}"
        );
        assert_eq!(
            stats.plan_clones, 0,
            "cached plan consumers must share immutable handles rather than clone plans: {stats:?}"
        );
        assert_eq!(
            stats.cloned_param_slots, 0,
            "cached plan consumers must not clone parameter-slot vectors: {stats:?}"
        );
        assert!(
            stats.plan_size_samples() > 0
                && stats.plan_size_p50() <= stats.plan_size_p95()
                && stats.plan_size_p95() <= stats.plan_size_max,
            "plan-size census must have a nonzero, ordered population: {stats:?}"
        );
        assert_eq!(
            emit.abi_plan_stats, stats,
            "the reported stats must be the context's own counters"
        );
    }

    #[test]
    fn concrete_result_order_preserves_emitted_lua_bytes() {
        let _inline = InlineOn::on();
        let ws = workspace();
        let mid = ws
            .modules
            .iter()
            .find(|module| module.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let (canonical, _) = emit_full_in_with_test_mutation(
            &ws,
            mid,
            BodyMutation::ConcreteResultOrder(
                skull_graph::concrete_body::BuildResultOrder::Canonical,
            ),
        )
        .expect("canonical emission");
        let (reverse, _) = emit_full_in_with_test_mutation(
            &ws,
            mid,
            BodyMutation::ConcreteResultOrder(
                skull_graph::concrete_body::BuildResultOrder::Reverse,
            ),
        )
        .expect("reverse emission");
        assert!(!canonical.lua.is_empty(), "fixture emitted no Lua artifact");
        assert_eq!(canonical.lua, reverse.lua);
        assert_eq!(canonical.skipped, reverse.skipped);
    }

    #[test]
    fn abi_plan_publication_witness_catches_a_bypassed_publish() {
        let mut modules = vec![
            skull_parser::parse_module_clean(
                "fn helper(x: int) -> int { return x }\nfn main() { let result = helper(1) }",
                Ustr::from("<abi-publish-bypass-test>"),
            )
            .expect("parse"),
        ];
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = ws
            .modules
            .iter()
            .find(|m| m.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let (_, cache) =
            emit_full_in_with_test_mutation(&ws, mid, BodyMutation::SkipAbiPlanPublish)
                .expect("the bypass must still take the fallback path");
        let stats = cache.scx.abi_plan_stats();
        let witness = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(
                stats.published_hits > 0,
                "the publication witness must fail when the publish step is bypassed: {stats:?}"
            );
        }));
        assert!(
            witness.is_err(),
            "the publication witness did not see its bypass: {stats:?}"
        );
    }

    #[test]
    fn full_emission_reports_pre_assembly_setup_work() {
        let (_, _, emit) = emit_with_mutation(BodyMutation::None).expect("intact emission");
        let setup = emit
            .phases
            .iter()
            .filter(|(name, _)| name.starts_with("setup: "))
            .collect::<Vec<_>>();
        assert_eq!(
            setup.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            [
                "setup: validation+context",
                "setup: root collection",
                "setup: graph",
                "setup: concrete plans",
                "setup: artifact+shader+ABI lanes",
                "setup: ABI validation",
                "setup: inline candidate table",
            ]
        );
        assert!(setup.iter().all(|(_, duration)| *duration > Duration::ZERO));
        // The joined group's children report under their own prefix so no
        // consumer can sum them into the sequential setup rows.
        let join = emit
            .phases
            .iter()
            .filter(|(name, _)| name.starts_with("join: "))
            .collect::<Vec<_>>();
        assert_eq!(
            join.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            [
                "join: cpu artifact branch",
                "join: shader view branch",
                "join: ABI plan warm branch"
            ]
        );
        assert!(join.iter().all(|(_, duration)| *duration > Duration::ZERO));
    }

    #[test]
    fn body_attribution_reports_nonzero_work_and_distribution() {
        let _inline = InlineOn::on();
        let (_, _, emit) = emit_with_mutation(BodyMutation::None).expect("intact emission");
        let attribution = &emit.attribution;
        assert!(
            attribution.bodies_examined > 0,
            "no emitted bodies were examined: {attribution:?}"
        );
        assert!(
            attribution.lower.bodies_with_work > 0 && attribution.lower.instructions_visited > 0,
            "lowering attribution has no production population: {attribution:?}"
        );
        assert!(
            attribution.inline.accepted_splices > 0
                && attribution.inline.copied_instructions > 0
                && attribution.inline.borrowed_block_scans > 0
                && attribution.inline.instructions_scanned > 0
                && !attribution.inline.top_owners.is_empty(),
            "inline attribution has no accepted splice population: {attribution:?}"
        );
        assert!(
            attribution.lower.p50_us > 0
                && attribution.lower.p50_us <= attribution.lower.p95_us
                && attribution.lower.p95_us <= attribution.lower.max_us,
            "lowering duration distribution is empty or unordered: {attribution:?}"
        );
        assert!(
            attribution.lower.concrete_mono_requests > 0
                && attribution.lower.concrete_mono_derivations > 0
                && attribution.lower.concrete_mono_requests
                    == attribution.lower.concrete_mono_cache_hits
                        + attribution.lower.concrete_mono_derivations
                && attribution.lower.concrete_mono_unique_ids
                    == attribution.lower.concrete_mono_derivations
                && attribution.lower.concrete_mono_cache_hits > 0
                && attribution.lower.concrete_mono_cache_insertion_cloned_nodes == 0
                && attribution.lower.concrete_mono_cache_hit_cloned_nodes == 0
                && attribution.lower.concrete_mono_cloned_nodes
                    == attribution.lower.concrete_mono_interner_miss_cloned_nodes
                        + attribution.lower.concrete_mono_cache_insertion_cloned_nodes
                        + attribution.lower.concrete_mono_cache_hit_cloned_nodes
                        + attribution
                            .lower
                            .concrete_mono_explicit_owned_escape_cloned_nodes
                && attribution.lower.concrete_mono_explicit_owned_escapes > 0
                && attribution.lower.concrete_mono_cache_hit_cloned_nodes == 0
                && attribution.lower.concrete_mono_cache_insertion_cloned_nodes == 0
                && attribution.lower.concrete_mono_shared_handle_requests
                    == attribution.lower.concrete_mono_requests
                && attribution
                    .lower
                    .concrete_mono_by_consumer
                    .iter()
                    .map(|stats| stats.requests)
                    .sum::<usize>()
                    == attribution.lower.concrete_mono_requests
                && attribution
                    .lower
                    .concrete_mono_by_consumer
                    .iter()
                    .map(|stats| stats.cloned_nodes)
                    .sum::<usize>()
                    == attribution.lower.concrete_mono_cloned_nodes
                && attribution.lower.mir_instruction_allocations > 0
                && attribution.lower.mir_instruction_capacity
                    >= attribution.lower.instructions_visited,
            "lowerer ownership census has no production population: {attribution:?}"
        );
        let consumers = &attribution.lower.concrete_mono_by_consumer;
        assert!(
            consumers[skull_mir::lower::ConcreteMonoConsumer::CallParameter as usize].requests > 0
                && consumers[skull_mir::lower::ConcreteMonoConsumer::CallParameter as usize]
                    .explicit_owned_escapes
                    == 0
                && consumers[skull_mir::lower::ConcreteMonoConsumer::GlobalPlan as usize]
                    .explicit_owned_escapes
                    > 0
                && consumers[skull_mir::lower::ConcreteMonoConsumer::Dispatch as usize].requests
                    > 0,
            "named lowerer consumer ownership routes are misclassified: {attribution:?}"
        );
        if std::env::var("ROCK_LOWER_CENSUS").is_ok_and(|value| value == "1") {
            assert!(
                attribution.lower.concrete_mono_timed_requests > 0
                    && attribution.lower.concrete_mono_lookup_ns > 0
                    && attribution.lower.concrete_mono_clone_self_ns > 0,
                "opt-in lowerer timing has no measured population: {attribution:?}"
            );
            let reallocations = attribution.lower.mir_instruction_reallocations
                + attribution.lower.mir_cell_reallocations
                + attribution.lower.mir_block_reallocations;
            let reallocation_ns = attribution.lower.mir_instruction_reallocation_ns
                + attribution.lower.mir_cell_reallocation_ns
                + attribution.lower.mir_block_reallocation_ns;
            assert!(
                reallocations > 0 && reallocation_ns > 0,
                "opt-in MIR growth timing has no measured population: {attribution:?}"
            );
        }
    }

    #[test]
    fn inline_census_has_a_nonzero_production_population() {
        let _inline = InlineOn::on();
        let _census = InlineCensusOn::on();
        let ws = census_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<inline-census-test>"))
            .expect("census test module");
        let (emit, _) =
            emit_full_in_with_test_mutation(&ws, mid, BodyMutation::None).expect("intact emission");
        let census = &emit.attribution.inline.census;
        assert!(
            census.candidate_requests > 0
                && census.candidate_requests
                    == census.candidate_cache_hits + census.candidate_derivations,
            "candidate requests must partition into hits and derivations: {census:?}"
        );
        assert_eq!(
            census.candidate_derivations,
            census.candidate_keys.len(),
            "each derivation must publish one measured instance key: {census:?}"
        );
        assert_eq!(
            census.candidate_derivations,
            census.candidate_worker_derivations.values().sum::<usize>(),
            "worker derivations must cover the global derivation population: {census:?}"
        );
        assert!(
            emit.attribution.inline.candidate_unique_keys > 0
                && census.candidate_requested_keys.len()
                    >= emit.attribution.inline.candidate_unique_keys
                && census.candidate_derivations == 0
                && emit.attribution.inline.candidate_setup_keys > 0
                && emit.attribution.inline.candidate_setup_warmed > 0,
            "the published candidate table must serve a nonzero requested population: {census:?}"
        );
        assert_eq!(
            emit.attribution.inline.candidate_setup_warmed,
            emit.attribution.inline.candidate_setup_eligible
                + emit.attribution.inline.candidate_setup_rejected,
            "candidate table warm results must partition into eligible and rejected keys"
        );
        assert!(
            emit.attribution.inline.candidate_setup_keys
                >= emit.attribution.inline.candidate_unique_keys,
            "the candidate table cannot be smaller than its requested-key population"
        );
        assert!(
            census.straight_accepted > 0 && census.general_accepted > 0,
            "both splice shapes must reach production: {census:?}"
        );
        assert!(
            census.substitution_invocations == 0
                && census.substitution_instructions_inspected == 0
                && census.substitution_terminators_inspected == 0
                && census.substitution_replacements == 0,
            "straight-result binding must remove the whole-caller substitution route: {census:?}"
        );
        assert_eq!(
            census.clone.source_instructions,
            census.clone.live_instructions + census.clone.dead_instructions,
            "instruction clone census must conserve source entries: {census:?}"
        );
        assert_eq!(
            census.clone.source_cells,
            census.clone.live_cells + census.clone.dead_cells,
            "cell clone census must conserve source entries: {census:?}"
        );
        assert_eq!(
            census.clone.source_blocks,
            census.clone.live_blocks + census.clone.dead_blocks,
            "block clone census must conserve source entries: {census:?}"
        );
        assert!(
            emit.attribution.inline.cleanup_live_before > 0
                && emit.attribution.inline.cleanup_live_after > 0
                && emit.attribution.inline.cleanup_live_after
                    <= emit.attribution.inline.cleanup_live_before,
            "cleanup census must expose a nonzero live population and cannot grow it: {:?}",
            emit.attribution.inline
        );
        assert!(
            census.clone.cloned_instructions <= census.clone.source_instructions
                && census.clone.cloned_cells <= census.clone.source_cells
                && census.clone.cloned_blocks <= census.clone.source_blocks,
            "cloning may not exceed the measured source population: {census:?}"
        );
        assert_eq!(
            census.clone.inst_remap_entries, census.clone.live_instructions,
            "every live instruction and no dead instruction must enter the remap: {census:?}"
        );
        assert_eq!(
            census.clone.cell_remap_entries, census.clone.live_cells,
            "every live cell and no dead cell must enter the remap: {census:?}"
        );
        assert_eq!(
            census.clone.block_remap_entries, census.clone.cloned_blocks,
            "only cloned live blocks may enter the remap: {census:?}"
        );
        assert!(
            census.clone.dead_instructions > 0,
            "the census fixture must expose a dead template instruction: {census:?}"
        );
        assert_eq!(
            census.clone.dead_cloned_instructions
                + census.clone.dead_cloned_cells
                + census.clone.dead_cloned_blocks,
            0,
            "the live template must not clone dead arena entries: {census:?}"
        );
        assert!(
            census.clone.inst_remap_capacity > 0
                && census.clone.cell_remap_capacity > 0
                && census.clone.block_remap_capacity > 0,
            "exact template populations must reach clone-map reservation: {census:?}"
        );
        assert!(
            census.clone.caller_reserved_instructions > 0
                && census.clone.caller_reserved_cells > 0
                && census.clone.caller_reserved_blocks > 0,
            "exact template populations must reach caller arena reservation: {census:?}"
        );
        macro_rules! side_bounds {
            ($field:ident) => {
                assert!(
                    census.clone.side_tables.$field.examined
                        <= census.clone.side_tables.$field.source_rows
                        && census.clone.side_tables.$field.copied
                            <= census.clone.side_tables.$field.examined,
                    "side-table census is not bounded for {}: {:?}",
                    stringify!($field),
                    census
                );
            };
        }
        side_bounds!(spec_bindings);
        side_bounds!(callee_instances);
        side_bounds!(abi_plans);
        side_bounds!(representation_plans);
        side_bounds!(closure_environment_plans);
        side_bounds!(raw_bound_trees);
        side_bounds!(resolved_bound_trees);
        side_bounds!(function_value_symbols);
        side_bounds!(instruction_spans);
        side_bounds!(call_sites);
        side_bounds!(call_site_owners);
        side_bounds!(iterator_headers);
    }

    #[test]
    fn inline_population_census_proves_each_lowering_route_and_conserves_keys() {
        let _inline = InlineOn::on();
        let _census = InlineCensusOn::on();
        let ws = census_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<inline-census-test>"))
            .expect("census test module");
        let (emit, _) =
            emit_full_in_with_test_mutation(&ws, mid, BodyMutation::None).expect("intact emission");
        let population = &emit.attribution.inline.population;
        assert!(
            population.candidate_published_keys > 0
                && population.candidate_lower_attempted > 0
                && population.candidate_lowered_keys > 0
                && population.candidate_eligible_keys > 0
                && population.candidate_rejected_before_lower > 0
                && population.ordinary_consumer_keys > 0
                && population.always_lowered_units > 0
                && population.candidate_ordinary_consumer_overlap > 0
                && population.ordinary_canonical_reuse_keys
                    == population.candidate_ordinary_consumer_overlap
                && population.ordinary_canonical_miss_keys == 0
                && population.candidate_requested_unique_keys > 0
                && population.candidate_spliced_unique_keys > 0
                && population.candidate_requested_never_spliced > 0,
            "the production route fixture has no required population: {population:?}"
        );
        assert_eq!(
            emit.attribution.inline.candidate_setup_warmed,
            population.candidate_lower_attempted + population.candidate_rejected_before_lower,
            "candidate warm keys must partition into lower attempts and early rejections: {population:?}"
        );
        assert_eq!(
            population.candidate_lower_attempted,
            population.candidate_lowered_keys + population.candidate_lower_failed,
            "lower attempts must partition into successful lowers and lower failures: {population:?}"
        );
        assert_eq!(
            population.candidate_lowered_keys,
            population.candidate_eligible_keys + population.candidate_rejected_after_lower,
            "successful lowers must partition into eligible and post-lowering rejections: {population:?}"
        );
        assert!(
            population.candidate_lower_attempted >= population.candidate_lowered_keys
                && population.candidate_raw_instructions > 0
                && population.candidate_raw_cells > 0
                && population.candidate_raw_blocks > 0
                && population.candidate_cleaned_instructions > 0
                && population.candidate_cleaned_cells > 0
                && population.candidate_cleaned_blocks > 0
                && population.candidate_full_clone_ns > 0
                && population.candidate_compact_ns > 0
                && population.candidate_full_clone_capacity.0 > 0
                && population.candidate_full_clone_capacity.1 > 0
                && population.candidate_full_clone_capacity.2 > 0
                && population.candidate_compact_capacity.0 > 0
                && population.candidate_compact_capacity.1 > 0
                && population.candidate_compact_capacity.2 > 0
                && population.ordinary_lowering_derivations >= population.ordinary_lowered_keys
                && population.specialized_lowering_derivations
                    >= population.specialized_lowered_keys
                && population.keys_lowered_more_than_once == 0
                && population.first_lower_ns > 0
                && population.duplicate_lower_ns == 0
                && population.canonical_clone_derivations > 0
                && emit.attribution.inline.census.splice_ns > 0,
            "lowering overlap census has no measured duplicate population: {population:?}"
        );
        assert_eq!(
            population.candidate_published_keys,
            population.candidate_requested_unique_keys
                + population.candidate_published_never_requested,
            "published keys must partition into requested and never-requested keys: {population:?}"
        );
        assert_eq!(
            population.candidate_requested_unique_keys,
            population.candidate_spliced_unique_keys + population.candidate_requested_never_spliced,
            "requested keys must partition into spliced and unspliced keys: {population:?}"
        );
        assert_eq!(
            population.candidate_requested_not_published, 0,
            "a production inline lookup must use the published candidate table: {population:?}"
        );

        let specialized = specialized_workspace();
        let specialized_mid = *specialized
            .module_ids
            .get(&Ustr::from("<specialized-test>"))
            .expect("specialized test module");
        let (specialized_emit, _) =
            emit_full_in_with_test_mutation(&specialized, specialized_mid, BodyMutation::None)
                .expect("specialized emission");
        let specialized_population = &specialized_emit.attribution.inline.population;
        assert!(
            specialized_population.specialized_consumer_keys > 0
                && specialized_population.specialized_canonical_reuse_keys
                    == specialized_population.candidate_specialized_consumer_overlap
                && specialized_population.specialized_canonical_miss_keys == 0,
            "the specialized production route was not censused: {specialized_population:?}"
        );
    }

    #[test]
    fn canonical_profiles_survive_fresh_incremental_overlay_and_sequential_reuse() {
        let _inline = InlineOn::on();
        let _census = InlineCensusOn::on();
        let parse = |source: &str| {
            skull_parser::parse_module_clean(source, Ustr::from("<inline-census-incremental-test>"))
                .expect("parse")
        };
        let v1 = CENSUS_SRC;
        let v2 = CENSUS_SRC.replace("return flag", "return false");
        let mut modules = vec![parse(v1)];
        let ws1 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v1");
        let mid1 = *ws1
            .module_ids
            .get(&Ustr::from("<inline-census-incremental-test>"))
            .expect("module v1");
        let (full1, mut cache) = emit_full(&ws1, mid1).expect("full v1");
        assert!(
            full1
                .attribution
                .inline
                .population
                .canonical_clone_derivations
                > 0,
            "fresh build did not use canonical consumers: {:?}",
            full1.attribution.inline.population
        );

        let mut modules = vec![parse(&v2)];
        let ws2 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v2");
        let mid2 = *ws2
            .module_ids
            .get(&Ustr::from("<inline-census-incremental-test>"))
            .expect("module v2");
        let straight = ref_for(&ws2, "straight");
        let reinferred = vec![(straight.module, BodyOwner::Function(straight.func))];
        let incremental =
            emit_incremental(&ws2, &mut cache, &reinferred).expect("incremental canonical reuse");
        let fresh = emit_full(&ws2, mid2).expect("fresh v2").0;
        assert_eq!(
            incremental.lua, fresh.lua,
            "incremental output drifted from fresh output"
        );
        assert!(
            incremental.attribution.inline.candidate_setup_reused > 0
                && incremental
                    .attribution
                    .inline
                    .population
                    .ordinary_canonical_miss_keys
                    == 0
                && incremental
                    .attribution
                    .inline
                    .population
                    .specialized_canonical_miss_keys
                    == 0,
            "incremental build did not preserve the canonical contract: {:?}",
            incremental.attribution.inline.population
        );

        let v3 = v2.replace("return 2", "return 3");
        let mut modules = vec![parse(&v3)];
        let ws3 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v3");
        let mid3 = *ws3
            .module_ids
            .get(&Ustr::from("<inline-census-incremental-test>"))
            .expect("module v3");
        let general = ref_for(&ws3, "general");
        let sequential = emit_incremental(
            &ws3,
            &mut cache,
            &[(general.module, BodyOwner::Function(general.func))],
        )
        .expect("sequential canonical reuse");
        let fresh3 = emit_full(&ws3, mid3).expect("fresh v3").0;
        assert_eq!(
            sequential.lua, fresh3.lua,
            "sequential incremental output drifted"
        );
        assert_eq!(
            sequential
                .attribution
                .inline
                .population
                .ordinary_canonical_miss_keys,
            0,
            "sequential incremental path lost the canonical contract"
        );

        let mut modules = vec![parse(v1)];
        let ws_overlay_base = bone_semantic::semantic_passes::semantic_pass(&mut modules)
            .expect("semantic overlay base");
        let mid_overlay = *ws_overlay_base
            .module_ids
            .get(&Ustr::from("<inline-census-incremental-test>"))
            .expect("module overlay base");
        // Compare the global overlay shape alongside bound full reassembly.
        let (_full_overlay, mut overlay_cache) = emit_full_with_callee_binding(
            &ws_overlay_base,
            mid_overlay,
            skull_codegen_lua::CalleeBindingMode::Global,
        )
        .expect("full overlay base");
        let overlay =
            emit_overlay(&ws2, &mut overlay_cache, &reinferred).expect("overlay canonical reuse");
        assert!(
            !overlay.lua.is_empty(),
            "overlay must emit the changed route"
        );
        let table = overlay_cache
            .inline_cache
            .candidate_table()
            .expect("overlay candidate table");
        assert_eq!(
            table.stats().profiles.len(),
            table.stats().keys,
            "overlay publication must profile every published key"
        );
        assert!(
            table.stats().reused > 0,
            "overlay must reuse unaffected canonical profiles"
        );
    }

    #[test]
    fn straight_cross_block_results_use_a_targeted_binding() {
        let _inline = InlineOn::on();
        let _census = InlineCensusOn::on();
        let ws = cross_block_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<inline-cross-block-test>"))
            .expect("cross-block test module");
        let (emit, _) = emit_full_in_with_test_mutation(&ws, mid, BodyMutation::None)
            .expect("cross-block emission");
        assert!(
            !emit.lua.contains("identity("),
            "the straight callee must still inline in the cross-block fixture"
        );
        assert_eq!(
            emit.attribution.inline.census.substitution_invocations, 0,
            "straight result binding must not scan the caller arena: {:?}",
            emit.attribution.inline.census
        );
    }

    #[test]
    fn body_attribution_reports_transform_populations() {
        let _inline = InlineOn::on();
        let bounded = bounded_workspace();
        let bounded_mid = *bounded
            .module_ids
            .get(&Ustr::from("<bound-plan-test>"))
            .expect("bounded test module");
        let (bounded_emit, _) =
            emit_full_in_with_test_mutation(&bounded, bounded_mid, BodyMutation::None)
                .expect("bounded emission");
        assert!(
            bounded_emit.attribution.counted_loop.bodies_with_work > 0,
            "counted-loop recovery did not report a rewritten body: {:?}",
            bounded_emit.attribution
        );
        assert!(
            bounded_emit.attribution.lower.abi_param_shared_views > 0,
            "lowerer did not exercise the shared ABI parameter view: {:?}",
            bounded_emit.attribution
        );
        assert!(
            bounded_emit.attribution.inline.bounded_splices > 0,
            "bounded fixture did not reach the bounded inline route: {:?}",
            bounded_emit.attribution.inline
        );

        let closure = closure_workspace();
        let closure_mid = *closure
            .module_ids
            .get(&Ustr::from("<closure-test>"))
            .expect("closure test module");
        let (closure_emit, _) =
            emit_full_in_with_test_mutation(&closure, closure_mid, BodyMutation::None)
                .expect("closure emission");
        assert!(
            closure_emit.attribution.closure.bodies_with_work > 0,
            "closure conversion did not report a hoisted body: {:?}",
            closure_emit.attribution
        );
        assert!(
            closure_emit.attribution.inline.hoisted_splices > 0,
            "the closure fixture did not reach the hoisted inline route: {:?}",
            closure_emit.attribution
        );

        let specialized = specialized_workspace();
        let specialized_mid = *specialized
            .module_ids
            .get(&Ustr::from("<specialized-test>"))
            .expect("specialized test module");
        let (specialized_emit, _) =
            emit_full_in_with_test_mutation(&specialized, specialized_mid, BodyMutation::None)
                .expect("specialized emission");
        assert!(
            specialized_emit.attribution.inline.specialized_splices > 0,
            "the specialized fixture did not reach specialized inline splicing: {:?}",
            specialized_emit.attribution
        );
    }

    #[test]
    fn body_attribution_includes_explicit_nested_lowering() {
        const NESTED_SRC: &str = r#"
fn outer(x: int) -> int {
    fn inner(y: int) -> int { return y }
    return inner(x)
}
fn main() { let result = outer(1) }
"#;
        let mut modules = vec![
            skull_parser::parse_module_clean(NESTED_SRC, Ustr::from("<nested-lower-test>"))
                .expect("parse"),
        ];
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = ws
            .modules
            .iter()
            .find(|m| m.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let (emit, _) =
            emit_full_in_with_test_mutation(&ws, mid, BodyMutation::None).expect("nested emission");
        assert!(
            emit.attribution.lower.mir_instruction_allocations
                > emit.attribution.lower.instructions_visited,
            "nested lowering was omitted from the arena census: {:?}",
            emit.attribution
        );
    }

    #[cfg(not(feature = "sequential"))]
    #[test]
    fn setup_lanes_use_deterministic_overlap_handshake() {
        let _inline = InlineOn::on();
        let ws = setup_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<setup-lane-test>"))
            .expect("setup test module");
        let (emit, cache) = emit_full_in_with_test_mutation(
            &ws,
            mid,
            BodyMutation::SetupLaneSchedule(SetupLaneSchedule::Handshake),
        )
        .expect("setup lane emission");
        let lanes = emit.setup_lanes;
        assert!(
            lanes.cpu_population > 0 && lanes.shader_population > 0 && lanes.abi_population > 0,
            "each setup lane must have a real population: {lanes:?}"
        );
        assert_eq!(
            lanes.setup_handshake_entered, 2,
            "CPU and shader lanes must both reach the handshake: {lanes:?}"
        );
        assert_eq!(
            lanes.setup_handshake_released, 2,
            "both handshake lanes must be released: {lanes:?}"
        );
        assert!(
            lanes.setup_handshake_released_after_both,
            "handshake must release only after both lanes enter: {lanes:?}"
        );
        assert!(
            lanes.mono_intern_new_during_join > 0,
            "the overlap fixture must allocate new shared identity: {lanes:?}"
        );
        assert!(!cache.lua_view().publisher_lanes.is_empty());
        assert!(!cache.shader_view().roots.is_empty());
    }

    #[cfg(feature = "sequential")]
    #[test]
    fn setup_lanes_keep_the_sequential_control() {
        let _inline = InlineOn::on();
        let ws = setup_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<setup-lane-test>"))
            .expect("setup test module");
        let (emit, _) = emit_full_in_with_test_mutation(&ws, mid, BodyMutation::None)
            .expect("setup lane emission");
        let lanes = emit.setup_lanes;
        assert!(
            lanes.cpu_population > 0 && lanes.shader_population > 0 && lanes.abi_population > 0,
            "each setup lane must have a real population: {lanes:?}"
        );
        assert_eq!(
            lanes.setup_handshake_entered, 0,
            "sequential setup used the parallel handshake"
        );
        assert_eq!(
            lanes.setup_handshake_released, 0,
            "sequential setup released a handshake"
        );
        assert!(!lanes.setup_handshake_released_after_both);
    }

    #[cfg(not(feature = "sequential"))]
    #[test]
    fn setup_lanes_preserve_identity_under_reversed_scheduling() {
        let _inline = InlineOn::on();
        let ws = setup_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<setup-lane-test>"))
            .expect("setup test module");
        let emit = |schedule| {
            emit_full_in_with_test_mutation(&ws, mid, BodyMutation::SetupLaneSchedule(schedule))
                .expect("setup lane emission")
        };
        let (cpu_first, cpu_cache) = emit(SetupLaneSchedule::CpuFirst);
        let (abi_first, abi_cache) = emit(SetupLaneSchedule::AbiFirst);

        for (name, emit, cache, expected_first) in [
            (
                "CPU-first",
                &cpu_first,
                &cpu_cache,
                SetupLaneKind::CpuArtifact,
            ),
            ("ABI-first", &abi_first, &abi_cache, SetupLaneKind::AbiWarm),
        ] {
            assert!(
                emit.setup_lanes.cpu_population > 0
                    && emit.setup_lanes.shader_population > 0
                    && emit.setup_lanes.abi_population > 0,
                "{name} schedule must exercise every setup lane: {:?}",
                emit.setup_lanes
            );
            assert_eq!(
                emit.setup_lanes.first_started_lane,
                Some(expected_first),
                "{name} schedule did not force the requested first lane: {:?}",
                emit.setup_lanes
            );
            assert!(
                emit.setup_lanes.mono_write_acqs_during_join > 0
                    && emit.setup_lanes.mono_intern_attempts_during_join > 0
                    && emit.setup_lanes.mono_intern_new_during_join > 0,
                "{name} fixture must expose the shared interner write population: {:?}",
                emit.setup_lanes
            );
            assert!(
                cache.lua_view().publisher_lanes.len() > 0,
                "{name} fixture must publish a concrete foreign body: {:?}",
                cache.lua_view().census()
            );
            assert!(
                cache.shader_view().roots.len() > 0,
                "{name} fixture must retain a shader root: {:?}",
                cache.shader_view().roots.len()
            );
        }

        assert_eq!(
            cpu_first.lua, abi_first.lua,
            "reversing setup scheduling changed the Lua artifact"
        );
        assert_eq!(
            cpu_cache.lua_view().publisher_lanes,
            abi_cache.lua_view().publisher_lanes,
            "reversing setup scheduling changed publisher ABI identity"
        );
        assert_eq!(
            cpu_cache.shader_registration_contracts(),
            abi_cache.shader_registration_contracts(),
            "reversing setup scheduling changed shader ABI identity"
        );
    }
}
