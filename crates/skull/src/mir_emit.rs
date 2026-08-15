//! New-path whole-workspace emit: the cell-model IR (`skull-mir`) lowered and
//! printed by `skull-codegen-lua`, assembled into a complete runnable Lua chunk.
//!
//! This is the end-to-end execution oracle for the bone-mir-faithful rebuild
//! (`docs/skull-bone-mir-faithful-plan.md` Phase 6): the per-function lowering
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

use bone_semantic::hir::{BodyOwner, Expr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{Idx, ModuleId};
use rustc_hash::{FxHashMap, FxHashSet};
use skull_codegen_lua::{emit_function, emit_named, mangle_fn, CodegenCtx};
use skull_mir::instance::Instance;
use skull_mir::mir::{FuncRef, Inst, MirFunc, Term};
use skull_parser::{CompilerError, CompilerErrorKind};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::{Duration, Instant};
use ustr::Ustr;

/// Map `f` over `items`, in PARALLEL by default (rayon work-stealing pool) or
/// SEQUENTIALLY under `--features sequential` (clean single-thread sampling
/// profiles — mirrors bone's `sequential` shim). Order-preserving in both modes,
/// so the assembled Lua chunk is byte-identical regardless of the feature. Each
/// `emit_*_unit` is independent (read-only `ws`/`scx`/`cg`, owns its own
/// `MirFunc`), so the lower+cleanup+emit work fans out cleanly per definition.
#[cfg(not(feature = "sequential"))]
fn par_map<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync + Send) -> Vec<R> {
    use rayon::prelude::*;
    items.into_par_iter().map(f).collect()
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
    items.into_par_iter().map_init(init, f).collect()
}
#[cfg(feature = "sequential")]
fn par_map_with<T, R, S>(items: Vec<T>, init: impl Fn() -> S, f: impl Fn(&mut S, T) -> R) -> Vec<R> {
    let mut state = init();
    items.into_iter().map(|t| f(&mut state, t)).collect()
}

#[cfg(not(feature = "sequential"))]
fn parallel_join<A: Send, B: Send>(left: impl FnOnce() -> A + Send, right: impl FnOnce() -> B + Send) -> (A, B) {
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
fn direct_refs(
    ws: &Workspace,
    mf: &MirFunc,
) -> (Vec<Ustr>, Vec<Ustr>) {
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
                Inst::Call { func, .. } => {
                    let f = &ws.module(func.module).functions[func.func];
                    calls.push(Ustr::from(&mangle_fn(ws, f)));
                }
                _ => {}
            }
        }
        match &block.term {
            Term::Goto(t) => stack.push(*t),
            Term::Branch { then_bb, else_bb, .. } => {
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

/// Transitive global-read closure over the call graph, on interned u32 ids so the
/// fixpoint's repeated set unions don't hash/clone strings. An init `g = f()` must
/// run after every global `f` touches even when the init expression names none of
/// them (`global sprite_batch_shader = watched_shader(...)` reaches
/// `SHADERS_TO_LOAD` only through `watched_shader`). Mirrors rock-codegen's
/// `global_dependencies.sorted_order`.
struct GlobalReach {
    id_of: FxHashMap<Ustr, u32>,
    names: Vec<Ustr>,
    closure: Vec<FxHashSet<u32>>,
}

impl GlobalReach {
    /// The global names a fn transitively reads (empty if the name is unknown).
    fn deps_of(&self, fn_name: Ustr) -> Vec<Ustr> {
        let Some(&id) = self.id_of.get(&fn_name) else {
            return Vec::new();
        };
        self.closure[id as usize]
            .iter()
            .map(|&g| self.names[g as usize])
            .collect()
    }
}

/// Build the transitive closure from per-fn direct global reads + direct calls.
fn transitive_global_reads(
    direct_globals: &FxHashMap<Ustr, Vec<Ustr>>,
    direct_calls: &FxHashMap<Ustr, Vec<Ustr>>,
) -> GlobalReach {
    let mut id_of: FxHashMap<Ustr, u32> = FxHashMap::default();
    let mut names: Vec<Ustr> = Vec::new();
    let intern = |s: Ustr, id_of: &mut FxHashMap<Ustr, u32>, names: &mut Vec<Ustr>| -> u32 {
        match id_of.get(&s) {
            Some(&i) => i,
            None => {
                let i = names.len() as u32;
                names.push(s);
                id_of.insert(s, i);
                i
            }
        }
    };
    for (f, gs) in direct_globals {
        intern(*f, &mut id_of, &mut names);
        for g in gs {
            intern(*g, &mut id_of, &mut names);
        }
    }
    for (f, cs) in direct_calls {
        intern(*f, &mut id_of, &mut names);
        for c in cs {
            intern(*c, &mut id_of, &mut names);
        }
    }
    let n = names.len();
    let mut closure: Vec<FxHashSet<u32>> = vec![FxHashSet::default(); n];
    let mut callees: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (f, gs) in direct_globals {
        let fi = id_of[f] as usize;
        for g in gs {
            closure[fi].insert(id_of[g]);
        }
    }
    for (f, cs) in direct_calls {
        let fi = id_of[f] as usize;
        callees[fi].extend(cs.iter().map(|c| id_of[c]));
    }
    // Fixpoint: closure[f] |= closure[callee]. `add` separates the read of callee
    // sets from the write to `closure[f]` (borrow), and is u32, not String.
    let mut add: Vec<u32> = Vec::new();
    loop {
        let mut changed = false;
        for fi in 0..n {
            if callees[fi].is_empty() {
                continue;
            }
            add.clear();
            for &c in &callees[fi] {
                for &g in &closure[c as usize] {
                    if !closure[fi].contains(&g) {
                        add.push(g);
                    }
                }
            }
            for &g in &add {
                if closure[fi].insert(g) {
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    GlobalReach { id_of, names, closure }
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

/// Render a literal default-value expression as Lua (for inline(lua) default
/// fills). `None` for a non-literal default (skipped — uncommon for inline fns).
fn render_default_lit(body: &bone_semantic::hir::Body, e: Idx<Expr>) -> Option<String> {
    match body.expr_arena[e] {
        Expr::Null | Expr::Unit => Some("nil".to_string()),
        Expr::Int(n) => Some(n.to_string()),
        Expr::Float(f) => Some(format!("{:?}", f.0)),
        Expr::Bool(b) => Some(b.to_string()),
        Expr::String(s) => Some(skull_codegen_lua::lua_string(s.as_str())),
        Expr::UnaryMinus(inner) => render_default_lit(body, inner).map(|s| format!("-{}", s)),
        _ => None,
    }
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
    /// Everything else crosses as the callee produced it: LuaJIT already hands
    /// back a Lua value for `bool`, `float`/`double` and a string, and a
    /// pointer return is declared `Ptr<T>` and stays a cdata by design.
    AsIs,
}

/// Hard backstop on the alias walk. Termination is decided by the visited set,
/// which compares INTERNED instantiations; this only bounds a chain that keeps
/// producing genuinely new ones.
const MAX_ALIAS_STEPS: usize = 256;

fn extern_ret(
    ws: &Workspace,
    f: &bone_semantic::hir::Function,
) -> Result<ExternRet, CompilerError> {
    use bone_semantic::hir::TypeExpr;
    use skull_mir::monoty::{MonoArena, MonoCtor, MonoSubst, MonoTy, MonoTyId, MonoTyReason};

    let Some(ret) = f.return_ty else { return Ok(ExternRet::None) };
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
            other => unrealizable(&format!("a type the compiler cannot normalize ({})", other.bucket())),
        }
    };

    // ONE normalization oracle, shared with monomorphization
    // (`skull_mir::monoty`). It is total, interning, and resolves a declared
    // generic through the substitution -- so a newtype APPLICATION composes for
    // free: each argument is converted in the environment it was written in and
    // becomes a closed `MonoTyId`, which is exactly what a flat substitution can
    // then carry into the next alias.
    //
    // `Outer<T>(Pair<Pair<T>>)` at `Outer<int>` is the shape that needs it: the
    // argument handed to the inner `Pair` still mentions `Outer`'s `T`, and it is
    // already `int` here because it was converted before being bound.
    let mut arena = MonoArena::new();
    let mut subst = MonoSubst::default();
    let mut cur = arena.convert_decl(ws, ret, &subst).map_err(describe)?;

    // The visited key is the interned instantiation itself. Interning is
    // injective within one arena, so a repeat IS a cycle -- no separate summary
    // to get wrong, and no reliance on the step bound.
    let mut visited: Vec<MonoTyId> = Vec::new();

    for _ in 0..MAX_ALIAS_STEPS {
        let (name, params) = match arena.get(cur) {
            MonoTy::Int => return Ok(ExternRet::Int),
            MonoTy::Uint => return Ok(ExternRet::Uint),
            MonoTy::App { ctor: MonoCtor::Nominal(name), params } => (*name, params.clone()),
            _ => return Ok(ExternRet::AsIs),
        };
        if crate::is_value_struct(ws, name) {
            return Ok(ExternRet::ValueStruct(name.as_str().to_string()));
        }
        let Some(nid) = ws.newtypes_by_name.get(&name).copied() else {
            return Ok(ExternRet::AsIs);
        };
        if visited.contains(&cur) {
            return Err(unrealizable("a newtype whose representation contains itself"));
        }
        visited.push(cur);

        // Fail-closed internal invariants, neither reachable from source: bone
        // refuses an arity mismatch itself ("Generic type `Two` expects 2 type
        // argument(s), found 1", verified against the real compiler), and a
        // newtype the workspace names but cannot produce is the registry
        // disagreeing with the arena. Stated rather than assumed, because
        // zipping a mismatched pair would silently drop or invent a binding.
        let owner = ws
            .modules
            .get(nid.module.index())
            .filter(|m| (nid.def.into_raw().into_u32() as usize) < m.newtypes.len())
            .ok_or_else(|| unrealizable("a newtype whose definition is missing"))?;
        let def = &owner.newtypes[nid.def];
        if def.generics.len() != params.len() {
            return Err(unrealizable(&format!(
                "`{name}` applied to {} argument(s) where its declaration takes {}",
                params.len(),
                def.generics.len()
            )));
        }

        let mut next = MonoSubst::default();
        for (g, a) in def.generics.iter().zip(params) {
            let (TypeExpr::Generic(var, _) | TypeExpr::Infer(var, _)) =
                skull_mir::monoty::TyEnv::Decl.decl_node(ws, *g).map_err(describe)?
            else {
                return Err(unrealizable(&format!(
                    "`{name}` declares a parameter that is not a generic"
                )));
            };
            next.insert(var, a);
        }
        let wrapped = def.wrapped;
        subst = next;
        cur = arena.convert_decl(ws, wrapped, &subst).map_err(describe)?;
    }
    Err(unrealizable("an alias chain that does not bottom out"))
}

/// The assembled new-path program plus the parity gap.
pub struct MirEmit {
    pub lua: String,
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
    pub concrete_mono_handle_clone_ns: u64,
    pub concrete_mono_clone_self_ns: u64,
    pub concrete_mono_timed_requests: usize,
    pub concrete_mono_by_consumer:
        [skull_mir::lower::ConcreteMonoConsumerStats; skull_mir::lower::ConcreteMonoConsumer::COUNT],
    pub abi_param_copy_volume: usize,
    pub abi_param_shared_views: usize,
    pub mir_instruction_allocations: usize,
    pub mir_instruction_reallocations: usize,
    pub mir_instruction_capacity: usize,
    pub mir_cell_allocations: usize,
    pub mir_cell_reallocations: usize,
    pub mir_cell_capacity: usize,
    pub mir_block_allocations: usize,
    pub mir_block_reallocations: usize,
    pub mir_block_capacity: usize,
}

#[derive(Clone, Debug, Default)]
pub struct InlineAttribution {
    pub accepted_splices: usize,
    pub copied_instructions: usize,
    pub arena_allocations: usize,
    pub borrowed_block_scans: usize,
    pub instructions_scanned: usize,
    pub top_owners: Vec<InlineOwnerAttribution>,
}

#[derive(Clone, Debug, Default)]
pub struct InlineOwnerAttribution {
    pub owner: String,
    pub splices: usize,
    pub copied_instructions: usize,
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
struct PhaseTimes {
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
    arena_allocations: usize,
    borrowed_block_scans: usize,
    instructions_scanned: usize,
    owners: BTreeMap<String, (usize, usize)>,
}

impl InlineWork {
    fn add_records(&mut self, records: &[skull_transform::SpliceRecord]) {
        self.accepted_splices += records.len();
        for record in records {
            self.copied_instructions += record.callee_insts;
            let owner = inline_owner_key(record.callee);
            let entry = self.owners.entry(owner).or_default();
            entry.0 += 1;
            entry.1 += record.callee_insts;
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
    }

    fn add(&mut self, other: InlineWork) {
        self.accepted_splices += other.accepted_splices;
        self.copied_instructions += other.copied_instructions;
        self.arena_allocations += other.arena_allocations;
        self.borrowed_block_scans += other.borrowed_block_scans;
        self.instructions_scanned += other.instructions_scanned;
        for (owner, (splices, copied)) in other.owners {
            let entry = self.owners.entry(owner).or_default();
            entry.0 += splices;
            entry.1 += copied;
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
}

fn inline_owner_key(fref: FuncRef) -> String {
    format!("m{}:f{}", fref.module.index(), u32::from(fref.func.into_raw()))
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

fn finish_distribution(phase: &mut AttributionPhase, samples: &mut Vec<u64>) {
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
        self.result.lower.concrete_mono_explicit_owned_escape_cloned_nodes +=
            unit.lower_facts.concrete_mono_explicit_owned_escape_cloned_nodes;
        self.result.lower.concrete_mono_explicit_owned_escapes +=
            unit.lower_facts.concrete_mono_explicit_owned_escapes;
        self.result.lower.concrete_mono_lookup_ns += unit.lower_facts.concrete_mono_lookup_ns;
        self.result.lower.concrete_mono_cache_lookup_ns +=
            unit.lower_facts.concrete_mono_cache_lookup_ns;
        self.result.lower.concrete_mono_handle_clone_ns +=
            unit.lower_facts.concrete_mono_handle_clone_ns;
        self.result.lower.concrete_mono_clone_self_ns += unit.lower_facts.concrete_mono_clone_self_ns;
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
        self.result.lower.mir_instruction_allocations += unit.lower_facts.mir_instruction_allocations;
        self.result.lower.mir_instruction_reallocations += unit.lower_facts.mir_instruction_reallocations;
        self.result.lower.mir_instruction_capacity += unit.lower_facts.mir_instruction_capacity;
        self.result.lower.mir_cell_allocations += unit.lower_facts.mir_cell_allocations;
        self.result.lower.mir_cell_reallocations += unit.lower_facts.mir_cell_reallocations;
        self.result.lower.mir_cell_capacity += unit.lower_facts.mir_cell_capacity;
        self.result.lower.mir_block_allocations += unit.lower_facts.mir_block_allocations;
        self.result.lower.mir_block_reallocations += unit.lower_facts.mir_block_reallocations;
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
        self.result.inline.arena_allocations += unit.inline.arena_allocations;
        self.result.inline.borrowed_block_scans += unit.inline.borrowed_block_scans;
        self.result.inline.instructions_scanned += unit.inline.instructions_scanned;
        for (owner, (splices, copied)) in &unit.inline.owners {
            let entry = self
                .result
                .inline
                .top_owners
                .iter_mut()
                .find(|item| item.owner == *owner);
            if let Some(entry) = entry {
                entry.splices += *splices;
                entry.copied_instructions += *copied;
            } else {
                self.result.inline.top_owners.push(InlineOwnerAttribution {
                    owner: owner.clone(),
                    splices: *splices,
                    copied_instructions: *copied,
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
        finish_distribution(&mut self.result.counted_loop, &mut self.counted_loop_samples);
        finish_distribution(&mut self.result.hoisted_inline, &mut self.hoisted_inline_samples);
        finish_distribution(&mut self.result.validation, &mut self.validation_samples);
        finish_distribution(&mut self.result.regalloc, &mut self.regalloc_samples);
        finish_distribution(&mut self.result.stringify, &mut self.stringify_samples);
        self.result.inline.top_owners.sort_by(|a, b| {
            b.copied_instructions
                .cmp(&a.copied_instructions)
                .then_with(|| a.owner.cmp(&b.owner))
        });
        self.result.inline.top_owners.truncate(8);
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
    /// Module-global names read/written (fn-as-value edges; see `Chunk`).
    global_names: Vec<Ustr>,
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
    out.lua = lua;
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
    let deco_name = crate::decorator_fn_name(d.name.as_str());
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
        .and_then(|plan| skull_mir::lower::lower_decorator_with_plans(
            ws,
            scx,
            plan,
            &concrete.plans,
            ustr::Ustr::from(deco_name.as_str()),
        ));
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
            skull_transform::cleanup_with_nobox(ws, &mut mf);
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
            let ctx = skull_mir::bound::concrete_body_ctx(
                ws,
                mid,
                owner,
                &plan.bound_tree,
                scx.interner(),
            );
            let tv = Instant::now();
            let check = check_unit_spec_bindings(ws, &ctx, mid, &deco_name, &mf, d.span, &mut out.specs);
            out.times.validation += tv.elapsed();
            out.times.validation_work = true;
            if let Err(e) = check {
                out.hard_error = Some(e);
                return out;
            }
            skull_codegen_lua::regalloc_time_reset();
            let te = Instant::now();
            let mut lua = emit_named(ws, cg, scx.interner(), &deco_name, &mf);
            let root_emit = te.elapsed();
            out.times.emit += root_emit;
            out.times.emit_work |= root_emit > Duration::ZERO;
            let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(root_emit);
            out.times.regalloc += regalloc;
            out.times.regalloc_work |= regalloc > Duration::ZERO;
            emit_pass_nested_fns(ws, cg, scx, &mut mf.nested_fns, &mut lua, &mut out.times);
            emit_lowered(ws, &mut out, &mf, lua, Some(Ustr::from(&deco_name)), Duration::ZERO);
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
) -> String {
    match skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty)
        .ok()
        .and_then(|plan| plan.leaf_names(g.name))
    {
        Some(leaves) => leaves
            .iter()
            .map(|l| cg.mint_synthetic(l.to_string()))
            .collect::<Vec<_>>()
            .join(", "),
        None => g.name.to_string(),
    }
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
    let init_name = crate::global_init_fn_name(g.name.as_str());
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
            .is_none() => Ok(None),
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
            skull_transform::cleanup_with_nobox(ws, &mut mf);
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
            let ctx = skull_mir::bound::concrete_body_ctx(
                ws,
                mid,
                owner,
                &plan.bound_tree,
                scx.interner(),
            );
            let tv = Instant::now();
            let check = check_unit_spec_bindings(ws, &ctx, mid, g.name.as_str(), &mf, g.name_span, &mut out.specs);
            out.times.validation += tv.elapsed();
            out.times.validation_work = true;
            if let Err(e) = check {
                out.hard_error = Some(e);
                return out;
            }
            let key = Ustr::from(&init_name);
            skull_codegen_lua::regalloc_time_reset();
            let te = Instant::now();
            let mut lua = emit_named(ws, cg, scx.interner(), &init_name, &mf);
            let root_emit = te.elapsed();
            out.times.emit += root_emit;
            out.times.emit_work |= root_emit > Duration::ZERO;
            let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(root_emit);
            out.times.regalloc += regalloc;
            out.times.regalloc_work |= regalloc > Duration::ZERO;
            emit_pass_nested_fns(ws, cg, scx, &mut mf.nested_fns, &mut lua, &mut out.times);
            emit_lowered(ws, &mut out, &mf, lua, Some(key), Duration::ZERO);
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
            let target = global_init_target(ws, scx, cg, g);
            let line = if g.is_const {
                format!("{target} = {init_name}()\n")
            } else {
                let ty_str = ws.render(None, g.ty);
                format!(
                    "if not _G.HOT_RELOAD or not _G.__rock_old_globals or _G.__rock_old_globals[{:?}] ~= {:?} then {target} = {init_name}() end\n",
                    g.name.as_str(), ty_str
                )
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
/// two string-keyed dispatch sites, `@on_reload` (via `on_hot_reload_by_name`) and
/// `on_loop_jump`, whose targets appear only as string literals. Measured cost on
/// match3: 24 fns / 7.2 KB, 0.3% of the chunk. Sound beats clever at that price.
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
/// dependency (`stone` via stick.toml). `None` means a single-file compile, whose
/// only library is the stdlib.
fn is_library_module(file: &str, project_root: Option<&str>) -> bool {
    if is_stdlib_module_file(file) {
        return true;
    }
    match project_root {
        Some(root) => !file.starts_with(root),
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
    let has = |n: &str| f.annotations.iter().any(|a| a.name.name.as_str() == n);
    if has("nomangle") || has("sim") || has("test") {
        return true;
    }
    // `extern` decls bind a bare name an inline(lua) body may call directly.
    if f.is_extern {
        return true;
    }
    f.annotations.iter().any(|a| deco_map.contains_key(&a.name.name))
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
    Some((name, CompilerError::new_with_label(
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
    )))
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
    let mut inline_cache = skull_transform::InlineCache::default();
    let mut pending: Vec<Instance> = initial.iter().map(|d| d.inst.clone()).collect();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            let self_ref = FuncRef { module: module.id, func: fid };
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
    while let Some(inst) = pending.pop() {
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
        let name = inst.symbol(ws, scx.interner());
        let mut specs = Vec::new();
        match emit_spec_chunk(ws, cg, scx, &cache.concrete, &inst, &name, &mut inline_cache, &mut specs) {
            Ok(chunk) => {
                pending.extend(chunk.specs.iter().map(|d| d.inst.clone()));
                built.insert(inst, chunk);
            }
            Err(e) => errs.push((name, e)),
        }
    }
    (built, errs)
}

/// Emit the whole workspace via the new (cell-model) path. `user_mid` is the
/// entry module (kept for signature parity with the old `emit_workspace`; the
/// new path is uniformly lenient across all modules today). Registered shaders
/// always emit Core330: the profile is a property of the GL context, not the
/// build, and the engine rewrites the version line + precision preamble for
/// GLES at shader-compile time (stone-gl `adapt_source_for_gles`) -- which is
/// the only thing the two profiles differ by.
pub fn emit_workspace_mir(ws: &Workspace, user_mid: ModuleId) -> Result<MirEmit, CompilerError> {
    emit_workspace_mir_in(ws, user_mid, None)
}

/// [`emit_workspace_mir`] for a PROJECT build: `project_root` is the target's
/// directory, so modules outside it (a stick.toml dependency such as `stone`) are
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
    emit_full_in_for_renderer(ws, user_mid, project_root, renderer_target)
        .map(|(emit, _cache)| emit)
}

/// One emitted ordinary/coroutine function definition plus the facts incremental
/// emit needs. The cheap definitions (prelude, decorators, externs, inline(lua),
/// global inits, init ordering, deco applications) are NOT cached — they are
/// regenerated on every [`assemble`]. See `docs/skull-incremental-emit-plan.md`.
#[derive(Clone)]
struct Chunk {
    lua: String,
    /// Callee bodies whose SOURCE or inline-eligibility affects this chunk:
    /// everything inlined in (transitive) plus every callee still called (its
    /// body could flip eligibility). Re-emit this chunk if any of them re-infer.
    deps: FxHashSet<FuncRef>,
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
    /// The callees this body still CALLS after inlining -- the reachability edge
    /// set. Distinct from `deps`, which is an invalidation superset (it also holds
    /// everything inlined AWAY, and would keep spliced-out callees alive).
    calls: Vec<FuncRef>,
    /// Callees this body invokes through a SPECIALIZED instantiation that no
    /// splice served, so each needs a standalone specialized copy emitted.
    /// Deduplicated program-wide by [`assemble`] -- many bodies ask for the same
    /// one, and it must be defined exactly once.
    specs: Vec<skull_mir::bound::SpecDemand>,
}

#[derive(Clone, Copy)]
enum BodyMutation {
    None,
    #[cfg(test)]
    RemoveCalleeOwner { caller: FuncRef, callee: FuncRef },
    #[cfg(test)]
    ClearCalleeOwnerKey { caller: FuncRef, callee: FuncRef },
    #[cfg(test)]
    ClearBoundPlan { body: FuncRef },
    #[cfg(test)]
    ClearCoroutineResultPlan { body: FuncRef },
    #[cfg(test)]
    ClearIteratorWitnessInstance { body: FuncRef },
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
        (handshake.entered, handshake.released, handshake.released_after_both)
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
fn should_emit_via_view(ws: &Workspace, view: &crate::artifact::ArtifactView, self_ref: FuncRef) -> bool {
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

/// Per-body emit cache for incremental codegen: the expensive function chunks
/// (ordinary + coroutine bodies), keyed by [`Instance`]. `Err` records a body the
/// new path could not lower (the parity gap). Everything else is regenerated by
/// [`assemble`] every time. See `docs/skull-incremental-emit-plan.md`.
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
    /// empty by `emit_full`. See `docs/skull-incremental-emit-split.md`.
    changed_since_full: FxHashSet<Instance>,
    /// Workspace overload set (drives mangling). Derived from function SIGNATURES,
    /// so it is byte-identical across every body edit and only changes on an
    /// interface/structural edit -- which routes to `emit_full` (a fresh cache).
    /// Built once here, reused by the body-edit paths (`emit_overlay` /
    /// `emit_incremental`) instead of an O(program) rescan per edit.
    /// `docs/skull-incremental-emit-cache-plan.md` step 1.
    cg: CodegenCtx,
    /// Scalar-ABI context (the set of fns taken by reference). Maintained
    /// incrementally per body edit (`ScalarCtx::update_body`): the edited body's
    /// value-reference contribution is diffed into the cached set, and any name
    /// whose taken-by-ref membership FLIPS drives re-emit of that fn + its callers
    /// (their scalar ABI / call-site shape changed). Rebuilt by `emit_full`.
    /// `docs/skull-incremental-emit-cache-plan.md` steps 2+4.
    scx: skull_mir::lower::ScalarCtx,
    /// Callee-eligibility/lowered-body cache, persisted across body edits: an
    /// unchanged callee keeps its cached lowered body instead of re-lowering on
    /// every edit (the dominant re-lower cost). `update_chunk_cache` invalidates
    /// the affected entries each edit (stale bodies are a subset of affected).
    /// Starts empty after `emit_full` (whose per-worker caches are discarded) and
    /// warms lazily as edits touch callees.
    /// `docs/skull-incremental-emit-cache-plan.md` step 3.
    inline_cache: skull_transform::InlineCache,
    /// Reverse dependency index: `dependents[C]` = the chunks that have `C` in
    /// their codegen deps (a body inlined `C` in, or calls `C`). Lets
    /// `update_chunk_cache` compute the affected set with one reverse hop per
    /// changed fn (O(edit x fan-out)) instead of scanning every chunk's deps.
    /// Built in `emit_full`, maintained as chunks re-emit. Sound on the body-edit
    /// path because the chunk key set is invariant between full builds.
    /// `docs/skull-incremental-emit-cache-plan.md` step 4.
    dependents: FxHashMap<FuncRef, FxHashSet<Instance>>,
    /// `const` globals changed since the last full build (`emit_full`), accumulated
    /// by `emit_overlay`. The overlay re-emits each one's init UNGUARDED so the new
    /// literal takes effect on reload (mutable `global`s are NEVER here -- their
    /// guard preserves the live value, and their value edits stay interface-tier).
    /// Reset to empty by `emit_full`. `docs/skull-const-hot-reload-plan.md`.
    changed_consts_since_full: FxHashSet<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)>,
    /// Baseline `static` cell keys (`type@init_hash`) from the last full build,
    /// keyed by cell name. `assemble_overlay` compares each changed fn's current
    /// cell key against this and emits an UNGUARDED reset (nil cell + clear
    /// done-flag) for any cell whose key changed, so an initializer edit re-inits
    /// on the next reach; an unchanged key is left alone (an unrelated body edit
    /// preserves the live value). Rebuilt by `emit_full`.
    /// `docs/static-hot-reload-overlay-bug.md`.
    static_key_baseline: FxHashMap<Ustr, String>,
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
    /// The compilation target's root directory; see [`is_library_module`].
    project_root: Option<String>,
    renderer_target: RendererTarget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererTarget {
    Gl,
    Wgpu,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WgpuProjectionCensus {
    pub examined: usize,
    pub admitted: usize,
    pub omitted: usize,
    pub omissions: BTreeMap<String, String>,
}

enum WgpuProjection<T> {
    Admitted(T),
    Unsupported { capability: String },
}

fn classify_wgpu_projection<T>(result: Result<T, CompilerError>) -> Result<WgpuProjection<T>, CompilerError> {
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

#[derive(Default)]
struct ShaderRegistrations {
    lua: String,
    dependencies: FxHashSet<FuncRef>,
    callsites: FxHashSet<(ModuleId, BodyOwner)>,
    roots: Vec<FuncRef>,
    keys: BTreeSet<String>,
    abi_wires: BTreeMap<String, String>,
    wgpu_projection_census: Option<WgpuProjectionCensus>,
}

/// A `static` cell's reload key: rendered type + initializer content hash. Equal
/// keys mean "same meaning, preserve the live cell"; a changed key triggers a
/// reset + re-init. Shared by `assemble` (the `__rock_known_globals` preamble),
/// the `emit_full` baseline, and `assemble_overlay` so all three compare identically.
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
    skull_transform::cleanup_with_nobox(ws, mf);
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
            format!("cannot build concrete coroutine plan for `{}`: {}", f.name, reason.bucket()),
        )
    })?;
    let lua = crate::coroutine_emit::emit_coroutine(ws, cg, scx.interner(), &plan, mf);
    times.emit = te.elapsed();
    times.emit_work = times.emit > Duration::ZERO;
    times.regalloc = skull_codegen_lua::regalloc_time_elapsed().min(times.emit);
    times.regalloc_work = times.regalloc > Duration::ZERO;
    // No declaration edge here: "this instance instantiates that declaration" is
    // the GRAPH's fact, and `update_chunk_cache` reads it through
    // `instances_of`. `deps` carries only what codegen alone knows -- which
    // bodies this chunk's TEXT contains.
    let deps = FxHashSet::default();
    Ok(Chunk {
        lua,
        deps,
        direct_globals: Vec::new(),
        direct_calls: Vec::new(),
        times,
        attribution: UnitAttribution::default(),
        statics: mf.statics.clone(),
        calls: skull_mir::reach::call_targets(mf),
        specs,
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
    emit_body_chunk_with_mutation(ws, cg, scx, concrete, self_ref, f, inline_cache, BodyMutation::None)
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
    let plan = concrete
        .plan_for(&shared(self_ref))
        .ok_or_else(|| CompilerError::new_with_label(f.name_span, format!("no concrete plan for `{}`", f.name)))?;
    let mut mf = skull_mir::lower::lower_function_with_plans(ws, scx, plan, &concrete.plans)?;
    times.lower = tl.elapsed();
    times.lower_work = mf.insts.len() > 0;
    attribution.examined = true;
    attribution.hir_nodes = hir_nodes_for_owner(ws, self_ref.module, BodyOwner::Function(self_ref.func));
    attribution.mir_instructions = mf.insts.len();
    attribution.lower_facts = mf.lower_attribution.clone();
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
                BodyMutation::ClearIteratorWitnessInstance { .. } | BodyMutation::SkipAbiPlanPublish => {
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
    // per-call frame (`docs/skull-inlining-plan.md`).
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
    times.inline = ti.elapsed();
    attribution.inline.add_records(&splices);
    attribution.inline.add_scan(scan_stats);
    times.inline_work = !splices.is_empty();
    attribution.inline.add_allocations(before_inline, (mf.insts.len(), mf.blocks.len(), mf.cells.len()));
    let mut deps: FxHashSet<FuncRef> = spliced.into_iter().collect();
    let cleanup_before = mir_live_instructions(&mf);
    let tc = Instant::now();
    skull_transform::cleanup_with_nobox(ws, &mut mf); // drop dead values + dead-cell stores
    times.cleanup = tc.elapsed();
    times.cleanup_work = mir_live_instructions(&mf) != cleanup_before;
    // Shared closure conversion (S2 cutover, docs/closure-conversion-mir-plan.md):
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
    collect_callee_frefs(&mf, &mut deps);
    let (direct_globals, direct_calls) = direct_refs(ws, &mf);
    times.validation = tv.elapsed();
    times.validation_work = true;
    skull_codegen_lua::regalloc_time_reset();
    let te = Instant::now();
    let mut lua = emit_function(ws, cg, scx.interner(), self_ref, &mf);
    times.emit = te.elapsed();
    times.emit_work = times.emit > Duration::ZERO;
    times.regalloc = skull_codegen_lua::regalloc_time_elapsed().min(times.emit);
    times.regalloc_work = times.regalloc > Duration::ZERO;
    // Nested `fn`s are hoisted to sibling top-level Lua fns in the same chunk
    // (their `__local_*` names are already final -- emit_named, no mangling). Their
    // `static` cells fold into THIS chunk's `statics` so the hot-reload preamble +
    // overlay reset see them -- a nested-fn static is otherwise invisible to both.
    let mut statics = mf.statics.clone();
    emit_nested_fns(ws, cg, scx, &mut mf.nested_fns, &mut deps, &mut statics, &mut lua, &mut times);
    attribution.mir_instructions = mf.insts.len();
    attribution.times = times;
    let calls = skull_mir::reach::call_targets(&mf);
    Ok(Chunk { lua, deps, direct_globals, direct_calls, times, attribution, statics, calls, specs })
}

/// Emit each hoisted nested `fn` (and its own nested fns, depth-first) as a
/// top-level Lua function appended to the enclosing chunk. Runs the standard
/// cleanup pipeline per fn, folds their callee refs into the chunk's deps (so an
/// edit to a callee still invalidates the enclosing chunk) and their `static`
/// cells into the chunk's `statics` (so hot-reload can reset them).
fn emit_nested_fns(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    nested: &mut [MirFunc],
    deps: &mut FxHashSet<FuncRef>,
    statics: &mut Vec<skull_mir::mir::StaticCell>,
    out: &mut String,
    times: &mut PhaseTimes,
) {
    for nf in nested.iter_mut() {
        let cleanup_before = mir_live_instructions(nf);
        let tc = Instant::now();
        skull_transform::cleanup_with_nobox(ws, nf);
        times.cleanup += tc.elapsed();
        times.cleanup_work |= mir_live_instructions(nf) != cleanup_before;
        let counted_before = nf.blocks.len();
        let tl = Instant::now();
        skull_transform::recover_counted_loops(ws, nf);
        times.counted_loop += tl.elapsed();
        times.counted_loop_work |= nf.blocks.len() > counted_before;
        collect_callee_frefs(nf, deps);
        statics.extend(nf.statics.iter().cloned());
        skull_codegen_lua::regalloc_time_reset();
        let te = Instant::now();
        out.push_str(&emit_named(ws, cg, scx.interner(), nf.name.as_str(), nf));
        let emit = te.elapsed();
        times.emit += emit;
        times.emit_work |= emit > Duration::ZERO;
        let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(emit);
        times.regalloc += regalloc;
        times.regalloc_work |= regalloc > Duration::ZERO;
        let mut children = std::mem::take(&mut nf.nested_fns);
        emit_nested_fns(ws, cg, scx, &mut children, deps, statics, out, times);
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
    out: &mut String,
    times: &mut PhaseTimes,
) {
    for nf in nested.iter_mut() {
        let cleanup_before = mir_live_instructions(nf);
        let tc = Instant::now();
        skull_transform::cleanup_with_nobox(ws, nf);
        times.cleanup += tc.elapsed();
        times.cleanup_work |= mir_live_instructions(nf) != cleanup_before;
        skull_codegen_lua::regalloc_time_reset();
        let te = Instant::now();
        out.push_str(&emit_named(ws, cg, scx.interner(), nf.name.as_str(), nf));
        let emit = te.elapsed();
        times.emit += emit;
        times.emit_work |= emit > Duration::ZERO;
        let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(emit);
        times.regalloc += regalloc;
        times.regalloc_work |= regalloc > Duration::ZERO;
    }
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

/// Whether a module file id names a stdlib module (`<bone-stdlib:...>` from the
/// CLI, `<stdlib:...>` from test harnesses). Stdlib emit gaps are the known
/// parity list and stay lenient skips; a USER fn that cannot emit is a hard
/// error — the lenient channel shipped callers of bounded fns as missing
/// globals that died as runtime nil calls.
///
/// NOTE: this does NOT cover every way the stdlib gets loaded — `bone-query`
/// warms it from its real filesystem path, which matches neither prefix. Do not
/// reuse this as a general "is this the stdlib" oracle.
fn is_stdlib_module_file(file: &str) -> bool {
    file.starts_with("<bone-stdlib:") || file.starts_with("<stdlib")
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
/// `statics` is NOT one of them. A specialized body's `static` cells are cells
/// the emitted program uses, so leaving them out left them absent from
/// `__rock_known_globals` -- no reset/preserve guard and no boot checkpoint
/// could see them. That was survivable only while a specialization was rare;
/// with emission owned by the instance, every generic body with a `static` is
/// one.
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
    let plan = concrete
        .plan_for(body)
        .ok_or_else(|| CompilerError::new_with_label(f.name_span, format!("no concrete plan for `{name}`")))?;
    let tree = &plan.bound_tree;
    if fn_has_bounds(f) && tree.is_empty() {
        return Err(CompilerError::new_with_label(
            f.name_span,
            format!("concrete plan has no resolved bound tree for specialized `{name}`"),
        ));
    }
    let tl = Instant::now();
    let mut mf = skull_mir::lower::lower_instance_with_plans(ws, scx, plan, &concrete.plans)
        .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?;
    let mut times = PhaseTimes::default();
    times.lower = tl.elapsed();
    times.lower_work = mf.insts.len() > 0;
    let mut attribution = UnitAttribution {
        owner: name.to_string(),
        examined: true,
        hir_nodes: hir_nodes_for_owner(ws, fref.module, BodyOwner::Function(fref.func)),
        mir_instructions: mf.insts.len(),
        lower_facts: mf.lower_attribution.clone(),
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
    let ctx = skull_mir::bound::concrete_body_ctx(
        ws,
        fref.module,
        owner,
        tree,
        scx.interner(),
    );
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
    skull_codegen_lua::regalloc_time_reset();
    let te = Instant::now();
    let mut lua = emit_named(ws, cg, scx.interner(), name, &mf);
    let root_emit = te.elapsed();
    times.emit += root_emit;
    times.emit_work |= root_emit > Duration::ZERO;
    let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(root_emit);
    times.regalloc += regalloc;
    times.regalloc_work |= regalloc > Duration::ZERO;
    let mut statics = mf.statics.clone();
    let mut nested = std::mem::take(&mut mf.nested_fns);
    collect_nested_statics(&nested, &mut statics);
    emit_pass_nested_fns(ws, cg, scx, &mut nested, &mut lua, &mut times);
    collect_callee_frefs(&mf, &mut deps);
    attribution.mir_instructions = mf.insts.len();
    attribution.times = times;
    Ok(Chunk {
        lua,
        deps,
        direct_globals: Vec::new(),
        direct_calls: Vec::new(),
        times,
        attribution,
        statics,
        calls: skull_mir::reach::call_targets(&mf),
        specs,
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
    statics: &mut Vec<skull_mir::mir::StaticCell>,
    out: &mut String,
) -> Vec<CompilerError> {
    let mut done: FxHashSet<Instance> = FxHashSet::default();
    let mut errs: Vec<(String, CompilerError)> = rejected.to_vec();
    let rejected_names: FxHashSet<&str> =
        rejected.iter().map(|(n, _)| n.as_str()).collect();
    while let Some(demand) = pending.pop() {
        let body = demand.inst;
        let fref = body.func;
        if !done.insert(body.clone()) {
            continue;
        }
        let f = &ws.module(fref.module).functions[fref.func];
        let name = body.symbol(ws, scx.interner());
        let Some(chunk) = live.get(&body).or_else(|| cached.get(&body).and_then(|c| c.as_ref().ok()))
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
    let (spliced, records, scan_stats) = skull_transform::inline_functions_with_plans(
        ws, scx, fref, mf, cache, concrete_plans,
    )?;
    times.inline = ti.elapsed();
    times.inline_work = !records.is_empty();
    work.add_records(&records);
    work.add_scan(scan_stats);
    work.add_allocations(before, (mf.insts.len(), mf.blocks.len(), mf.cells.len()));
    let cleanup_before = mir_live_instructions(mf);
    let tc = Instant::now();
    skull_transform::cleanup_with_nobox(ws, mf);
    times.cleanup = tc.elapsed();
    times.cleanup_work = mir_live_instructions(mf) != cleanup_before;
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
        work.add_scan(scan_stats);
        times.hoisted_inline_work |= !records.is_empty();
        work.add_allocations(before, (nf.insts.len(), nf.blocks.len(), nf.cells.len()));
        deps.extend(spliced);
        let cleanup_before = mir_live_instructions(nf);
        let tc = Instant::now();
        skull_transform::cleanup_with_nobox(ws, nf);
        times.cleanup += tc.elapsed();
        times.cleanup_work |= mir_live_instructions(nf) != cleanup_before;
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
/// game code hits, in bitgun's `update_survivors`).
///
/// The emitter already refuses these (`write_index`'s `unreachable!`), but a
/// panic in a parallel emit worker is the wrong shape of loud: no span, no source
/// location, and the process exit code depends on the caller. Rejecting here
/// gives the same guarantee as a spanned `CompilerError`.
fn validate_no_whole_element_column_access(mf: &skull_mir::mir::MirFunc) -> Result<(), String> {
    use skull_mir::mir::{ContainerKind::VecValueColumns, Inst};
    for (_, block) in mf.blocks.iter() {
        for &iid in &block.insts {
            let elem = match &mf.insts[iid] {
                Inst::Index { kind: VecValueColumns(e), .. }
                | Inst::StoreIndex { kind: VecValueColumns(e), .. }
                | Inst::ArrayPush { kind: VecValueColumns(e), .. } => e,
                _ => continue,
            };
            return Err(format!(
                "whole-element access to a column-stored `[{elem}]` survived lowering \
                 (column storage has no element table, so there is no slot to name)"
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
        Self { findings: vec![Finding { message, guarantee: false, site: None }] }
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
        if self.findings.is_empty() { Ok(()) } else { Err(self) }
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
        skull_transform::nobox_materialization_violations(ws, mf).into_iter().map(
            |(message, site)| Finding {
                message: format!("{message}, while emitting `{}`", mf.name),
                guarantee: true,
                site,
            },
        ),
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
/// + dependency-ordered global inits + decorator applications. Everything except
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
pub(crate) fn build_deco_map(ws: &Workspace) -> (DecoMap, Vec<(ModuleId, Idx<bone_semantic::hir::DecoratorDef>)>) {
    let mut deco_map: DecoMap = Default::default();
    let mut deco_items: Vec<(ModuleId, Idx<bone_semantic::hir::DecoratorDef>)> = Vec::new();
    for module in ws.modules.iter() {
        for (did, d) in module.decorators.iter() {
            deco_map.insert(d.name, (module.id, did));
            deco_items.push((module.id, did));
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
    skipped: &mut Vec<(String, String)>,
) -> String {
    let mut deco_lines = String::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (_fid, f) in module.functions.iter() {
            for ann in &f.annotations {
                let Some(&(dmid, did)) = deco_map.get(&ann.name.name) else {
                    continue;
                };
                let d = &ws.module(dmid).decorators[did];
                let mangled = mangle_fn(ws, f);
                let target = format!("{{ name = {:?}, params = {{}}, func = {} }}", f.name.as_str(), mangled);
                match crate::decorator_call_args(d, ann) {
                    Ok(args) => {
                        let mut call_args = vec![target];
                        call_args.extend(args);
                        let _ = writeln!(deco_lines, "{}({})", crate::decorator_fn_name(ann.name.name.as_str()), call_args.join(", "));
                    }
                    Err(e) => skipped.push((f.name.as_str().to_string(), e.message)),
                }
            }
        }
    }
    deco_lines
}

/// Every `@sim`-annotated fn in the workspace, paired with its wire name (the
/// `@sim("name")` string arg, else the fn name). The SINGLE source of the
/// `@sim` walk + wire derivation, shared by the codegen registry
/// ([`sim_registration_lines`]) and the rock-cli sim runner
/// (`collect_skull_sims`) so the registered name and the name the runner asks
/// to dispatch cannot drift.
pub fn collect_sim_fns(ws: &Workspace) -> Vec<(&bone_semantic::hir::Function, String)> {
    use bone_semantic::hir::AnnoValue;
    let mut sims = Vec::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (_fid, f) in module.functions.iter() {
            let Some(ann) = f.annotations.iter().find(|a| a.name.name.as_str() == "sim") else {
                continue;
            };
            let wire = match ann.args.first().map(|a| &a.value) {
                Some(AnnoValue::String(s)) => s.to_string(),
                _ => f.name.as_str().to_string(),
            };
            sims.push((f, wire));
        }
    }
    sims
}

/// `register_sim("<wire>", <mangled_fn>)` for every `@sim`-annotated fn,
/// workspace-wide -- the skull analog of rock-codegen's `sim_registrations.rs`.
/// Emitted at module-init time (after fn + global defs) so a skull-compiled game
/// registers its sims and the runtime's `ROCK_RUN_SIM=<wire>` dispatch resolves.
///
/// `register_sim` is itself a regular Rock fn (`stone/engine/sim.rock`), so it
/// is mangled the same way as its definition (every fn mangles by name +
/// signature, so definition and call agree). Every `@sim` implies stone/engine
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
fn register_sim_fn(ws: &Workspace) -> Option<FuncRef> {
    ws.modules.iter().find_map(|m| {
        m.functions
            .iter()
            .find(|(_, f)| f.name.as_str() == "register_sim")
            .map(|(fid, _)| FuncRef { module: m.id, func: fid })
    })
}

/// Every function this build roots from, by the emitter's own predicate.
///
/// It lives in the DRIVER, not in `skull_graph`: which declarations a build
/// roots from is a production decision (project layout, decorators, `@sim`,
/// `@test`), and the graph takes roots as an INPUT precisely so it cannot make
/// that decision itself. See [`all_roots`], which adds the generated-glue
/// obligations on top.
pub fn emission_roots(ws: &Workspace, project_root: Option<&str>) -> Vec<FuncRef> {
    let (deco_map, _) = build_deco_map(ws);
    let mut roots = Vec::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            if is_emission_root(module.file.as_str(), project_root, f, &deco_map)
            {
                roots.push(FuncRef { module: module.id, func: fid });
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
fn all_roots(
    ws: &Workspace,
    project_root: Option<&str>,
    shader_roots: &[FuncRef],
) -> Vec<FuncRef> {
    let mut roots = emission_roots(ws, project_root);
    let mut seen: FxHashSet<FuncRef> = roots.iter().copied().collect();
    let mut glue: Vec<FuncRef> = Vec::new();
    if !collect_sim_fns(ws).is_empty() {
        glue.extend(register_sim_fn(ws));
    }
    glue.extend(shader_roots.iter().copied());
    // Deterministic: `dependencies` is a hash set, so its iteration order is not.
    glue.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
    roots.extend(glue.into_iter().filter(|f| seen.insert(*f)));
    roots
}

fn sim_registration_lines(
    ws: &Workspace,
) -> Result<String, CompilerError> {
    let sims = collect_sim_fns(ws);
    if sims.is_empty() {
        return Ok(String::new());
    }

    let register_fn = register_sim_fn(ws).ok_or_else(|| {
        CompilerError::new_no_location(
            "internal: `register_sim` is not in scope yet `@sim` annotations exist \
             (skull) -- stone/engine must be linked. Please report."
                .to_string(),
        )
    })?;
    let register_name = mangle_fn(ws, &ws.module(register_fn.module).functions[register_fn.func]);

    let mut out = String::new();
    for (f, wire) in sims {
        let mangled = mangle_fn(ws, f);
        let _ = writeln!(out, "{}({:?}, {})", register_name, wire, mangled);
    }
    Ok(out)
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
        .map(|pair| crate::shader_artifact::ShaderProgram { vert: pair.vert, frag: pair.frag })
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
    for rejection in graph.rejections() {
        let caller = &ws.module(rejection.at.func.module).functions[rejection.at.func.func];
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
            };
            skull_mir::ablation::note_examined();
            skull_mir::ablation::record(
                category,
                if rejection.kind == crate::instance_graph::RejectionKind::ChangingInstanceRecursion {
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

/// `__rock_shader_register("<key>", <glsl>)` for every distinct `rock_shader`
/// pair (docs/shaders.md). Emitted at module-init time after
/// the `@sim` registrations, at chunk top level, so a reload re-`dofile`s them
/// with the new source (the hot-reload-parity invariant). Deterministic
/// sorted-by-key order (from `collect_shader_pairs`) keeps re-emission
/// byte-identical. The driver realizes the graph-owned shader view and passes
/// it to GLSL; skull-mir still surfaces only the resolved `FuncRef`s. Selected
/// WGPU projects every graph pair independently: GL registration remains
/// unconditional, while a pair whose WGSL projection cannot represent its
/// graph-owned interface is omitted from the WGPU artifact view and rejected
/// by the runtime before effects.
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
    // Always Core330. The profile is a property of the GL context, which a build
    // cannot know: the engine rewrites #version + precision for GLES at
    // shader-compile time (stone-gl `adapt_source_for_gles`), and that header is
    // the ONLY difference between the two profiles.
    let profile = skull_codegen_glsl::GlslProfile::Core330;
    if view.profile != profile || view.program_views.len() != collection.pairs.len() {
        return Err(CompilerError::new_no_location(
            "shader graph/view registration denominator differs from shader collection"
                .to_string(),
        ));
    }
    let mut out = String::new();
    let mut dependencies = FxHashSet::default();
    let mut abi_wires = BTreeMap::new();
    for (pair, program_view) in collection.pairs.iter().zip(&view.program_views) {
        // vert=None is the A3c single-arg fullscreen form -> the A1 single-entry
        // emitter (PIXEL body + fullscreen VERTEX passthrough).
        let emission = match pair.vert {
            Some(_) => skull_codegen_glsl::emit_shader_pipeline_with_view(ws, scx, program_view, profile)?,
            None => skull_codegen_glsl::emit_shader_entry_with_view(ws, scx, program_view, profile)?,
        };
        dependencies.extend(emission.dependencies);
        // Slot order is the engine's whole view of texture binding: it points
        // each sampler at its unit once at link time, then binds by slot.
        let slot_names: Vec<&str> = emission.abi.texture_units.iter().map(|n| n.as_str()).collect();
        // The `@uniform` blocks' std140 layouts, in binding order: A1 has at
        // most one, an A2 pipeline has one per stage. Carried as PARALLEL FLAT
        // lists -- one entry per block, plus the members concatenated with a
        // per-block count -- so a hand-written driver never nests. A program
        // with no group sends empty lists, which are still arguments; a short
        // call is what rock-cli/tests/shader_register_arity_gate.rs catches.
        let w = skull_codegen_glsl::BlockWire::of(emission.abi.uniform_blocks.iter(), emission.abi.uniform_array_blocks.iter());
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
        .chain(compute_collection.entries.iter().map(|entry| entry.key.clone()))
        .collect();
    let mut wgpu_projection_census = None;
    if renderer_target == RendererTarget::Wgpu {
        let items = collection
            .pairs
            .iter()
            .zip(&view.program_views)
            .map(|(pair, program_view)| {
                (pair.key.clone(), (pair.vert.is_none(), program_view))
            });
        let (artifacts, census) = collect_wgpu_projections(items, |key, (fullscreen, program_view)| {
            let emitted = (|| -> Result<(String, String, String, Vec<FuncRef>), CompilerError> {
                if fullscreen {
                    let stage_view = program_view
                        .stages
                        .iter()
                        .find(|stage| stage.stage == skull_mir::shader_ir::ShaderStage::Fragment)
                        .ok_or_else(|| {
                            CompilerError::new_no_location(format!(
                                "wgsl artifact {key} has no fragment stage view"
                            ))
                        })?;
                    let input = concrete_stage_input(stage_view, &program_view.concrete_plans);
                    let neutral = skull_codegen_shader::plan_fullscreen_fragment_from_concrete(
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
                        emission.abi.wire(),
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
                        emission.abi.wire(),
                        glsl.interface.semantic_wire(false),
                        emission.dependencies,
                    ))
                }
            })();
            emitted
        })?;
        for (shader_key, (source, wire, semantic_wire, emission_dependencies)) in artifacts {
            dependencies.extend(emission_dependencies);
            abi_wires.insert(shader_key.clone(), wire.clone());
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
    if view.compute_views.len() != compute_collection.entries.len() {
        return Err(CompilerError::new_no_location(format!(
            "compute graph/view registration denominator differs: {} views for {} roots",
            view.compute_views.len(),
            compute_collection.entries.len()
        )));
    }
    let has_compute = !compute_collection.entries.is_empty();
    if has_compute {
        out.push_str("__rock_wgpu_compute_snapshot_begin()\n");
    }
    for (entry, compute_view) in compute_collection.entries.iter().zip(&view.compute_views) {
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
        let wire = emission.abi.wire();
        abi_wires.insert(entry.key.clone(), wire.clone());
        let _ = writeln!(
            out,
            "__rock_wgpu_compute_register({}, {}, {}, {})",
            skull_codegen_lua::lua_string(&entry.key),
            skull_codegen_lua::lua_string(&emission.source),
            skull_codegen_lua::lua_string(&wire),
            skull_codegen_lua::lua_string(&neutral.interface.semantic_wire(false)),
        );
    }
    if has_compute {
        out.push_str("if not __rock_wgpu_compute_snapshot_end() then error(\"compiler compute snapshot failed\") end\n");
    }
    Ok(ShaderRegistrations {
        lua: out,
        dependencies,
        callsites: collection
            .callsites
            .iter()
            .copied()
            .chain(compute_collection.callsites.iter().copied())
            .collect(),
        roots: roots
            .into_iter()
            .chain(compute_collection.entries.iter().map(|entry| entry.entry))
            .collect(),
        keys,
        abi_wires,
        wgpu_projection_census,
    })
}

fn assemble(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    cache: &SkullEmitCache,
    built_specs: &mut Vec<(Instance, Chunk)>,
) -> Result<(MirEmit, FxHashSet<Instance>), CompilerError> {
    // Per-stage lower/cleanup/emit/regalloc times: chunk times (captured during the
    // parallel `emit_full`/`emit_incremental` chunk build) summed in as chunks are
    // pulled, plus the decorator/global units re-emitted here. Under parallel emit
    // these are summed per-unit elapsed time across workers (so they exceed the wall-clock emit
    // total `backend-stats` measures). `t_scaffold` is the directly-timed
    // SEQUENTIAL remainder (prelude/value-structs/type-tables + global-init
    // ordering + deco application).
    let mut times = PhaseTimes::default();
    let mut attribution = AttributionAccumulator::default();
    let mut t_scaffold = Duration::ZERO;
    let mut needed_specs: Vec<skull_mir::bound::SpecDemand> = Vec::new();
    let mut rejections = Rejections::default();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut direct_globals: FxHashMap<Ustr, Vec<Ustr>> = FxHashMap::default();
    let mut direct_calls: FxHashMap<Ustr, Vec<Ustr>> = FxHashMap::default();
    let mut all_statics: Vec<skull_mir::mir::StaticCell> = Vec::new();

    // IR-agnostic load-time scaffolding (shared with the old backend).
    let ts = Instant::now();
    let mut out = String::from(crate::runtime_prelude());
    out.push_str(&crate::enum_cdata_defs(ws));
    let (vdefs, value_emitted, value_gc) = crate::value_struct_defs(ws)?;
    out.push_str(&vdefs);
    out.push_str(&crate::value_struct_gc_defs(ws, &value_gc));
    out.push_str(&crate::value_struct_metatypes(ws, &value_emitted));
    out.push_str(&crate::enum_cdata_metatypes(ws));
    out.push_str(&crate::c_struct_inspect_defs(ws, &value_emitted));
    out.push_str(&crate::type_table_defs(ws, &value_gc));
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
    let mut always_emitted_calls: Vec<FuncRef> = Vec::new();
    let mut always_emitted_names: Vec<Ustr> = Vec::new();
    merge_units(deco_results, &mut out, &mut skipped, &mut direct_globals, &mut direct_calls, &mut Vec::new(), &mut always_emitted_calls, &mut always_emitted_names, &mut attribution, &mut times);

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
    // name->type snapshot table for the NEXT reload's preservation guards
    // (mirrors rock's `__rock_known_globals`). Built from the same `ws.render`
    // the per-init guards use, so an unchanged global's type compares equal.
    let known_globals: String = global_items
        .iter()
        .map(|(mid, gid)| {
            let g = &ws.module(*mid).globals[*gid];
            format!("[{:?}] = {:?},", g.name.as_str(), ws.render(None, g.ty))
        })
        .collect();
    let global_results = par_map(global_items, |(mid, gid)| {
        emit_global_unit(ws, scx, cg, &cache.concrete, mid, gid)
    });
    rejections.extend(unit_errors(&global_results));
    needed_specs.extend(global_results.iter().flat_map(|u| u.specs.iter().cloned()));
    let mut globals_lua = String::new();
    // Held back and appended after the fn-def skips so the reported order is
    // unchanged by the hoist.
    let mut global_skipped: Vec<(String, String)> = Vec::new();
    merge_units(global_results, &mut globals_lua, &mut global_skipped, &mut direct_globals, &mut direct_calls, &mut pending_inits, &mut always_emitted_calls, &mut always_emitted_names, &mut attribution, &mut times);

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

    // **Foreign-body (`inline(lua)`/`extern`) DELIVERY, separate from
    // RETENTION.** `lua_view` (and `should_emit_via_view`'s call-serving half)
    // answers "is this declaration's canonical Instance part of the program",
    // which is not the same question as "does anything call it in RESIDUAL
    // (non-spliced) form" -- `@pure @inline` stdlib operators (`+`, `-`, ...)
    // are retained (something calls `+`) but almost always fully spliced away
    // at every call site, so a naive retention-only gate emitted them
    // standalone anyway (found empirically: `__op_add_int_int_ret_int` and
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
            let self_ref = FuncRef { module: module.id, func: fid };
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
            let self_ref = FuncRef { module: mid, func: fid };
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
                    let nm = f.name.as_str();
                    let params = f.param_decls.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ");
                    let body = match extern_ret(ws, f)? {
                        // Returns an OWNING copy, never `__out[0]`. LuaJIT does
                        // not keep the parent array alive through a reference
                        // into it, so handing one back is a use-after-free that
                        // reads fine until the memory is reused
                        // (`tests/extern_value_return_ownership.rs`).
                        ExternRet::ValueStruct(sname) => {
                            let call_args = if params.is_empty() {
                                "__out".to_string()
                            } else {
                                format!("__out, {params}")
                            };
                            format!(
                                "  local __out = ffi.new(\"{sname}[1]\")\n  {lib}.{nm}({call_args})\n  return ffi.new(\"{sname}\", __out[0])"
                            )
                        }
                        ExternRet::Int => format!("  return tonumber({lib}.{nm}({params}))"),
                        ExternRet::Uint => format!("  return tonumber({lib}.{nm}({params}))"),
                        ExternRet::AsIs => format!("  return {lib}.{nm}({params})"),
                        ExternRet::None => format!("  {lib}.{nm}({params})"),
                    };
                    // The rejection above runs for EVERY extern; only the WRITE
                    // is gated. Pruning may change which definitions are emitted
                    // and nothing else -- gating the whole arm suppressed
                    // `extern_ret`'s unbound-generic rejection, whose subject is
                    // a generic declaration the view holds no base instance for.
                    if emit_def {
                        let _ = writeln!(out, "function {nm}({params})\n{body}\nend");
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
                let params = match skull_mir::lower::BackendFnAbiPlan::for_boundary_declaration(ws, scx, self_ref)
                    .lua_param_names(ws)
                {
                    Some(names) => names.join(", "),
                    None => f.param_decls.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "),
                };
                let body = body.as_str();
                let body = if f.return_ty.is_some() && !body.contains("return") {
                    format!("return {}", body.trim())
                } else {
                    body.to_string()
                };
                let mut prologue = String::new();
                if let Some(fbody) = ws.module(mid).bodies.get(&BodyOwner::Function(fid)) {
                    for (i, def) in fbody.param_defaults.iter().enumerate() {
                        if let (Some(d), Some(pd)) = (def, f.param_decls.get(i)) {
                            if let Some(lit) = render_default_lit(fbody, *d) {
                                let p = pd.name.as_str();
                                let _ = writeln!(prologue, "if {p} == nil then {p} = {lit} end");
                            }
                        }
                    }
                }
                let _ = writeln!(out, "function {name}({params})\n{prologue}{body}\nend");
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
                        out.push_str(&chunk.lua);
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
                        skipped.push((mangle_fn(ws, f), "not emitted (no cached chunk)".to_string()));
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
        &mut all_statics,
        &mut out,
    ));

    // Every emitted definition has now been visited, so this is the earliest
    // point where the whole rejection list exists. Anything past here is
    // scaffolding (init ordering, deco application) whose own errors are
    // unrelated to a body that cannot emit.
    rejections.into_result()?;

    // Globals: an init fn + a dependency-ordered `<NAME> = __skull_ginit_<NAME>()`.
    // (Lowered above, so the walk could see their call edges; appended here, in
    // the original slot, so the emitted text is unchanged.)
    out.push_str(&globals_lua);
    skipped.extend(global_skipped);

    // Order the init lines by their transitive global-read closure over the full
    // call graph (now that every fn's direct facts are recorded).
    let ts = Instant::now();
    let reach = transitive_global_reads(&direct_globals, &direct_calls);
    // Init ordering is stated over SOURCE globals, but a `nobox` global is READ
    // through its leaves, so a leaf dep has to resolve back to the group that
    // owns it or the dependent initializer sorts before it. Built from the
    // globals that actually mint leaves -- never a name-shape guess.
    let leaf_owner: FxHashMap<Ustr, Ustr> = ws
        .modules
        .iter()
        .flat_map(|m| m.globals.iter())
        .flat_map(|(_, g)| {
            skull_mir::lower::GlobalPlan::for_declared_type(ws, &cache.scx, g.ty)
                .ok()
                .and_then(|plan| plan.leaf_names(g.name))
                .into_iter()
                .flatten()
                .map(move |l| (l, g.name))
        })
        .collect();
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
    // the edited initializer; an unchanged key preserves the live cell. Register
    // both names in the snapshot so the next reload can compare. Dedup by cell.
    let mut seen_static: FxHashSet<Ustr> = FxHashSet::default();
    let mut static_resets = String::new();
    let mut static_regs = String::new();
    // The worker boot checkpoint's view (`docs/sim-speed-plan.md` B1): cell ->
    // done flag, stated rather than re-derived from a name suffix the way
    // `__rock_known_globals`' flat name->key listing would force.
    let mut static_pairs = String::new();
    for sc in &all_statics {
        if !seen_static.insert(sc.cell) {
            continue;
        }
        let _ = write!(
            static_pairs,
            "[{:?}] = {:?},",
            sc.cell.as_str(),
            sc.done_flag.as_str()
        );
        let key = static_cell_key(sc);
        let _ = write!(
            static_resets,
            "if _G.HOT_RELOAD and _G.__rock_old_globals and _G.__rock_old_globals[{:?}] ~= {:?} then {} = nil; {} = false end\n",
            sc.cell.as_str(), key, sc.cell, sc.done_flag
        );
        let _ = write!(
            static_regs,
            "[{:?}] = {:?},[{:?}] = {:?},",
            sc.cell.as_str(), key, sc.done_flag.as_str(), key
        );
    }

    // On a reload, snapshot the previous run's known globals so the per-global
    // guards can preserve live values whose type is unchanged.
    out.push_str("if _G.HOT_RELOAD then _G.__rock_old_globals = _G.__rock_known_globals end\n");
    out.push_str(&static_resets);
    for line in sort_global_init_lines(inits)? {
        out.push_str(&line);
    }
    // Record this run's globals for the next reload's snapshot.
    out.push_str("_G.__rock_known_globals = {");
    out.push_str(&known_globals);
    out.push_str(&static_regs);
    out.push_str("}\n");
    // Unconditional, an empty table included: a per-build registry that is only
    // assigned when non-empty leaves the PREVIOUS build's table live across a
    // hot reload, and a checkpoint would then restore cells this build does not
    // have (`docs/backends.md`, skull codegen invariants).
    out.push_str("_G.__ROCK_STATICS = {");
    out.push_str(&static_pairs);
    out.push_str("}\n");

    // Decorator APPLICATIONS (after global inits so a deco body can push a global).
    out.push_str(&decorator_application_lines(ws, &deco_map, &mut skipped));
    // `@sim` registrations (after fn + global defs; same init slot as rock).
    out.push_str(&sim_registration_lines(ws)?);
    // `rock_shader` pipeline registrations (docs/shaders.md):
    // one top-level `__rock_shader_register(key, glsl)` per distinct pair, so a
    // reload re-`dofile`s them with the new source (hot-reload parity).
    out.push_str(&cache.shader_registrations.lua);
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
    if let Some(err) = cg.take_rejection() {
        return Err(err);
    }
    let mut emitted_bodies: FxHashSet<Instance> = FxHashSet::default();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            if f.backend == bone_semantic::hir::FnBackend::Js {
                continue;
            }
            let self_ref = FuncRef { module: module.id, func: fid };
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
    emitted_bodies.extend(spec_chunks.keys().cloned());
    Ok((
        MirEmit {
            lua: out,
            skipped,
            phases,
            abi_plan_stats: scx.abi_plan_stats(),
            attribution: attribution.finish(),
            arena_traffic: ArenaTraffic::default(),
            setup_lanes: SetupLaneAttribution::default(),
            graph_census: crate::instance_graph::RunStats::default(),
            concrete_stats: crate::concrete_body::ConcreteBuildStats::default(),
        },
        emitted_bodies,
    ))
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
/// `@on_reload` registers its callback BY NAME (re-resolved via `_G[name]` on
/// each fire so an edited body propagates -- see `on_hot_reload_by_name` in the
/// stdlib), so the Lua global must keep the source name -- i.e. the fn must be
/// `@nomangle`. A same-named overload would otherwise mangle it and the lookup
/// would silently miss. Reject before emit; runs on every full/interface build
/// (adding `@on_reload` is a structural edit -> full rebuild).
fn validate_on_reload_nomangle(ws: &Workspace) -> Result<(), CompilerError> {
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (_fid, f) in module.functions.iter() {
            let Some(ann) = f.annotations.iter().find(|a| a.name.name.as_str() == "on_reload")
            else {
                continue;
            };
            if !f.annotations.iter().any(|a| a.name.name.as_str() == "nomangle") {
                return Err(CompilerError::new_with_label(
                    ann.span,
                    format!(
                        "`@on_reload` requires `@nomangle` on `{}`: the callback is re-resolved \
                         by name on each hot reload, so its Lua global must keep the source name. \
                         Add `@nomangle`.",
                        f.name.as_str()
                    ),
                ));
            }
        }
    }
    Ok(())
}

pub fn emit_full(
    ws: &Workspace,
    user_mid: ModuleId,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    emit_full_in_for_renderer(ws, user_mid, None, RendererTarget::Gl)
}

/// Absolutize `project_root` so it can be prefix-matched against module file
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
    let Some(root) = project_root else { return Ok(None) };
    let abs = std::fs::canonicalize(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| root.to_string());
    let matches = ws
        .modules
        .iter()
        .any(|m| !is_stdlib_module_file(m.file.as_str()) && m.file.as_str().starts_with(&abs));
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
    emit_full_in_with_mutation(
        ws,
        user_mid,
        project_root,
        BodyMutation::None,
        renderer_target,
    )
}

#[cfg(test)]
fn emit_full_in_with_test_mutation(
    ws: &Workspace,
    user_mid: ModuleId,
    mutation: BodyMutation,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    emit_full_in_with_mutation(ws, user_mid, None, mutation, RendererTarget::Gl)
}

fn emit_full_in_with_mutation(
    ws: &Workspace,
    user_mid: ModuleId,
    project_root: Option<&str>,
    mutation: BodyMutation,
    renderer_target: RendererTarget,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    let mut setup_phases = Vec::new();
    let t_setup = Instant::now();
    let project_root = canonical_project_root(ws, project_root)?;
    let project_root = project_root.as_deref();
    validate_on_reload_nomangle(ws)?;
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
    let cg = CodegenCtx::new(ws);
    setup_phases.push(("setup: validation+context", t_setup.elapsed()));

    let t_setup = Instant::now();
    // **The graph is built FIRST, into a fresh arena, and `scx` shares that
    // arena rather than minting its own.** An `Instance` the graph mints has to
    // mean something to lowering, and a second arena would be a second identity
    // for the same instance. See `SkullEmitCache::graph`.
    // Generated glue is part of the ROOT INPUT. Shader declarations are known
    // before the graph; their GLSL bodies are rendered only after the view owns
    // the retained instances.
    let shader_collection = skull_mir::lower::collect_shader_collection(ws)?;
    let shader_programs = shader_programs(&shader_collection);
    let compute_collection = skull_mir::lower::collect_compute_collection(ws)?;
    let compute_programs = compute_programs(&compute_collection);
    let shader_roots: Vec<FuncRef> = shader_programs
        .iter()
        .flat_map(|program| program.vert.into_iter().chain(std::iter::once(program.frag)))
        .chain(compute_programs.iter().map(|program| program.entry))
        .collect();
    let emission_roots = all_roots(ws, project_root, &shader_roots);
    setup_phases.push(("setup: root collection", t_setup.elapsed()));

    let t_setup = Instant::now();
    let mut graph = crate::instance_graph::build(ws, emission_roots.clone(), false);
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
            crate::concrete_body::build(ws, &mut graph)
        }
        #[cfg(not(test))]
        {
            crate::concrete_body::build(ws, &mut graph)
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
            let witness = plan
                .bound_tree
                .next
                .values_mut()
                .find_map(|next| match next {
                    skull_mir::bound::ResolvedBoundNext::Concrete { instance, .. }
                        if !instance.args.is_empty() => Some(instance),
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
    if let Some((instance, reason)) = concrete.plan_failures.first() {
        let f = &ws.module(instance.func.module).functions[instance.func.func];
        return Err(CompilerError::new_with_label(
            f.name_span,
            format!("cannot build concrete lowering plan for `{}`: {:?}", f.name, reason),
        ));
    }
    if let Some((owner, reason)) = concrete.always_plan_failures.first() {
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
        BodyMutation::SetupLaneSchedule(schedule) => Some(std::sync::Arc::new(
            SetupLaneScheduler::new(schedule),
        )),
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
                    setup_scheduler
                        .as_deref()
                        .map(|scheduler| scheduler.wait(SetupLaneKind::CpuArtifact));
                    let started = Instant::now();
                    let lua_view = crate::artifact::realize(
                        ws,
                        &graph,
                        &crate::artifact::ArtifactRequest::new(
                            crate::target_projection::LUAJIT_DESKTOP,
                            &emission_roots,
                        ),
                    );
                    let population = lua_view.instances.len() + lua_view.boundaries.len();
                    let scx = skull_mir::lower::ScalarCtx::new(ws)
                        .sharing(std::sync::Arc::clone(graph.interner()));
                    let finished = Instant::now();
                    #[cfg(all(test, not(feature = "sequential")))]
                    setup_scheduler
                        .as_deref()
                        .map(|scheduler| scheduler.finish(SetupLaneKind::CpuArtifact));
                    (
                        lua_view,
                        scx,
                        SetupLaneWindow { started, finished, population },
                    )
                },
                || {
                    #[cfg(all(test, not(feature = "sequential")))]
                    setup_scheduler
                        .as_deref()
                        .map(|scheduler| scheduler.wait(SetupLaneKind::Shader));
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
                    let population = result.as_ref().map(|(_, _, population)| *population).unwrap_or(0);
                    let finished = Instant::now();
                    #[cfg(all(test, not(feature = "sequential")))]
                    setup_scheduler
                        .as_deref()
                        .map(|scheduler| scheduler.finish(SetupLaneKind::Shader));
                    (
                        result.map(|(view, registrations, _)| (view, registrations)),
                        SetupLaneWindow { started, finished, population },
                    )
                },
            )
        },
        || {
            #[cfg(all(test, not(feature = "sequential")))]
            setup_scheduler
                .as_deref()
                .map(|scheduler| scheduler.wait(SetupLaneKind::AbiWarm));
            let started = Instant::now();
            let mut abi_scx = skull_mir::lower::ScalarCtx::new(ws)
                .sharing(std::sync::Arc::clone(graph.interner()));
            if !skip_abi_plan_publish {
                abi_scx.warm_abi_plans(ws, &abi_plans, abi_always_plans);
            }
            let population = abi_scx.abi_plan_stats().warm_keys as usize;
            let finished = Instant::now();
            #[cfg(all(test, not(feature = "sequential")))]
            setup_scheduler
                .as_deref()
                .map(|scheduler| scheduler.finish(SetupLaneKind::AbiWarm));
            (
                abi_scx,
                SetupLaneWindow { started, finished, population },
            )
        },
        )
    };
    #[cfg(all(test, not(feature = "sequential")))]
    let ((cpu_result, shader_result), (abi_scx, abi_lane)) =
        if matches!(mutation, BodyMutation::SetupLaneSchedule(SetupLaneSchedule::Handshake)) {
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
    setup_phases.push(("join: cpu artifact branch", cpu_lane.finished - cpu_lane.started));
    setup_phases.push(("join: shader view branch", shader_lane.finished - shader_lane.started));
    setup_phases.push(("join: ABI plan warm branch", abi_lane.finished - abi_lane.started));

    // The emitted param list must be injective. Checked per SIGNATURE, over every
    // function rather than just the emitted set: a duplicate Lua param is illegal
    // regardless of which filter would have reached the body.
    let t_setup = Instant::now();
    for m in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (fid, _) in m.functions.iter() {
            let fref = FuncRef { module: m.id, func: fid };
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
            m.functions
                .iter()
                .filter_map(move |(fid, f)| {
                    let self_ref = FuncRef { module: m.id, func: fid };
                    (is_cached_body(f) && should_emit_via_view(ws, view, self_ref))
                        .then_some((self_ref, f))
                })
        })
        .collect();
    let chunks = par_map_with(
        fn_items,
        skull_transform::InlineCache::default,
        |inline_cache, (self_ref, f)| {
            (
                shared(self_ref),
                emit_body_chunk_with_mutation(ws, &cg, &scx, &concrete, self_ref, f, inline_cache, mutation),
            )
        },
    );
    let chunks: FxHashMap<Instance, Result<Chunk, CompilerError>> = chunks.into_iter().collect();
    // Seed the reverse dependency index from every chunk's codegen deps.
    let mut dependents: FxHashMap<FuncRef, FxHashSet<Instance>> = FxHashMap::default();
    for (body, chunk) in &chunks {
        if let Ok(c) = chunk {
            for d in &c.deps {
                dependents.entry(*d).or_default().insert(body.clone());
            }
        }
    }
    // Baseline every `static` cell's reload key so the overlay can detect an
    // initializer edit (changed key) vs an unrelated body edit (unchanged key).
    let mut static_key_baseline: FxHashMap<Ustr, String> = FxHashMap::default();
    for chunk in chunks.values().flatten() {
        for sc in &chunk.statics {
            static_key_baseline.insert(sc.cell, static_cell_key(sc));
        }
    }
    let concrete_stats = concrete.build_stats;
    let cache = SkullEmitCache {
        chunks,
        concrete,
        graph,
        lua_view,
        changed_since_full: FxHashSet::default(),
        cg,
        scx,
        inline_cache: skull_transform::InlineCache::default(),
        dependents,
        changed_consts_since_full: FxHashSet::default(),
        static_key_baseline,
        shader_registrations,
        shader_view,
        emitted: FxHashSet::default(),
        project_root: project_root.map(str::to_string),
        renderer_target,
    };
    let mut built_specs: Vec<(Instance, Chunk)> = Vec::new();
    let (mut emit, emitted) = assemble(ws, &cache.cg, &cache.scx, &cache, &mut built_specs)?;
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
    let mut cache = cache;
    cache.emitted = emitted;
    cache.absorb_spec_chunks(built_specs);
    emit.graph_census = cache.graph.run_stats;
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
impl SkullEmitCache {
    /// The authoritative instance graph this cache emits against. Always
    /// present -- there is no build in which production has no graph.
    pub fn graph(&self) -> &crate::instance_graph::InstanceGraph {
        &self.graph
    }

    /// The shader artifact view used by the current renderer registration.
    pub fn shader_view(&self) -> &crate::shader_artifact::ShaderArtifactView {
        &self.shader_view
    }

    /// The Lua artifact's realization of [`Self::graph`], from the same roots
    /// the graph was built/updated from. Recomputed every edit; see the field.
    pub fn lua_view(&self) -> &crate::artifact::ArtifactView {
        &self.lua_view
    }

    /// A live shader registry cannot remove a key from the running process:
    /// registration lines only add or replace entries. A key-set change must
    /// therefore take the process-restarting reload path before any overlay is
    /// written.
    pub fn shader_registration_keys_changed(
        &self,
        ws: &Workspace,
    ) -> Result<bool, CompilerError> {
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
    let collection = skull_mir::lower::collect_shader_collection(ws)?;
    let compute_collection = skull_mir::lower::collect_compute_collection(ws)?;
    self.shader_registrations.roots = collection
        .pairs
        .iter()
        .flat_map(|pair| pair.vert.into_iter().chain(std::iter::once(pair.frag)))
        .chain(compute_collection.entries.iter().map(|entry| entry.entry))
        .collect();
    self.shader_registrations.callsites = collection
        .callsites
        .iter()
        .copied()
        .chain(compute_collection.callsites.iter().copied())
        .collect();
    Ok(true)
}

    fn refresh_shader_registrations(
        &mut self,
        ws: &Workspace,
    ) -> Result<(), CompilerError> {
    let collection = skull_mir::lower::collect_shader_collection(ws)?;
    let programs = shader_programs(&collection);
    let compute_collection = skull_mir::lower::collect_compute_collection(ws)?;
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
fn absorb_spec_chunks(&mut self, built: Vec<(Instance, Chunk)>) {
    for (body, chunk) in built {
        for d in &chunk.deps {
            self.dependents.entry(*d).or_default().insert(body.clone());
        }
        self.chunks.insert(body, Ok(chunk));
    }
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
) -> Vec<CompilerError> {
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
        if cache.chunks.get(&body).is_some_and(|chunk| chunk.is_ok()) {
            if !cache.emitted.contains(&body) {
                cache.changed_since_full.insert(body.clone());
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
        for dep in &chunk.deps {
            cache.dependents.entry(*dep).or_default().insert(body.clone());
        }
        cache.chunks.insert(body.clone(), Ok(chunk));
        cache.changed_since_full.insert(body);
    }
    errs
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
) -> (
    FxHashSet<Instance>,
    crate::instance_graph::UpdateStats,
    Vec<CompilerError>,
) {
    let cache = self;
    // Fold each re-inferred body's value-reference contribution into the cached
    // ScalarCtx. A name whose taken-by-ref membership flips changes scalar-ABI
    // eligibility, so the named fn(s) -- and, via the deps scan below, every
    // direct caller -- must be re-emitted. `flipped_frefs` seeds `changed`.
    let mut flipped_frefs: FxHashSet<FuncRef> = FxHashSet::default();
    for (mid, owner) in reinferred {
        for name in cache.scx.update_body(ws, *mid, *owner) {
            if let Some(list) = ws.functions_by_name.get(&name) {
                for (m, fid) in list.iter() {
                    flipped_frefs.insert(FuncRef { module: *m, func: *fid });
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
    let _t_graph = Instant::now();
    // `update_shader_registrations` runs BEFORE this (in `emit_incremental`), so
    // the glue roots below are this edit's, not the previous build's.
    let roots = all_roots(
        ws,
        cache.project_root.as_deref(),
        &cache.shader_registrations.roots,
    );
    let update = cache.graph.update_bodies(ws, roots.clone(), reinferred);
    cache.concrete = crate::concrete_body::build(ws, &mut cache.graph);
    let _graph_ms = _t_graph.elapsed();

    // Before any borrow of the cache's own fields, because it mutates several.
    for gone in &update.removed {
        cache.forget_instance(gone);
    }

    // The graph moved; the Lua view is this edit's realization of it, from the
    // same roots. Kept in step with `graph` rather than a first-edit snapshot.
    cache.lua_view = crate::artifact::realize(
        ws,
        &cache.graph,
        &crate::artifact::ArtifactRequest::new(crate::target_projection::LUAJIT_DESKTOP, &roots),
    );

    let scx = &cache.scx;
    let cg = &cache.cg;

    let _t_aff = Instant::now();
    let mut changed: FxHashSet<FuncRef> = reinferred
        .iter()
        .filter_map(|(mid, owner)| match owner {
            BodyOwner::Function(fid) => Some(FuncRef { module: *mid, func: *fid }),
            _ => None,
        })
        .collect();
    // Fns whose scalar ABI flipped (a body now takes them by ref, or stopped) must
    // re-emit even if their own body didn't change.
    changed.extend(flipped_frefs.iter().copied());

    // **A changed declaration maps to its LIVE INSTANCES through the graph**,
    // not to one declaration-shaped key. Editing a generic `co fn` with two
    // instantiations must re-emit both bodies, and editing one specialization's
    // declaration must not be indistinguishable from editing the other's.
    // `Instance::base` is still taken for every changed fn: a non-generic one
    // IS its base instance, and a declaration the graph never reached still has
    // a chunk here that an edit can invalidate.
    let mut affected: FxHashSet<Instance> = changed.iter().copied().map(shared).collect();
    for c in &changed {
        affected.extend(cache.graph.instances_of(*c).cloned());
    }
    // ...plus every chunk that DEPENDS on a changed fn. This is the CODEGEN
    // reverse graph, which the instance graph does not hold and must not be
    // confused with `Indices::preds`: `preds` is call edges between instances,
    // while `deps` records that a chunk's TEXT contains another declaration's
    // body (an inline splice) or specializes it. Neither is derivable from the
    // other, so both survive.
    for c in &changed {
        if let Some(deps) = cache.dependents.get(c) {
            affected.extend(deps.iter().cloned());
        }
    }
    // REMOVAL is the graph's to report and emission's alone cannot see it:
    // nothing on this side knows an instance was stranded, so a surviving chunk
    // makes `emit_incremental` re-emit a definition no call reaches. That is
    // handled above, before the borrows.
    //
    // Construction of graph additions happens after invalidation below, so a
    // newly reached body cannot reuse an inline decision made for the old body.
    let _aff_ms = _t_aff.elapsed();

    // The persistent InlineCache keeps unchanged callee bodies across edits; the
    // affected entries are exactly those whose lowered body could have changed
    // (own body edit, own ABI flip, or a callee's ABI flip -- all within
    // `affected`), so invalidate them before re-lowering. Everything else is
    // provably unchanged and reused.
    let _t_relow = Instant::now();
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
            let chunk = rebuild_chunk(ws, cg, scx, &cache.concrete, body, f, &mut cache.inline_cache);
            if let Ok(c) = &chunk {
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
    let added_errors = Self::build_graph_added_chunks(cache, ws, &update.added);
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
    (affected, update, added_errors)
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
    let (_affected, update, added_errors) = cache.update_chunk_cache(ws, reinferred);
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
    let (mut emit, emitted) = assemble(ws, &cache.cg, &cache.scx, cache, &mut built_specs)?;
    emit.concrete_stats = cache.concrete.build_stats;
    cache.emitted = emitted;
    cache.absorb_spec_chunks(built_specs);
    let reassemble = t_asm.elapsed();
    emit.phases.push(("incr: re-emit affected bodies", reemit));
    emit.phases.push(("incr: reassemble full output", reassemble));
    Ok(emit)
}

/// Assemble the incremental OVERLAY (`incremental.lua`): the cached chunks of the
/// functions changed since the last full build, then a system rebind
/// (`clear_all_systems()` + every decorator application) so the system registries
/// re-point at the CURRENT globals (the changed fns, redefined above, win).
/// Loaded AFTER `main.lua`; redefined globals override their stale defs. Emits NO
/// scaffold/globals -- those are unchanged on a body edit and live in main.lua.
/// `docs/skull-incremental-emit-split.md`.
fn assemble_overlay(
    ws: &Workspace,
    cache: &SkullEmitCache,
    shader_changed: bool,
) -> MirEmit {
    let mut out = String::from("-- skull incremental overlay (loaded after main.lua)\n");
    let mut skipped: Vec<(String, String)> = Vec::new();

    // Redefine the changed functions, deterministic order for a stable diff.
    let mut order: Vec<Instance> = cache.changed_since_full.iter().cloned().collect();
    order.sort_by_key(|b| overlay_order_key(&cache.scx, b));
    order.dedup();
    for body in &order {
        match cache.chunks.get(body) {
            Some(Ok(chunk)) => out.push_str(&chunk.lua),
            Some(Err(err)) => {
                let fref = body.func;
                let f = &ws.module(fref.module).functions[fref.func];
                skipped.push((mangle_fn(ws, f), err.message.clone()));
            }
            // extern / inline(lua) edits are interface-tier -> full rebuild path.
            None => {}
        }
    }

    // Reset `static` cells whose initializer MEANING changed since the last full
    // build (`type@init_hash` key differs from the baseline). A static is
    // name-keyed and lazy, so redefining its fn above leaves the warmed cell +
    // set done-flag intact -- the redefined fn would re-read the stale value. Nil
    // the cell + clear its done-flag UNGUARDED so the next reach re-inits with the
    // edited initializer; an unchanged key is skipped (an unrelated body edit
    // preserves the live cell). Symmetric with the const re-emit below. Only the
    // changed fns' statics are considered -- a static's key can't change without
    // its enclosing fn being re-lowered into `changed_since_full`.
    let mut seen_static: FxHashSet<Ustr> = FxHashSet::default();
    for body in &order {
        let Some(Ok(chunk)) = cache.chunks.get(body) else { continue };
        for sc in &chunk.statics {
            if !seen_static.insert(sc.cell) {
                continue;
            }
            let changed = cache
                .static_key_baseline
                .get(&sc.cell)
                .is_none_or(|base| *base != static_cell_key(sc));
            if changed {
                let _ = write!(out, "{} = nil; {} = false\n", sc.cell, sc.done_flag);
            }
        }
    }

    // Re-emit only the AFFECTED `const` inits when a const changed since the last
    // full build: the edited const(s) PLUS their transitive DEPENDENTS (consts
    // whose init reads a changed const, directly or through a function). An
    // unrelated const is left untouched in main.lua. The dependency graph is the
    // SAME symbol-reference info goto-def / references / the re-inference cascade
    // use -- `BodyInference.deps` (the names each body references). Globals are
    // ALWAYS re-inferred on a body edit, so these deps are fresh (no caching, no
    // staleness). Each affected const emits an UNGUARDED
    // `<NAME> = __skull_ginit_<NAME>()` line in dependency order; a CHANGED const
    // also re-emits its `__skull_ginit_<NAME>` fn (new value), while an unchanged
    // dependent reuses the ginit already in main.lua and only re-runs its line
    // (after its upstream is reassigned). Mutable globals are never re-emitted:
    // their guard preserves the live value across reload.
    if !cache.changed_consts_since_full.is_empty() {
        // Forward symbol-reference graph (name -> names it references) from the
        // CURRENT inference. Overloads merge under one name (conservative: at worst
        // re-emits an extra const, never misses one).
        let mut deps: FxHashMap<Ustr, FxHashSet<Ustr>> = FxHashMap::default();
        for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
            let Some(inf) = ws.inference(module.id) else { continue };
            for (fid, f) in module.functions.iter() {
                if let Some(bi) = inf.per_fn.get(&BodyOwner::Function(fid)) {
                    deps.entry(f.name).or_default().extend(bi.deps.iter().copied());
                }
            }
            for (gid, g) in module.globals.iter() {
                if let Some(bi) = inf.per_fn.get(&BodyOwner::Global(gid)) {
                    deps.entry(g.name).or_default().extend(bi.deps.iter().copied());
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
            changed: bool,
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
                affected.push(Affected { mid, gid, name, changed, reach });
            }
        }
        let affected_names: FxHashSet<Ustr> = affected.iter().map(|a| a.name).collect();

        // Emit each affected const's ginit fn (changed only) now; build its init
        // line + dependency edges (restricted to affected consts) for the sort.
        let mut inits: Vec<GlobalInit> = Vec::new();
        for a in &affected {
            let line = if a.changed {
                let unit = emit_global_unit(
                    ws,
                    &cache.scx,
                    &cache.cg,
                    &cache.concrete,
                    a.mid,
                    a.gid,
                );
                out.push_str(&unit.lua); // the (new) ginit fn def
                if let Some(s) = unit.skipped {
                    skipped.push(s);
                }
                unit.pending_init.map(|(_, _, l)| l).unwrap_or_default()
            } else {
                // Unchanged dependent: its ginit is already in main.lua; re-run only
                // the line so it recomputes from the upstream's new value.
                let g = &ws.module(a.mid).globals[a.gid];
                format!(
                    "{} = {}()\n",
                    global_init_target(ws, &cache.scx, &cache.cg, g),
                    crate::global_init_fn_name(a.name.as_str())
                )
            };
            let edges: Vec<Ustr> = a
                .reach
                .iter()
                .copied()
                .filter(|n| *n != a.name && affected_names.contains(n))
                .collect();
            inits.push(GlobalInit { name: a.name, deps: edges, line });
        }

        match sort_global_init_lines(inits) {
            Ok(lines) => out.push_str(&lines.concat()),
            Err(e) => skipped.push(("const init ordering".to_string(), e.message)),
        }
    }

    // Rebind: re-point ONLY the changed decorated functions' registry entries at
    // their redefined globals, in place (`__rock_rebind_system`). A registry holds
    // a system's function VALUE, so redefining a decorated global leaves the entry
    // stale -- but only for the functions that actually changed. Re-pointing just
    // those (vs clear-all + re-register-all) keeps the rebind proportional to the
    // edit, not the whole system count, and a body edit touching no decorated fn
    // emits no rebind at all. (`docs/skull-incremental-emit-split.md`.)
    let (deco_map, _) = build_deco_map(ws);
    // A decorator rebind is a DECLARATION fact -- it names the system by its
    // source name -- so specializations of one declaration contribute it once.
    let mut rebind_decls: Vec<FuncRef> = order.iter().map(|i| i.func).collect();
    rebind_decls.sort_by_key(|fr| (fr.module.index(), u32::from(fr.func.into_raw())));
    rebind_decls.dedup();
    out.push_str(&decorator_rebind_lines(ws, &deco_map, &rebind_decls));

    if shader_changed {
        out.push_str(&cache.shader_registrations.lua);
    }

    MirEmit {
        lua: out,
        skipped,
        phases: Vec::new(),
        abi_plan_stats: cache.scx.abi_plan_stats(),
        attribution: EmitAttribution::default(),
        arena_traffic: ArenaTraffic::default(),
        setup_lanes: SetupLaneAttribution::default(),
        graph_census: crate::instance_graph::RunStats::default(),
        concrete_stats: crate::concrete_body::ConcreteBuildStats::default(),
    }
}

/// `__rock_rebind_system("<name>", <func>)` for each CHANGED decorated function
/// (those in the overlay's changed set that carry a decorator) -- re-points its
/// system-registry entry at the redefined global, in place. The real engine's
/// decorators all register into the system registries, so the one helper covers
/// them; a changed non-decorated function (or none) emits nothing here.
fn decorator_rebind_lines(
    ws: &Workspace,
    deco_map: &DecoMap,
    changed: &[FuncRef],
) -> String {
    let mut out = String::new();
    for &fref in changed {
        let f = &ws.module(fref.module).functions[fref.func];
        if !f.annotations.iter().any(|ann| deco_map.contains_key(&ann.name.name)) {
            continue;
        }
        let mangled = mangle_fn(ws, f);
        let _ = writeln!(out, "__rock_rebind_system({:?}, {})", f.name.as_str(), mangled);
    }
    out
}

/// Incremental OVERLAY emit for a body edit: update the chunk cache for the
/// re-inferred bodies, accumulate them into `changed_since_full`, and assemble
/// `incremental.lua` from the WHOLE set (so an earlier edit isn't reverted to
/// main.lua's stale copy -- see the accumulation rule in
/// `docs/skull-incremental-emit-split.md`). The caller writes the result to
/// `tmp/incremental.lua` (loaded after `tmp/main.lua`); on an interface/full
/// rebuild the caller uses `emit_full` and clears `changed_since_full`.
pub fn emit_overlay(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> Result<MirEmit, CompilerError> {
    let t0 = Instant::now();
    // `cg` is cached (interface-derived, identical on body edits) -- no rebuild.
    let t_cg = t0.elapsed();
    let t1 = Instant::now();
    // `scx` is maintained incrementally inside `update_chunk_cache` (per-body diff)
    // -- no whole-workspace rebuild.
    let t_scx = t1.elapsed();
    let t2 = Instant::now();
    // Shader registrations FIRST -- their dependencies are graph/view roots, so
    // `update_chunk_cache`'s re-walk must see this edit's set.
    let shader_changed = cache.update_shader_registrations(ws, reinferred)?;
    let (affected, update, added_errors) = cache.update_chunk_cache(ws, reinferred);
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
    let t_relower = t2.elapsed();
    let t3 = Instant::now();
    let mut emit = assemble_overlay(ws, cache, shader_changed);
    emit.concrete_stats = cache.concrete.build_stats;
    // Advance the static baseline to the keys now in effect. A reset line fires on
    // the reload that dofiles the overlay carrying it, so once THIS overlay resets a
    // cell (or leaves it unchanged), a LATER overlay must not reset it again -- else
    // an unrelated follow-up edit would re-run the initializer and wipe accumulated
    // state. Bring each changed fn's cells up to their current key so the next
    // overlay only resets cells whose key changes AGAIN.
    let advanced: Vec<(Ustr, String)> = cache
        .changed_since_full
        .iter()
        .filter_map(|fref| cache.chunks.get(fref).and_then(|c| c.as_ref().ok()))
        .flat_map(|chunk| chunk.statics.iter().map(|sc| (sc.cell, static_cell_key(sc))))
        .collect();
    for (cell, key) in advanced {
        cache.static_key_baseline.insert(cell, key);
    }
    let t_asm = t3.elapsed();
    // A body edit that mints a newly-colliding synthetic name is not safe to
    // deliver as an overlay.
    if let Some(err) = cache.cg.take_rejection() {
        return Err(err);
    }
    emit.phases = vec![
        ("CodegenCtx::new", t_cg),
        ("ScalarCtx::new", t_scx),
        ("update_chunk_cache (re-lower changed)", t_relower),
        ("assemble_overlay (+build_deco_map)", t_asm),
    ];
    Ok(emit)
}

#[cfg(test)]
mod wgpu_projection_tests {
    use super::*;

    #[test]
    fn unexpected_emitter_failure_aborts_after_admission() {
        let unexpected = CompilerError::new_no_location("unexpected WGSL emitter failure");
        let mut visited = Vec::new();
        let result = collect_wgpu_projections(
            vec![("admitted".to_string(), true), ("unexpected".to_string(), false)],
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
        assert_eq!(error.kind, CompilerErrorKind::Generic);
        assert_eq!(visited, ["admitted", "unexpected"]);
    }
}

#[cfg(all(test, not(feature = "sequential")))]
mod setup_parallel_tests {
    use super::*;

    #[cfg(not(feature = "sequential"))]
    #[test]
    fn independent_setup_branches_can_overlap() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("two-thread test pool");
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        pool.install(|| {
            parallel_join(
                {
                    let active = Arc::clone(&active);
                    let peak = Arc::clone(&peak);
                    move || {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                },
                {
                    let active = Arc::clone(&active);
                    let peak = Arc::clone(&peak);
                    move || {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                },
            );
        });
        assert_eq!(peak.load(Ordering::SeqCst), 2);
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
fn top() -> int {
    let counter = 100
    let add = (n: int) => n + counter
    return add(5)
}

fn main() { let result = top() }
"#;

    const SETUP_SRC: &str = r#"
@nomangle inline(lua) fn publish(value: int) -> int = "return value"

@shader(fragment)
fn frag(tc: Vec2) -> Vec4 { return v(tc.x, 0.0, 0.0, 1.0) }

fn main() {
    let shader = rock_shader(frag)
    let published = publish(7)
}
"#;

    static STDLIB: std::sync::LazyLock<Vec<skull_parser::Ast>> = std::sync::LazyLock::new(|| {
        rock_host::stdlib_loader::stdlib_files()
            .iter()
            .map(|file| {
                let id = Ustr::from(format!("<stdlib:{}>", file.name).as_str());
                skull_parser::parse_module_clean(file.contents, id).expect("stdlib parse")
            })
            .collect()
    });

    static INLINE_OFF: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[allow(dead_code)]
    struct InlineOff(std::sync::MutexGuard<'static, ()>, Option<String>);

    impl InlineOff {
        fn on() -> Self {
            let guard = INLINE_OFF.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("SKULL_INLINE_OFF").ok();
            unsafe { std::env::set_var("SKULL_INLINE_OFF", "1") };
            skull_mir::ablation::clear();
            skull_mir::ablation::set_enabled(true);
            Self(guard, previous)
        }
    }

    impl Drop for InlineOff {
        fn drop(&mut self) {
            skull_mir::ablation::set_enabled(false);
            skull_mir::ablation::clear();
            match &self.1 {
                Some(value) => unsafe { std::env::set_var("SKULL_INLINE_OFF", value) },
                None => unsafe { std::env::remove_var("SKULL_INLINE_OFF") },
            }
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

    fn workspace() -> Workspace {
        let mut modules = vec![
            skull_parser::parse_module_clean(SRC, Ustr::from("<missing-owner-test>"))
                .expect("parse"),
        ];
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
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
            skull_parser::parse_module_clean(COROUTINE_PLAN_SRC, Ustr::from("<coroutine-plan-test>"))
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

    fn setup_workspace() -> Workspace {
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(SETUP_SRC, Ustr::from("<setup-lane-test>"))
                .expect("parse"),
        );
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
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
                    .map(|(func, _)| FuncRef { module: module.id, func })
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
        assert!(
            emit.lua.contains(&format!("function {symbol}(")),
            "the concrete residual definition is missing: {symbol}\n{}",
            emit.lua
        );
        assert!(
            emit.lua
                .lines()
                .any(|line| !line.starts_with("function ") && line.contains(&format!("{symbol}("))),
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
                err.message.contains("no concrete owner agreeing with its delivery instance"),
                "owner loss must be an explicit rejection, not an unrelated emission gap: {err:?}"
            ),
            Ok((ws, cache, emit)) => assert_residual_definition_and_call(&ws, &cache, &emit, callee),
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
        let err = match emit_full_in_with_test_mutation(&ws, mid, BodyMutation::ClearBoundPlan { body }) {
            Ok(_) => panic!("emission ignored the poisoned concrete bound plan"),
            Err(err) => err,
        };
        assert!(
            err.message.contains("concrete callee bound plan is empty for its site"),
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
            err.message.contains("concrete callee signature disagrees with its body plan"),
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
            err.message.contains("concrete ABI plan is missing for callee `next`"),
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
        let mid =
            ws.modules.iter().find(|m| m.id != ModuleId::builtin()).expect("test module").id;
        let (emit, cache) = emit_full_in_with_test_mutation(&ws, mid, BodyMutation::None)
            .expect("intact emission");
        let stats = cache.scx.abi_plan_stats();
        let (hits, misses) = (stats.hits(), stats.requests() - stats.hits());
        assert!(misses > 0, "no ABI-plan question consulted the memo (hits={hits})");
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
        let (_, cache) = emit_full_in_with_test_mutation(&ws, mid, BodyMutation::SkipAbiPlanPublish)
            .expect("the bypass must still take the fallback path");
        let stats = cache.scx.abi_plan_stats();
        let witness = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(
                stats.published_hits > 0,
                "the publication witness must fail when the publish step is bypassed: {stats:?}"
            );
        }));
        assert!(witness.is_err(), "the publication witness did not see its bypass: {stats:?}");
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
        assert!(attribution.bodies_examined > 0, "no emitted bodies were examined: {attribution:?}");
        assert!(
            attribution.lower.bodies_with_work > 0
                && attribution.lower.instructions_visited > 0,
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
                && attribution.lower.concrete_mono_interner_miss_cloned_nodes > 0
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
                && attribution.lower.concrete_mono_explicit_owned_escape_cloned_nodes > 0
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
                && attribution.lower.mir_instruction_capacity >= attribution.lower.instructions_visited,
            "lowerer ownership census has no production population: {attribution:?}"
        );
        let consumers = &attribution.lower.concrete_mono_by_consumer;
        assert!(
            consumers[skull_mir::lower::ConcreteMonoConsumer::CallParameter as usize].requests > 0
                && consumers[skull_mir::lower::ConcreteMonoConsumer::CallParameter as usize]
                    .explicit_owned_escapes
                    > 0
                && consumers[skull_mir::lower::ConcreteMonoConsumer::Dispatch as usize].requests
                    > 0,
            "named lowerer consumer routes have no production population: {attribution:?}"
        );
        if std::env::var("ROCK_LOWER_CENSUS").is_ok_and(|value| value == "1") {
            assert!(
                attribution.lower.concrete_mono_timed_requests > 0
                    && attribution.lower.concrete_mono_lookup_ns > 0
                    && attribution.lower.concrete_mono_clone_self_ns > 0,
                "opt-in lowerer timing has no measured population: {attribution:?}"
            );
        }
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
            skull_parser::parse_module_clean(NESTED_SRC, Ustr::from("<nested-lower-test>")).expect("parse"),
        ];
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = ws
            .modules
            .iter()
            .find(|m| m.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let (emit, _) = emit_full_in_with_test_mutation(&ws, mid, BodyMutation::None)
            .expect("nested emission");
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
        assert_eq!(lanes.setup_handshake_entered, 0, "sequential setup used the parallel handshake");
        assert_eq!(lanes.setup_handshake_released, 0, "sequential setup released a handshake");
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
            emit_full_in_with_test_mutation(
                &ws,
                mid,
                BodyMutation::SetupLaneSchedule(schedule),
            )
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
