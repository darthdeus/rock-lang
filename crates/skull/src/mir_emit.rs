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
use skull_mir::lower::EmittedBodyId;
use skull_mir::mir::{FuncRef, Inst, MirFunc, Term};
use skull_parser::CompilerError;
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
    /// Coarse per-stage time for the emit, in run order: HIR->MIR lowering, MIR
    /// cleanup, and Lua emission (regalloc + stringify) — each summed across every
    /// fn/decorator/global (CPU-time under parallel emit, wall-clock under
    /// `--features sequential`) — plus the directly-timed sequential scaffolding /
    /// global-init ordering / deco application. Surfaced by `rock backend-stats`.
    pub phases: Vec<(&'static str, std::time::Duration)>,
}

/// Coarse per-stage wall-clock for one emitted definition. Summed across every
/// cached chunk + decorator/global unit in [`assemble`] to produce the
/// `backend-stats` breakdown.
#[derive(Default, Clone, Copy)]
struct PhaseTimes {
    lower: Duration,
    cleanup: Duration,
    emit: Duration,
    /// Regalloc subset of `emit` (read from the per-thread `REGALLOC_TIME`
    /// accumulator, reset just before this unit's emit so it is exact per-unit
    /// even when units run concurrently on a rayon worker pool).
    regalloc: Duration,
}

impl PhaseTimes {
    fn add(&mut self, o: &PhaseTimes) {
        self.lower += o.lower;
        self.cleanup += o.cleanup;
        self.emit += o.emit;
        self.regalloc += o.regalloc;
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
    if let Some(key) = collect_refs {
        let (gs, cs) = direct_refs(ws, mf);
        out.refs = Some((key, gs, cs));
    }
    out.calls = skull_mir::reach::call_targets(mf);
    out.global_names = skull_mir::reach::global_ref_names(mf);
    out.lua = lua;
    out.times.emit = t_emit;
    out.times.regalloc = skull_codegen_lua::regalloc_time_elapsed().min(t_emit);
}

/// Lower + emit one decorator definition (`deco <name>` -> `__deco_<name>`).
fn emit_decorator_unit(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    mid: ModuleId,
    did: Idx<bone_semantic::hir::DecoratorDef>,
) -> UnitOut {
    let mut out = UnitOut::default();
    let d = &ws.module(mid).decorators[did];
    let deco_name = crate::decorator_fn_name(d.name.as_str());
    let tl = Instant::now();
    let lowered =
        skull_mir::lower::lower_decorator(ws, scx, mid, did, ustr::Ustr::from(deco_name.as_str()));
    out.times.lower = tl.elapsed();
    match lowered {
        Ok(mut mf) => {
            let tc = Instant::now();
            skull_transform::cleanup_with_nobox(ws, &mut mf);
            // Decorator bodies can carry lambdas too -- convert like an ordinary fn.
            let pre_nested = mf.nested_fns.len();
            skull_transform::convert_closures(&mut mf, &deco_name);
            for nf in &mf.nested_fns[pre_nested..] {
                cg.mint_synthetic(nf.name.to_string());
            }
            out.times.cleanup = tc.elapsed();
            let (subst, incoming) = Default::default();
            let owner = bone_semantic::hir::BodyOwner::Decorator(did);
            let ctx = skull_mir::bound::body_ctx(ws, mid, owner, &subst, &incoming);
            if let Err(e) =
                check_unit_spec_bindings(ws, &ctx, mid, &deco_name, &mf, d.span, &mut out.specs)
            {
                out.hard_error = Some(e);
                return out;
            }
            skull_codegen_lua::regalloc_time_reset();
            let te = Instant::now();
            let mut lua = emit_named(ws, cg, &deco_name, &mf);
            emit_pass_nested_fns(ws, cg, &mut mf.nested_fns, &mut lua);
            emit_lowered(ws, &mut out, &mf, lua, Some(Ustr::from(&deco_name)), te.elapsed());
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
    cg: &CodegenCtx,
    g: &bone_semantic::hir::GlobalDef,
) -> String {
    match skull_mir::lower::nobox_global_leaves(ws, g.name, g.ty) {
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
    mid: ModuleId,
    gid: Idx<bone_semantic::hir::GlobalDef>,
) -> UnitOut {
    let mut out = UnitOut::default();
    let g = &ws.module(mid).globals[gid];
    let init_name = crate::global_init_fn_name(g.name.as_str());
    let tl = Instant::now();
    let lowered =
        skull_mir::lower::lower_global(ws, scx, mid, gid, ustr::Ustr::from(init_name.as_str()));
    out.times.lower = tl.elapsed();
    match lowered {
        Ok(Some(mut mf)) => {
            let tc = Instant::now();
            skull_transform::cleanup_with_nobox(ws, &mut mf);
            // A global-initializer body can carry lambdas (e.g. a `[TileDef]`
            // literal with fn fields) -- route them through the shared pass, same
            // as an ordinary fn. Their global reads are NOT init-ordering
            // constraints (the fn value is stored, not called at init), so `refs`
            // is computed on the post-pass `mf` where those reads live in the
            // hoisted siblings, not the main body.
            let pre_nested = mf.nested_fns.len();
            skull_transform::convert_closures(&mut mf, &init_name);
            for nf in &mf.nested_fns[pre_nested..] {
                cg.mint_synthetic(nf.name.to_string());
            }
            out.times.cleanup = tc.elapsed();
            let (subst, incoming) = Default::default();
            let owner = bone_semantic::hir::BodyOwner::Global(gid);
            let ctx = skull_mir::bound::body_ctx(ws, mid, owner, &subst, &incoming);
            if let Err(e) =
                check_unit_spec_bindings(ws, &ctx, mid, g.name.as_str(), &mf, g.name_span, &mut out.specs)
            {
                out.hard_error = Some(e);
                return out;
            }
            let key = Ustr::from(&init_name);
            skull_codegen_lua::regalloc_time_reset();
            let te = Instant::now();
            let mut lua = emit_named(ws, cg, &init_name, &mf);
            emit_pass_nested_fns(ws, cg, &mut mf.nested_fns, &mut lua);
            emit_lowered(ws, &mut out, &mf, lua, Some(key), te.elapsed());
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
            let target = global_init_target(ws, cg, g);
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
        times.add(&u.times);
    }
}

/// The Lua half of [`skull_mir::reach`]. Unlike JS and GLSL this does NOT
/// interleave lowering with discovery: `emit_full` has already lowered every body
/// in parallel, so the call graph is final before the walk starts and `visit` is a
/// cache lookup. (The "reachability cannot be a pre-pass" rule is about LOWERING
/// order; once the chunks exist it no longer applies.) Keeping the parallel
/// fan-out matters -- it is the edit loop's throughput -- and it leaves the chunk
/// key set complete, which the incremental cache depends on.
struct LuaReach<'a> {
    ws: &'a Workspace,
    cg: &'a CodegenCtx,
    scx: &'a skull_mir::lower::ScalarCtx,
    by_name: &'a FxHashMap<Ustr, Vec<FuncRef>>,
    chunks: &'a FxHashMap<EmittedBodyId, Result<Chunk, CompilerError>>,
    /// Specialized chunks this walk built, plus any it pulled from the cache.
    built: FxHashMap<EmittedBodyId, Chunk>,
    /// Specialized bodies that could not emit, by emitted name. Reported by the
    /// renderer in name order, where a specialized rejection has always been
    /// ordered against its siblings.
    errs: Vec<(String, CompilerError)>,
    inline_cache: skull_transform::InlineCache,
    roots: Vec<EmittedBodyId>,
    /// The bound-conformance tree each reached specialized body is emitted
    /// under, from the demand that reached it, with the agreement check
    /// `skull_mir::bound::DemandMap` carries.
    demands: skull_mir::bound::DemandMap,
}

/// Global-name -> fns, keyed by BOTH the emitted (mangled) name and the source
/// name. A fn taken as a VALUE lowers to `Inst::Global` carrying the name the Lua
/// printer emits verbatim -- which is the MANGLED name for an ordinary fn
/// (`double` -> `double_int_ret_int`) and the source name for a `@nomangle` one.
/// Looking up only the source name silently misses every mangled fn value, which
/// is how `let f: fn(int) -> int = double` first emitted a reference to a
/// definition DCE had dropped.
///
/// A source name maps to its whole overload set: the read carries no signature, so
/// this is conservative (it can keep a sibling the body never takes) but never
/// drops a live one.
fn build_name_index(ws: &Workspace) -> FxHashMap<Ustr, Vec<FuncRef>> {
    let mut idx: FxHashMap<Ustr, Vec<FuncRef>> = FxHashMap::default();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            let fref = FuncRef { module: module.id, func: fid };
            idx.entry(f.name).or_default().push(fref);
            let mangled = Ustr::from(&mangle_fn(ws, f));
            if mangled != f.name {
                idx.entry(mangled).or_default().push(fref);
            }
        }
    }
    idx
}

fn fns_named<'a>(
    by_name: &'a FxHashMap<Ustr, Vec<FuncRef>>,
    name: Ustr,
) -> impl Iterator<Item = FuncRef> + 'a {
    by_name.get(&name).map(|v| v.as_slice()).unwrap_or(&[]).iter().copied()
}

impl skull_mir::reach::ReachPolicy for LuaReach<'_> {
    type Node = EmittedBodyId;

    fn roots(&mut self) -> Result<Vec<EmittedBodyId>, CompilerError> {
        Ok(std::mem::take(&mut self.roots))
    }

    /// A body with no chunk (extern, `inline(lua)`, `Iterator`-bounded) has no
    /// MIR to walk. It is still EMITTED if reached -- `is_leaf` only means
    /// "expand no further", and those bodies reach onward only by bare name,
    /// which the `@nomangle` root rule covers.
    ///
    /// A specialized body is never a leaf: [`Self::chunk_of`] builds one on
    /// demand, so "no chunk yet" is not the same answer as "no body to walk".
    fn is_leaf(&mut self, body: EmittedBodyId) -> Result<bool, CompilerError> {
        Ok(match &body {
            EmittedBodyId::Shared(fref) => {
                let leaf = !matches!(self.chunks.get(&shared(*fref)), Some(Ok(_)));
                if leaf {
                    // A legitimate leaf is OPAQUE -- extern / `inline(lua)` --
                    // and is still emitted, reaching onward by bare name. A
                    // GENERIC Rock declaration is not: E1 left it with no body
                    // at all, so arriving at its shared id means a call site
                    // spelled the declaration's own symbol and nothing defines
                    // it. Lua would emit that call and die at runtime on a nil
                    // global with no diagnostic, which is how a witness that
                    // recorded identity without DELIVERING its instantiation
                    // stayed invisible for 59 Rock `@test`s. JS rejects the same
                    // condition; this is the missing half.
                    let f = &self.ws.module(fref.module).functions[fref.func];
                    if is_generic(f) && !f.is_extern && f.inline_body.is_none() {
                        let name = mangle_fn(self.ws, f);
                        self.errs.push((
                            name.clone(),
                            CompilerError::new_with_label(
                                f.name_span,
                                format!(
                                    "`{name}` is reached at its DECLARATION, and a generic \
                                     declaration has no emitted body -- every call must name a \
                                     concrete instantiation. Two things land here. Either a \
                                     type argument is UNCONSTRAINED at the call (nothing pins \
                                     it, e.g. a generic that appears only in the return type), \
                                     which is a program error; or a call site recorded which \
                                     instance it means without DELIVERING it, which is a \
                                     compiler one. Check whether the call's type arguments are \
                                     inferable first."
                                ),
                            ),
                        ));
                    }
                }
                leaf
            }
            EmittedBodyId::Specialized(..) => false,
        })
    }

    /// Recursion is ordinary Rock.
    fn on_cycle(&mut self, _body: EmittedBodyId) -> Result<(), CompilerError> {
        Ok(())
    }

    fn visit(&mut self, body: EmittedBodyId) -> Result<Vec<EmittedBodyId>, CompilerError> {
        let (calls, global_names, specs) = match self.chunk_of(&body) {
            Some(chunk) => (chunk.calls.clone(), chunk.global_names.clone(), chunk.specs.clone()),
            None => return Ok(Vec::new()),
        };
        // BEFORE the successors are returned: a specialized successor is BUILT
        // when it is visited, and its tree has to be on the map by then.
        for body in self.demands.record(&specs) {
            self.errs.push(demand_clash(self.ws, &body));
        }
        let mut succs: Vec<EmittedBodyId> = calls.into_iter().map(shared).collect();
        for name in global_names {
            succs.extend(fns_named(self.by_name, name).map(shared));
        }
        // The edge a declaration-keyed walk could not carry: a specialized body
        // is lowered under its instantiation, so it reaches callees, globals and
        // FURTHER specializations no shared body mentions.
        //
        // It is also the edge that can fail to terminate. Polymorphic recursion
        // demands a strictly deeper instantiation at every step, so each node is
        // DISTINCT and the walk's own dedup can never close the loop -- before
        // E1 the shared generic body absorbed the recursion, and removing it is
        // what turned a compiling program into a hanging compiler. Bounded by
        // the same `MAX_INSTANTIATION_DEPTH` over the same `MonoTy::depth` the
        // instance graph uses, so the two agree on what does not terminate.
        //
        // Emission REJECTS where the graph merely declines to expand: the call
        // site already spells the specialized name, so skipping the body would
        // ship a call to a definition nothing writes.
        for d in specs.iter() {
            if let Some((name, err)) = too_deep_to_instantiate(self.ws, d) {
                self.errs.push((name, err));
                continue;
            }
            succs.push(EmittedBodyId::new(d.func, d.key.clone()));
        }
        Ok(succs)
    }
}

impl<'a> LuaReach<'a> {
    /// This body's chunk, BUILDING a specialized one that has none yet.
    ///
    /// The chicken-and-egg the previous two mechanisms avoided by cutting the
    /// arrow: a specialized body's edges exist only once it is lowered, and it
    /// was lowered only after DCE had already decided what to emit. One walk
    /// cannot defer that, so it builds -- and the build is not wasted, because
    /// the renderer downstream pulls the same chunk.
    ///
    /// A body that cannot emit contributes NO edges and is not recorded; the
    /// renderer rebuilds it and reports the rejection there, which is the one
    /// place a specialized rejection is ordered against its siblings.
    fn chunk_of(&mut self, body: &EmittedBodyId) -> Option<&Chunk> {
        if !self.built.contains_key(body) {
            match body {
                EmittedBodyId::Shared(fref) => {
                    return match self.chunks.get(&shared(*fref)) {
                        Some(Ok(chunk)) => Some(chunk),
                        _ => None,
                    };
                }
                EmittedBodyId::Specialized(fref, key) => {
                    if let Some(Ok(chunk)) = self.chunks.get(body) {
                        self.built.insert(body.clone(), chunk.clone());
                    } else {
                        let f = &self.ws.module(fref.module).functions[fref.func];
                        let name = skull_mir::mangle::spec_fn_name(self.ws, f, key)?;
                        let mut specs = Vec::new();
                        let tree = self.demands.tree_of(body);
                        let built = emit_spec_chunk(
                            self.ws,
                            self.cg,
                            self.scx,
                            body,
                            &tree,
                            &name,
                            &mut self.inline_cache,
                            &mut specs,
                        );
                        match built {
                            Ok(chunk) => {
                                self.built.insert(body.clone(), chunk);
                            }
                            Err(e) => {
                                self.errs.push((name, e));
                                return None;
                            }
                        }
                    }
                }
            }
        }
        self.built.get(body)
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

/// The fns whose definitions must appear in the emitted chunk: the roots plus
/// everything they transitively call. `extra_roots` carries the call edges of the
/// definitions that are always emitted regardless (decorator bodies, global
/// initializers) and of the shader registrations.
/// **Post-inline liveness over EMITTED BODIES** -- the one answer to "which
/// definitions does this program need", for the shared body of a declaration and
/// for a specialized instantiation alike.
///
/// There were two mechanisms. A `FuncRef` DCE walk decided shared bodies, and a
/// demand closure seeded from the EMITTED chunks decided specialized ones, with
/// the arrow between them cut: a declaration reached only by a specialized
/// body's residual call was invisible to the walk. Measured before unifying, that
/// arrow adds nothing on this corpus (0 such declarations over 8 projects), which
/// is why one walk that carries it is byte-identical rather than a fix.
///
/// The walk BUILDS the specialized chunks it needs, so the answer is a fixpoint
/// rather than one pass with a missing edge. Those chunks are handed back: the
/// renderer downstream pulls them instead of lowering the same bodies again.
struct LiveBodies {
    /// Every emitted body a root reaches, both kinds.
    bodies: FxHashSet<EmittedBodyId>,
    /// Specialized chunks the walk built, for the renderer and the cache.
    spec_chunks: FxHashMap<EmittedBodyId, Chunk>,
    /// Specialized bodies that could not emit, by emitted name.
    rejected: Vec<(String, CompilerError)>,
}

/// The diagnostic for a demand clash, at the declaration whose specialization
/// two sites disagree about.
fn demand_clash(ws: &Workspace, body: &EmittedBodyId) -> (String, CompilerError) {
    let fref = body.decl();
    let f = &ws.module(fref.module).functions[fref.func];
    let name = body
        .key()
        .and_then(|k| skull_mir::mangle::spec_fn_name(ws, f, k))
        .unwrap_or_else(|| f.name.to_string());
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

impl LiveBodies {
    /// Whether this DECLARATION's own definition is emitted. The
    /// validation-vs-emitted split rests on this being the ONLY thing liveness
    /// gates: every declaration is still visited, reported on and rejected from.
    fn emits(&self, fref: FuncRef) -> bool {
        self.bodies.contains(&EmittedBodyId::Shared(fref))
    }

    /// The declarations whose definitions are emitted -- what the overlay's
    /// `emitted` baseline records.
    fn emitted_bodies(&self) -> FxHashSet<EmittedBodyId> {
        self.bodies.clone()
    }
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
    d: &skull_mir::bound::SpecDemand,
) -> Option<(String, CompilerError)> {
    let at = skull_mir::monoty::too_deep_to_instantiate(d.key.iter())?.render();
    let f = &ws.module(d.func.module).functions[d.func.func];
    let name = skull_mir::mangle::spec_fn_name(ws, f, &d.key).unwrap_or_else(|| f.name.to_string());
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

fn live_bodies(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    cache: &SkullEmitCache,
    deco_map: &DecoMap,
    project_root: Option<&str>,
    extra_roots: &[FuncRef],
    extra_root_names: &[Ustr],
    unit_specs: &[skull_mir::bound::SpecDemand],
) -> Result<LiveBodies, CompilerError> {
    let by_name = build_name_index(ws);
    let mut roots: Vec<FuncRef> = Vec::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            // A GENERIC declaration is never a root, however the root rule reads:
            // it has no body to emit, so rooting it would ask for a shared
            // generic snapshot that no longer exists. Its instances are reached
            // through the demands of the bodies that call them, and a generic
            // nothing instantiates emits nothing.
            if is_generic(f) {
                continue;
            }
            if is_emission_root(module.file.as_str(), project_root, f, deco_map) {
                roots.push(FuncRef { module: module.id, func: fid });
            }
        }
    }
    roots.extend(extra_roots.iter().copied());
    roots.extend(cache.shader_registrations.dependencies.iter().copied());
    // `sim_registration_lines` calls `register_sim` from generated text.
    if !collect_sim_fns(ws).is_empty() {
        roots.extend(register_sim_fn(ws));
    }
    for &name in extra_root_names {
        roots.extend(fns_named(&by_name, name));
    }

    // The always-emitted units (decorator bodies, global initializers) are not
    // reached through any chunk -- they emit unconditionally -- so the
    // instantiations they ask for are roots in their own right.
    let mut root_bodies: Vec<EmittedBodyId> = roots.iter().copied().map(shared).collect();
    root_bodies
        .extend(unit_specs.iter().map(|d| EmittedBodyId::new(d.func, d.key.clone())));

    let mut policy = LuaReach {
        ws,
        cg,
        scx,
        by_name: &by_name,
        chunks: &cache.chunks,
        built: FxHashMap::default(),
        errs: Vec::new(),
        inline_cache: skull_transform::InlineCache::default(),
        roots: root_bodies.clone(),
        demands: skull_mir::bound::DemandMap::default(),
    };
    for body in policy.demands.record(unit_specs) {
        policy.errs.push(demand_clash(ws, &body));
    }
    let order = skull_mir::reach::walk(&mut policy)?;

    // `walk` returns only EXPANDED nodes. A root and a LEAF (extern, inline(lua),
    // `Iterator`-bounded -- anything with no chunk) are reached and emittable
    // too, so fold the roots and every expanded body's edges back in.
    let mut bodies: FxHashSet<EmittedBodyId> = order.iter().cloned().collect();
    bodies.extend(root_bodies);
    for body in &order {
        let chunk = match body {
            EmittedBodyId::Shared(fref) => match cache.chunks.get(&shared(*fref)) {
                Some(Ok(chunk)) => chunk,
                _ => continue,
            },
            EmittedBodyId::Specialized(..) => match policy.built.get(body) {
                Some(chunk) => chunk,
                None => continue,
            },
        };
        bodies.extend(chunk.calls.iter().copied().map(shared));
        for &name in &chunk.global_names {
            bodies.extend(fns_named(&by_name, name).map(shared));
        }
        bodies.extend(chunk.specs.iter().map(|d| EmittedBodyId::new(d.func, d.key.clone())));
    }
    Ok(LiveBodies { bodies, spec_chunks: policy.built, rejected: policy.errs })
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
    /// `static` cells this body declares -- aggregated at chunk-pull for the
    /// hot-reload preservation preamble + `__rock_known_globals` registration.
    statics: Vec<skull_mir::mir::StaticCell>,
    /// The callees this body still CALLS after inlining -- the reachability edge
    /// set. Distinct from `deps`, which is an invalidation superset (it also holds
    /// everything inlined AWAY, and would keep spliced-out callees alive).
    calls: Vec<FuncRef>,
    /// Module-global names this body reads/writes. A top-level fn used as a VALUE
    /// lowers to an `Inst::Global`, not a call, so these are reachability edges
    /// too -- without them a fn passed as an argument gets pruned out from under
    /// a live reference.
    global_names: Vec<Ustr>,
    /// Callees this body invokes through a SPECIALIZED instantiation that no
    /// splice served, so each needs a standalone specialized copy emitted.
    /// Deduplicated program-wide by [`assemble`] -- many bodies ask for the same
    /// one, and it must be defined exactly once.
    specs: Vec<skull_mir::bound::SpecDemand>,
}

/// The shared body of `fref` -- the id under which a declaration's own
/// definition is cached. Written out at every chunk lookup so the day a
/// SPECIALIZED body answers one of them is a visible change rather than a
/// silent widening.
fn shared(fref: FuncRef) -> EmittedBodyId {
    EmittedBodyId::Shared(fref)
}

/// Deterministic overlay order over emitted bodies. Reduces to the declaration
/// order the overlay used before specializations could appear in it: a
/// declaration's shared body sorts ahead of its own specializations, and those
/// sort by their instantiation's canonical spelling, which is injective.
fn overlay_order_key(body: &EmittedBodyId) -> (usize, u32, Option<String>) {
    let fref = body.decl();
    let key = body
        .key()
        .map(|k| k.iter().map(skull_mir::monoty::MonoTy::canonical).collect::<Vec<_>>().join(","));
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
    body: &EmittedBodyId,
    f: &bone_semantic::hir::Function,
    inline_cache: &mut skull_transform::InlineCache,
) -> Result<Chunk, CompilerError> {
    match body {
        EmittedBodyId::Shared(fref) => emit_body_chunk(ws, cg, scx, *fref, f, inline_cache),
        EmittedBodyId::Specialized(fref, key) => {
            // A rebuild that cannot emit DROPS the entry rather than storing the
            // rejection: assembly rebuilds a miss and reports the error from
            // there, which is the one place a specialized rejection is ordered
            // against its siblings.
            let Some(name) = skull_mir::mangle::spec_fn_name(ws, f, key) else {
                return Err(CompilerError::new_with_label(
                    f.name_span,
                    format!("internal: `{}` has a specialized chunk under an all-`None` key", f.name),
                ));
            };
            let _ = fref;
            let mut specs = Vec::new();
            // The EMPTY tree, and that is not a fallback. A body with placeholders
            // belongs to an `Iterator`-bounded declaration, and `is_cached_body`
            // is false for one -- its caller DROPS the chunk instead of rebuilding
            // it, so the liveness walk re-derives it WITH the demand's tree. If a
            // body with placeholders ever did arrive here,
            // `resolve_standalone_bound_calls` refuses it rather than emitting a
            // definition that names no `next`.
            emit_spec_chunk(ws, cg, scx, body, &Default::default(), &name, inline_cache, &mut specs)
        }
    }
}

/// Per-body emit cache for incremental codegen: the expensive function chunks
/// (ordinary + coroutine bodies), keyed by [`EmittedBodyId`]. `Err` records a body the
/// new path could not lower (the parity gap). Everything else is regenerated by
/// [`assemble`] every time. See `docs/skull-incremental-emit-plan.md`.
pub struct SkullEmitCache {
    chunks: FxHashMap<EmittedBodyId, Result<Chunk, CompilerError>>,
    /// Functions changed since the last full build (`emit_full`), accumulated by
    /// `emit_overlay` so the `incremental.lua` overlay always carries the latest
    /// version of every touched fn (not just the most recent edit). Reset to
    /// empty by `emit_full`. See `docs/skull-incremental-emit-split.md`.
    changed_since_full: FxHashSet<EmittedBodyId>,
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
    dependents: FxHashMap<FuncRef, FxHashSet<EmittedBodyId>>,
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
    /// The definitions the last full assembly actually EMITTED (global DCE prunes
    /// the rest), keyed by EMITTED BODY. `assemble_overlay` needs it: the overlay
    /// redefines only CHANGED fns, so a body edit that newly reaches a
    /// previously-pruned definition would otherwise leave it defined nowhere -- a
    /// runtime nil call on reload.
    ///
    /// By emitted body rather than by declaration, because a specialization is
    /// its own definition with its own symbol. Keyed by `FuncRef`, the backfill
    /// walk could only ever answer for shared bodies.
    emitted: FxHashSet<EmittedBodyId>,
    /// The compilation target's root directory; see [`is_library_module`].
    project_root: Option<String>,
    renderer_target: RendererTarget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererTarget {
    Gl,
    Wgpu,
}

#[derive(Default)]
struct ShaderRegistrations {
    lua: String,
    dependencies: FxHashSet<FuncRef>,
    callsites: FxHashSet<(ModuleId, BodyOwner)>,
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
/// [`prepare_instance_body`] -- there is no inline pass, no `convert_closures`
/// (the step's lambdas convert framed, inside `emit_coroutine`) and no FORL
/// recovery (codegen's relooper does it).
///
/// `body` decides every symbol, through [`skull_mir::lower::plan_coroutine`].
fn emit_coroutine_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    ctx: &skull_mir::bound::BoundCtx,
    body: &EmittedBodyId,
    f: &bone_semantic::hir::Function,
    mf: &mut MirFunc,
    mut times: PhaseTimes,
) -> Result<Chunk, CompilerError> {
    let tc = Instant::now();
    skull_transform::cleanup_with_nobox(ws, mf);
    times.cleanup = tc.elapsed();
    // Coroutines never inline, so a combinator chain here leaves a residual call
    // -- fine when the lowerer specialized it (a standalone body serves it), a
    // rejection when it did not.
    skull_transform::validate_no_residual_bound_calls(ws, f, mf)?;
    let mut specs = Vec::new();
    validate_emitted_body(ws, ctx, mf, &mut specs).map_err(|r| r.into_error(f.name_span, ""))?;
    skull_codegen_lua::regalloc_time_reset();
    let te = Instant::now();
    let plan = skull_mir::lower::plan_coroutine(ws, body);
    let lua = crate::coroutine_emit::emit_coroutine(ws, cg, &plan, mf);
    times.emit = te.elapsed();
    times.regalloc = skull_codegen_lua::regalloc_time_elapsed().min(times.emit);
    // A specialization's chunk names the DECLARATION it specializes, so editing a
    // generic `co fn` re-emits every instantiation of it; the shared body has no
    // such edge to record and keeps its empty dep set.
    let mut deps = FxHashSet::default();
    if body.key().is_some() {
        deps.insert(body.decl());
    }
    Ok(Chunk {
        lua,
        deps,
        direct_globals: Vec::new(),
        direct_calls: Vec::new(),
        times,
        statics: mf.statics.clone(),
        calls: skull_mir::reach::call_targets(mf),
        global_names: skull_mir::reach::global_ref_names(mf),
        specs,
    })
}

/// Emit one ordinary or coroutine function body to a cacheable [`Chunk`]. Extern
/// and `inline(lua)` functions are NOT handled here — they are cheap and
/// regenerated in [`assemble`]. Captures per-stage emit time into the chunk so the
/// parallel chunk fan-out's CPU time still rolls up into `backend-stats`.
fn emit_body_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    self_ref: FuncRef,
    f: &bone_semantic::hir::Function,
    inline_cache: &mut skull_transform::InlineCache,
) -> Result<Chunk, CompilerError> {
    let mut times = PhaseTimes::default();
    let tl = Instant::now();
    let mut mf = skull_mir::lower::lower_function(ws, scx, self_ref.module, self_ref.func)?;
    times.lower = tl.elapsed();
    if f.is_coroutine {
        let (subst, incoming) = Default::default();
        let owner = BodyOwner::Function(self_ref.func);
        let ctx = skull_mir::bound::body_ctx(ws, self_ref.module, owner, &subst, &incoming);
        return emit_coroutine_chunk(ws, cg, &ctx, &shared(self_ref), f, &mut mf, times);
    }
    // Inline eligible direct calls before cleanup so a hot loop traces with no
    // per-call frame (`docs/skull-inlining-plan.md`).
    let (spliced, _splices) =
        skull_transform::inline_functions(ws, scx, self_ref, &mut mf, inline_cache)?;
    let mut deps: FxHashSet<FuncRef> = spliced.into_iter().collect();
    let tc = Instant::now();
    skull_transform::cleanup_with_nobox(ws, &mut mf); // drop dead values + dead-cell stores
    // Shared closure conversion (S2 cutover, docs/closure-conversion-mir-plan.md):
    // every `Inst::Lambda` in this ORDINARY fn becomes a `MakeEnv` record / bare
    // `Global` + a hoisted sibling in `nested_fns`. Runs AFTER inline+cleanup,
    // BEFORE FORL recovery (matching the JS pipeline) so a loop lifted into a
    // lambda body is FORL-recovered in the split fn. Coroutines skip this (their
    // `Inst::Lambda`s convert at codegen via `FrameCtx`/`HoistPlan` -- S3).
    let name = mangle_fn(ws, f);
    let pre_nested = mf.nested_fns.len();
    skull_transform::convert_closures(&mut mf, &name);
    // Route each pass-hoisted `<name>__lambda<k>` through the synthetic-name
    // collision guard now that they enter the Lua global namespace (a user fn
    // named `foo__lambda0` must reject, not silently clobber). The pass appends
    // its lambda bodies after any named nested `fn`s, so guard only the new tail.
    for nf in &mf.nested_fns[pre_nested..] {
        cg.mint_synthetic(nf.name.to_string());
    }
    skull_transform::recover_counted_loops(ws, &mut mf); // desugared iter loops -> FORL shape
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
    inline_hoisted_bodies(ws, scx, self_ref, &mut mf.nested_fns, inline_cache, &mut deps)
        .map_err(|msg| CompilerError::new_with_label(f.name_span, msg))?;
    times.cleanup = tc.elapsed();
    skull_transform::validate_no_residual_bound_calls(ws, f, &mf)?;
    let mut specs = Vec::new();
    let (subst, incoming) = Default::default();
    let owner = BodyOwner::Function(self_ref.func);
    let ctx = skull_mir::bound::body_ctx(ws, self_ref.module, owner, &subst, &incoming);
    validate_emitted_body(ws, &ctx, &mf, &mut specs).map_err(|r| r.into_error(f.name_span, ""))?;
    collect_callee_frefs(&mf, &mut deps);
    let (direct_globals, direct_calls) = direct_refs(ws, &mf);
    skull_codegen_lua::regalloc_time_reset();
    let te = Instant::now();
    let mut lua = emit_function(ws, cg, self_ref, &mf);
    // Nested `fn`s are hoisted to sibling top-level Lua fns in the same chunk
    // (their `__local_*` names are already final -- emit_named, no mangling). Their
    // `static` cells fold into THIS chunk's `statics` so the hot-reload preamble +
    // overlay reset see them -- a nested-fn static is otherwise invisible to both.
    let mut statics = mf.statics.clone();
    emit_nested_fns(ws, cg, &mut mf.nested_fns, &mut deps, &mut statics, &mut lua);
    times.emit = te.elapsed();
    times.regalloc = skull_codegen_lua::regalloc_time_elapsed().min(times.emit);
    let calls = skull_mir::reach::call_targets(&mf);
    let global_names = skull_mir::reach::global_ref_names(&mf);
    Ok(Chunk { lua, deps, direct_globals, direct_calls, times, statics, calls, global_names, specs })
}

/// Emit each hoisted nested `fn` (and its own nested fns, depth-first) as a
/// top-level Lua function appended to the enclosing chunk. Runs the standard
/// cleanup pipeline per fn, folds their callee refs into the chunk's deps (so an
/// edit to a callee still invalidates the enclosing chunk) and their `static`
/// cells into the chunk's `statics` (so hot-reload can reset them).
fn emit_nested_fns(
    ws: &Workspace,
    cg: &CodegenCtx,
    nested: &mut [MirFunc],
    deps: &mut FxHashSet<FuncRef>,
    statics: &mut Vec<skull_mir::mir::StaticCell>,
    out: &mut String,
) {
    for nf in nested.iter_mut() {
        skull_transform::cleanup_with_nobox(ws, nf);
        skull_transform::recover_counted_loops(ws, nf);
        collect_callee_frefs(nf, deps);
        statics.extend(nf.statics.iter().cloned());
        out.push_str(&emit_named(ws, cg, nf.name.as_str(), nf));
        let mut children = std::mem::take(&mut nf.nested_fns);
        emit_nested_fns(ws, cg, &mut children, deps, statics, out);
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

fn emit_pass_nested_fns(ws: &Workspace, cg: &CodegenCtx, nested: &mut [MirFunc], out: &mut String) {
    for nf in nested.iter_mut() {
        skull_transform::cleanup_with_nobox(ws, nf);
        out.push_str(&emit_named(ws, cg, nf.name.as_str(), nf));
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
pub fn is_cached_emitted_body(f: &bone_semantic::hir::Function, body: &EmittedBodyId) -> bool {
    match body {
        EmittedBodyId::Specialized(..) => !f.is_extern && f.inline_body.is_none(),
        EmittedBodyId::Shared(_) => is_cached_body(f),
    }
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
/// `__vec_data_set` at `T = Vec2`). `done` keys on the full `(FuncRef, SpecKey)`
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
    body: &EmittedBodyId,
    tree: &skull_mir::bound::ResolvedBoundTree,
    name: &str,
    inline_cache: &mut skull_transform::InlineCache,
    specs: &mut Vec<skull_mir::bound::SpecDemand>,
) -> Result<Chunk, CompilerError> {
    let fref = body.decl();
    let f = &ws.module(fref.module).functions[fref.func];
    let subst = body.subst(ws);
    let mut mf = skull_mir::lower::lower_emitted_body(ws, scx, body)
        .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?;
    // BEFORE every transform: an `Iterator`-bounded body's placeholders are what
    // make it unemittable on its own, and the demand that reached here is what
    // resolves them. The splice path does the same substitution at the moment it
    // clones the body into a caller.
    skull_transform::bound::resolve_standalone_bound_calls(ws, &mut mf, tree)
        .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?;
    let owner = BodyOwner::Function(fref.func);
    let ctx = skull_mir::bound::body_ctx(ws, fref.module, owner, &subst, tree);
    if f.is_coroutine {
        return emit_coroutine_chunk(ws, cg, &ctx, body, f, &mut mf, PhaseTimes::default());
    }
    let mut deps = prepare_instance_body(ws, scx, fref, &subst, name, &mut mf, inline_cache)
        .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?;
    let before = specs.len();
    // Discovered BEFORE the verdict, and handed back through `specs` even when
    // the verdict rejects: the inline worklist pushed them into the live queue
    // first, so a failing build reports over that same closure.
    validate_emitted_body(ws, &ctx, &mf, specs)
        .map_err(|r| r.into_error(f.name_span, &format!("cannot emit specialized `{name}`: ")))?;
    let specs = specs[before..].to_vec();
    let mut lua = emit_named(ws, cg, name, &mf);
    let mut statics = mf.statics.clone();
    let mut nested = std::mem::take(&mut mf.nested_fns);
    collect_nested_statics(&nested, &mut statics);
    emit_pass_nested_fns(ws, cg, &mut nested, &mut lua);
    deps.insert(fref);
    collect_callee_frefs(&mf, &mut deps);
    Ok(Chunk {
        lua,
        deps,
        direct_globals: Vec::new(),
        direct_calls: Vec::new(),
        times: PhaseTimes::default(),
        statics,
        calls: skull_mir::reach::call_targets(&mf),
        global_names: skull_mir::reach::global_ref_names(&mf),
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
    pending: &mut Vec<skull_mir::bound::SpecDemand>,
    cached: &FxHashMap<EmittedBodyId, Result<Chunk, CompilerError>>,
    live: &FxHashMap<EmittedBodyId, Chunk>,
    rejected: &[(String, CompilerError)],
    built: &mut Vec<(EmittedBodyId, Chunk)>,
    statics: &mut Vec<skull_mir::mir::StaticCell>,
    out: &mut String,
) -> Vec<CompilerError> {
    let mut done: FxHashSet<EmittedBodyId> = FxHashSet::default();
    let mut errs: Vec<(String, CompilerError)> = rejected.to_vec();
    let rejected_names: FxHashSet<&str> =
        rejected.iter().map(|(n, _)| n.as_str()).collect();
    while let Some(demand) = pending.pop() {
        let (fref, key) = (demand.func, demand.key);
        let body = EmittedBodyId::new(fref, key.clone());
        if !done.insert(body.clone()) {
            continue;
        }
        let f = &ws.module(fref.module).functions[fref.func];
        let Some(name) = skull_mir::mangle::spec_fn_name(ws, f, &key) else {
            continue; // all-`None` key: the shared body already serves it
        };
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
pub fn prepare_instance_body(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    fref: FuncRef,
    subst: &skull_mir::lower::SpecSubst,
    name: &str,
    mf: &mut skull_mir::mir::MirFunc,
    cache: &mut skull_transform::InlineCache,
) -> Result<FxHashSet<FuncRef>, CompilerError> {
    let (spliced, _records) =
        skull_transform::inline_functions_with_subst(ws, scx, fref, mf, cache, subst)?;
    skull_transform::cleanup_with_nobox(ws, mf);
    skull_transform::convert_closures(mf, name);
    skull_transform::recover_counted_loops(ws, mf);
    Ok(spliced.into_iter().collect())
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
    deps: &mut FxHashSet<FuncRef>,
) -> Result<(), String> {
    for nf in nested.iter_mut() {
        let (spliced, _) = skull_transform::inline_functions(ws, scx, self_ref, nf, inline_cache)
            .map_err(|e| e.message)?;
        deps.extend(spliced);
        skull_transform::cleanup_with_nobox(ws, nf);
        let mut children = std::mem::take(&mut nf.nested_fns);
        inline_hoisted_bodies(ws, scx, self_ref, &mut children, inline_cache, deps)?;
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
    if let Err(e) = skull_mir::reach::spec_bound_calls(ws, ctx, mf, specs) {
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

/// `__rock_shader_register("<key>", <glsl>)` for every distinct `rock_shader`
/// pair (docs/shaders.md). Emitted at module-init time after
/// the `@sim` registrations, at chunk top level, so a reload re-`dofile`s them
/// with the new source (the hot-reload-parity invariant). Deterministic
/// sorted-by-key order (from `collect_shader_pairs`) keeps re-emission
/// byte-identical. GLSL emission lives HERE, in the driver -- skull-mir surfaces
/// only the resolved `FuncRef`s (the layering constraint). An emit error (tier
/// violation, stage/collision) is a spanned `CompilerError` failing the compile,
/// never skip-and-continue.
const WGSL_FULLSCREEN_ROOTS: [&str; 3] = ["blit_frag", "present_frag", "tonemap_frag"];

fn shader_registration_lines(
    ws: &Workspace,
    renderer_target: RendererTarget,
) -> Result<ShaderRegistrations, CompilerError> {
    // Always Core330. The profile is a property of the GL context, which a build
    // cannot know: the engine rewrites #version + precision for GLES at
    // shader-compile time (stone-gl `adapt_source_for_gles`), and that header is
    // the ONLY difference between the two profiles.
    let profile = skull_codegen_glsl::GlslProfile::Core330;
    let collection = skull_mir::lower::collect_shader_collection(ws)?;
    let mut out = String::new();
    let mut dependencies = FxHashSet::default();
    for pair in &collection.pairs {
        let fname = ws.module(pair.frag.module).functions[pair.frag.func].name;
        // vert=None is the A3c single-arg fullscreen form -> the A1 single-entry
        // emitter (PIXEL body + fullscreen VERTEX passthrough).
        let emission = match pair.vert {
            Some(vert) => {
                let vname = ws.module(vert.module).functions[vert.func].name;
                skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
                    ws,
                    vert.module,
                    vname.as_str(),
                    fname.as_str(),
                    profile,
                )?
            }
            None => skull_codegen_glsl::emit_shader_entry_with_dependencies(
                ws,
                pair.frag.module,
                fname.as_str(),
                profile,
            )?,
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
    if renderer_target == RendererTarget::Wgpu {
        let mut admitted = 0usize;
        for root in WGSL_FULLSCREEN_ROOTS {
            let Some(pair) = collection.pairs.iter().find(|pair| pair.key == root) else {
                return Err(CompilerError::new_no_location(format!(
                    "selected wgpu fullscreen root `{root}` was not admitted by rock_shader"
                )));
            };
            if let Some(vert) = pair.vert {
                let vname = ws.module(vert.module).functions[vert.func].name;
                return Err(CompilerError::new_no_location(format!(
                    "selected wgpu fullscreen root `{root}` has explicit vertex stage `{vname}`; \
                     only compiler-owned fullscreen geometry is supported"
                )));
            }
            let neutral = skull_codegen_wgsl::plan_fullscreen_artifact(ws, pair.frag)?;
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
            dependencies.extend(emission.dependencies.iter().copied());
            let wire = emission.abi.wire();
            let semantic_wire = neutral.interface.semantic_wire(true);
            let _ = writeln!(
                out,
                "__rock_wgpu_shader_register({}, {}, {}, {})",
                skull_codegen_lua::lua_string(root),
                skull_codegen_lua::lua_string(&emission.source),
                skull_codegen_lua::lua_string(&wire),
                skull_codegen_lua::lua_string(&semantic_wire),
            );
            admitted += 1;
        }
        if admitted != WGSL_FULLSCREEN_ROOTS.len() {
            return Err(CompilerError::new_no_location(format!(
                "selected wgpu fullscreen registration admitted {admitted} roots, expected {}",
                WGSL_FULLSCREEN_ROOTS.len()
            )));
        }
    }
    Ok(ShaderRegistrations {
        lua: out,
        dependencies,
        callsites: collection.callsites.into_iter().collect(),
    })
}

fn assemble(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    cache: &SkullEmitCache,
    built_specs: &mut Vec<(EmittedBodyId, Chunk)>,
) -> Result<(MirEmit, FxHashSet<EmittedBodyId>), CompilerError> {
    // Per-stage lower/cleanup/emit/regalloc times: chunk times (captured during the
    // parallel `emit_full`/`emit_incremental` chunk build) summed in as chunks are
    // pulled, plus the decorator/global units re-emitted here. Under parallel emit
    // these are summed CPU time across workers (so they exceed the wall-clock emit
    // total `backend-stats` measures). `t_scaffold` is the directly-timed
    // SEQUENTIAL remainder (prelude/value-structs/type-tables + global-init
    // ordering + deco application).
    let mut times = PhaseTimes::default();
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
        emit_decorator_unit(ws, scx, cg, mid, did)
    });
    rejections.extend(unit_errors(&deco_results));
    needed_specs.extend(deco_results.iter().flat_map(|u| u.specs.iter().cloned()));
    let mut always_emitted_calls: Vec<FuncRef> = Vec::new();
    let mut always_emitted_names: Vec<Ustr> = Vec::new();
    merge_units(deco_results, &mut out, &mut skipped, &mut direct_globals, &mut direct_calls, &mut Vec::new(), &mut always_emitted_calls, &mut always_emitted_names, &mut times);

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
        emit_global_unit(ws, scx, cg, mid, gid)
    });
    rejections.extend(unit_errors(&global_results));
    needed_specs.extend(global_results.iter().flat_map(|u| u.specs.iter().cloned()));
    let mut globals_lua = String::new();
    // Held back and appended after the fn-def skips so the reported order is
    // unchanged by the hoist.
    let mut global_skipped: Vec<(String, String)> = Vec::new();
    merge_units(global_results, &mut globals_lua, &mut global_skipped, &mut direct_globals, &mut direct_calls, &mut pending_inits, &mut always_emitted_calls, &mut always_emitted_names, &mut times);

    // Whole-program DCE: emit a fn definition only if a root reaches it. ONE
    // liveness answer, for shared and specialized bodies alike.
    let live = live_bodies(
        ws, cg, scx, cache, &deco_map, cache.project_root.as_deref(), &always_emitted_calls,
        &always_emitted_names, &needed_specs,
    )?;

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
            // Whole-program DCE. Only the DEFINITION is gated: the bookkeeping
            // below (global-init ordering facts, skip reporting, the hard error on
            // an unlowerable user fn) runs for every fn exactly as before, so
            // pruning can change which definitions are emitted and nothing else --
            // in particular it never hides a diagnostic.
            let emit_def = live.emits(self_ref);
            if f.is_extern {
                // An `extern(ffi, "lib")` fn needs a thin wrapper binding it to the
                // loaded library handle. A non-ffi extern is a pre-existing global.
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
                        ExternRet::AsIs => format!("  return {lib}.{nm}({params})"),
                        ExternRet::None => format!("  {lib}.{nm}({params})"),
                    };
                    let _ = writeln!(out, "function {nm}({params})\n{body}\nend");
                }
                continue;
            }
            if let Some((_, body)) = f.inline_body {
                if !emit_def {
                    continue;
                }
                // inline(lua): emit the hand-written body verbatim, wrapping a
                // value-returning bodyless-`return` and prepending default fills.
                // (Js inline bodies were skipped above via the backend guard.)
                let name = mangle_fn(ws, f);
                let params = match skull_mir::lower::BackendFnAbiPlan::for_declaration(ws, scx, self_ref)
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
                    if emit_def {
                        out.push_str(&chunk.lua);
                        // A dropped definition's `static` cells do not exist, so
                        // they must not be registered for hot-reload preservation.
                        all_statics.extend(chunk.statics.iter().cloned());
                        // Only a body that is actually emitted can call one.
                        needed_specs.extend(chunk.specs.iter().cloned());
                    }
                }
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
        &mut needed_specs,
        &cache.chunks,
        &live.spec_chunks,
        &live.rejected,
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
            skull_mir::lower::nobox_global_leaves(ws, g.name, g.ty)
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
        ("cleanup mir", times.cleanup),
        ("emit: regalloc", times.regalloc),
        ("emit: stringify", t_stringify),
        ("scaffold+callgraph+order", t_scaffold),
    ];

    // A synthetic top-level name (`<fn>__lambda<k>`, `<co>__step`/`__on_*`/...)
    // that collides with a user fn/global was recorded during chunk emit (above,
    // and in `emit_incremental`'s re-emit). Reject with a spanned diagnostic
    // rather than emit two clobbering top-level defs. Covers `emit_full` +
    // `emit_incremental` (both funnel through here); the overlay fast path drains
    // in `emit_overlay` (can't return Err, so it warns).
    if let Some(err) = cg.take_rejection() {
        return Err(err);
    }
    Ok((MirEmit { lua: out, skipped, phases }, live.emitted_bodies()))
}

/// Full emit: populate the chunk cache for every ordinary/coroutine body, then
/// assemble. Byte-identical to the pre-cache emit. Returns the cache so a later
/// [`emit_incremental`] can reuse the unchanged chunks.
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
    if let Some((msg, span)) = skull_mir::lower::native_buffer_alloc_reject(ws, user_mid) {
        return Err(CompilerError::new_with_label(span, msg));
    }
    let cg = CodegenCtx::new(ws);
    // The overload set lives in `cg` (computed once there); borrow it rather than
    // rescanning the workspace.
    let scx = skull_mir::lower::ScalarCtx::new(ws);

    // The emitted param list must be injective. Checked per SIGNATURE, over every
    // function rather than just the emitted set: a duplicate Lua param is illegal
    // regardless of which filter would have reached the body.
    for m in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (fid, _) in m.functions.iter() {
            let fref = FuncRef { module: m.id, func: fid };
            for (key, err) in skull_mir::lower::scalar_abi_param_collisions(ws, &scx, fref) {
                cg.reject(key, err);
            }
        }
    }

    let fn_items: Vec<(FuncRef, &bone_semantic::hir::Function)> = ws
        .modules
        .iter()
        .filter(|m| m.id != ModuleId::builtin())
        .flat_map(|m| {
            m.functions
                .iter()
                .filter(|(_, f)| is_cached_body(f))
                .map(move |(fid, f)| (FuncRef { module: m.id, func: fid }, f))
        })
        .collect();
    let chunks = par_map_with(
        fn_items,
        skull_transform::InlineCache::default,
        |inline_cache, (self_ref, f)| {
            (shared(self_ref), emit_body_chunk(ws, &cg, &scx, self_ref, f, inline_cache))
        },
    );
    let chunks: FxHashMap<EmittedBodyId, Result<Chunk, CompilerError>> = chunks.into_iter().collect();
    // Seed the reverse dependency index from every chunk's codegen deps.
    let mut dependents: FxHashMap<FuncRef, FxHashSet<EmittedBodyId>> = FxHashMap::default();
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
    let shader_registrations = shader_registration_lines(ws, renderer_target)?;
    let cache = SkullEmitCache {
        chunks,
        changed_since_full: FxHashSet::default(),
        cg,
        scx,
        inline_cache: skull_transform::InlineCache::default(),
        dependents,
        changed_consts_since_full: FxHashSet::default(),
        static_key_baseline,
        shader_registrations,
        emitted: FxHashSet::default(),
        project_root: project_root.map(str::to_string),
        renderer_target,
    };
    let mut built_specs: Vec<(EmittedBodyId, Chunk)> = Vec::new();
    let (emit, emitted) = assemble(ws, &cache.cg, &cache.scx, &cache, &mut built_specs)?;
    let mut cache = cache;
    cache.emitted = emitted;
    cache.absorb_spec_chunks(built_specs);
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
    });
    if !affected {
        return Ok(false);
    }
    self.shader_registrations = shader_registration_lines(ws, self.renderer_target)?;
    Ok(true)
}

/// Take the specialized chunks an assembly BUILT into the cache, with their
/// reverse-dependency edges, so the next build reuses them and an edit to any
/// declaration they depend on -- their own included -- invalidates them.
fn absorb_spec_chunks(&mut self, built: Vec<(EmittedBodyId, Chunk)>) {
    for (body, chunk) in built {
        for d in &chunk.deps {
            self.dependents.entry(*d).or_default().insert(body.clone());
        }
        self.chunks.insert(body, Ok(chunk));
    }
}

fn update_chunk_cache(
    &mut self,
    ws: &Workspace,
    reinferred: &[(ModuleId, BodyOwner)],
) -> FxHashSet<EmittedBodyId> {
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
    let scx = &cache.scx;
    let cg = &cache.cg;
    // No valid-scan/retain: this path only ever sees BODY edits (an add/remove fn
    // is interface-tier -> the caller routes to `emit_full`, a fresh cache), so the
    // chunk key set -- and the `dependents` index built over it -- is invariant
    // between full builds. There are no dangling `FuncRef`s to prune.

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

    // Affected = changed + every chunk that DEPENDS on a changed fn. The reverse
    // `dependents` index turns this O(edit x fan-out) instead of the old O(chunks)
    // scan. Each chunk's deps already encode its full inline closure, so one
    // reverse hop is complete (no fixpoint) -- identical result to the prior scan.
    let mut affected: FxHashSet<EmittedBodyId> = changed.iter().copied().map(shared).collect();
    for c in &changed {
        if let Some(deps) = cache.dependents.get(c) {
            affected.extend(deps.iter().cloned());
        }
    }
    let _aff_ms = _t_aff.elapsed();

    // The persistent InlineCache keeps unchanged callee bodies across edits; the
    // affected entries are exactly those whose lowered body could have changed
    // (own body edit, own ABI flip, or a callee's ABI flip -- all within
    // `affected`), so invalidate them before re-lowering. Everything else is
    // provably unchanged and reused.
    let _t_relow = Instant::now();
    for body in &affected {
        cache.inline_cache.invalidate(body.decl());
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
        let fref = body.decl();
        let f = &ws.module(fref.module).functions[fref.func];
        // An edit can flip a body between cached (ordinary/coroutine) and
        // regenerated (extern / inline(lua)); only the former has a chunk.
        if is_cached_emitted_body(f, body) {
            let chunk = rebuild_chunk(ws, cg, scx, body, f, &mut cache.inline_cache);
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
    if std::env::var("SKULL_EMIT_PROFILE").is_ok() {
        eprintln!(
            "    update_chunk_cache: affected-scan {:.3}ms (O(edit), {} chunks) | re-lower {:.3}ms ({} affected)",
            _aff_ms.as_secs_f64() * 1000.0,
            cache.chunks.len(),
            _t_relow.elapsed().as_secs_f64() * 1000.0,
            affected.len(),
        );
    }
    affected
}
}

pub fn emit_incremental(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> Result<MirEmit, CompilerError> {
    let t_reemit = Instant::now();
    let _affected = cache.update_chunk_cache(ws, reinferred);
    cache.update_shader_registrations(ws, reinferred)?;
    let reemit = t_reemit.elapsed();

    // Reassembly: the SAME work `emit_full` does (concat every cached chunk +
    // re-emit globals/decorators/prelude/entry), scaling with total output size.
    // The two-file split (`emit_overlay`) avoids it entirely on a body edit.
    let t_asm = Instant::now();
    let mut built_specs: Vec<(EmittedBodyId, Chunk)> = Vec::new();
    let (mut emit, emitted) = assemble(ws, &cache.cg, &cache.scx, cache, &mut built_specs)?;
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
/// Fns the changed chunks now reach that the last full build did not emit.
/// Bounded by `cache.emitted`, which is the leaf set -- so the common case (an
/// edit that calls nothing new) expands nothing.
fn newly_reachable(cache: &SkullEmitCache) -> Vec<EmittedBodyId> {
    struct Backfill<'a> {
        cache: &'a SkullEmitCache,
        roots: Vec<EmittedBodyId>,
    }
    impl skull_mir::reach::ReachPolicy for Backfill<'_> {
        type Node = EmittedBodyId;
        fn roots(&mut self) -> Result<Vec<EmittedBodyId>, CompilerError> {
            Ok(std::mem::take(&mut self.roots))
        }
        fn is_leaf(&mut self, body: EmittedBodyId) -> Result<bool, CompilerError> {
            Ok(self.cache.emitted.contains(&body)
                || !matches!(self.cache.chunks.get(&body), Some(Ok(_))))
        }
        fn on_cycle(&mut self, _body: EmittedBodyId) -> Result<(), CompilerError> {
            Ok(())
        }
        fn visit(&mut self, body: EmittedBodyId) -> Result<Vec<EmittedBodyId>, CompilerError> {
            match self.cache.chunks.get(&body) {
                Some(Ok(chunk)) => Ok(successors_of(chunk)),
                _ => Ok(Vec::new()),
            }
        }
    }
    let mut roots: Vec<EmittedBodyId> = Vec::new();
    for body in &cache.changed_since_full {
        if let Some(Ok(chunk)) = cache.chunks.get(body) {
            roots.extend(successors_of(chunk).into_iter().filter(|b| !cache.emitted.contains(b)));
        }
    }
    if roots.is_empty() {
        return Vec::new();
    }
    let mut policy = Backfill { cache, roots: roots.clone() };
    // Infallible: every policy method returns Ok.
    let mut out = skull_mir::reach::walk(&mut policy).unwrap_or_default();
    out.extend(roots);
    out.retain(|b| !cache.emitted.contains(b));
    out
}

/// The emitted bodies a chunk reaches: its declaration callees AND the
/// specializations it demands.
///
/// Both, keyed the same way. Walking only `calls` -- which are DECLARATIONS --
/// is what made the overlay backfill a shared body and never a specialization.
fn successors_of(chunk: &Chunk) -> Vec<EmittedBodyId> {
    let mut out: Vec<EmittedBodyId> = chunk.calls.iter().copied().map(shared).collect();
    out.extend(chunk.specs.iter().map(|d| EmittedBodyId::new(d.func, d.key.clone())));
    out
}

/// BUILD every specialization the changed bodies now demand that the cache does
/// not already hold, so [`newly_reachable`] has a chunk to find.
///
/// The overlay is loaded after `main.lua`, which carries only what the last full
/// build emitted. An edit that adds the first call to a NEW instantiation
/// therefore needs the definition ADDED, not redefined -- and its call site
/// already spells the specialized symbol, so omitting it is a nil global on
/// reload rather than a stale body.
///
/// This is where the demand's bound tree earns its keep: an `Iterator`-bounded
/// callee reached for the first time by an edit cannot be lowered from its
/// declaration alone.
fn backfill_new_specializations(ws: &Workspace, cache: &mut SkullEmitCache) -> Vec<CompilerError> {
    let mut errs = Vec::new();
    let mut demands: Vec<skull_mir::bound::SpecDemand> = Vec::new();
    for body in &cache.changed_since_full {
        if let Some(Ok(chunk)) = cache.chunks.get(body) {
            demands.extend(chunk.specs.iter().cloned());
        }
    }
    let mut map = skull_mir::bound::DemandMap::default();
    for body in map.record(&demands) {
        errs.push(demand_clash(ws, &body).1);
    }
    // A worklist, not one pass: a specialization built here can demand further
    // ones that no chunk in the cache mentions.
    let mut queue: Vec<EmittedBodyId> =
        demands.iter().map(|d| EmittedBodyId::new(d.func, d.key.clone())).collect();
    let mut seen: FxHashSet<EmittedBodyId> = FxHashSet::default();
    while let Some(body) = queue.pop() {
        if !seen.insert(body.clone()) || cache.chunks.contains_key(&body) {
            continue;
        }
        let EmittedBodyId::Specialized(fref, key) = &body else { continue };
        let f = &ws.module(fref.module).functions[fref.func];
        let Some(name) = skull_mir::mangle::spec_fn_name(ws, f, key) else { continue };
        let tree = map.tree_of(&body);
        let mut specs = Vec::new();
        // Disjoint field borrows: the emitter reads `cg`/`scx` and mutates only
        // `inline_cache`, so the chunk map stays free to be written below.
        let built = emit_spec_chunk(
            ws,
            &cache.cg,
            &cache.scx,
            &body,
            &tree,
            &name,
            &mut cache.inline_cache,
            &mut specs,
        );
        for d in &specs {
            queue.push(EmittedBodyId::new(d.func, d.key.clone()));
        }
        for b in map.record(&specs) {
            errs.push(demand_clash(ws, &b).1);
        }
        match built {
            Ok(chunk) => {
                for d in &chunk.deps {
                    cache.dependents.entry(*d).or_default().insert(body.clone());
                }
                cache.chunks.insert(body.clone(), Ok(chunk));
                cache.changed_since_full.insert(body);
            }
            Err(e) => errs.push(e),
        }
    }
    errs
}

fn assemble_overlay(
    ws: &Workspace,
    cache: &SkullEmitCache,
    shader_changed: bool,
) -> MirEmit {
    let mut out = String::from("-- skull incremental overlay (loaded after main.lua)\n");
    let mut skipped: Vec<(String, String)> = Vec::new();

    // Redefine the changed functions, deterministic order for a stable diff.
    let mut order: Vec<EmittedBodyId> = cache.changed_since_full.iter().cloned().collect();
    // ... plus anything they now reach that the last full build had PRUNED. The
    // overlay redefines only changed fns, so without this a body edit that adds
    // the first call to a dead fn leaves that fn defined nowhere and the reload
    // dies on a nil call. The walk stops at every already-emitted fn (`is_leaf`),
    // so on a normal edit it visits nothing.
    order.extend(newly_reachable(cache));
    order.sort_by_key(overlay_order_key);
    order.dedup();
    for body in &order {
        match cache.chunks.get(body) {
            Some(Ok(chunk)) => out.push_str(&chunk.lua),
            Some(Err(err)) => {
                let fref = body.decl();
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
                let unit = emit_global_unit(ws, &cache.scx, &cache.cg, a.mid, a.gid);
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
                    global_init_target(ws, &cache.cg, g),
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
    let mut rebind_decls: Vec<FuncRef> = order.iter().map(EmittedBodyId::decl).collect();
    rebind_decls.sort_by_key(|fr| (fr.module.index(), u32::from(fr.func.into_raw())));
    rebind_decls.dedup();
    out.push_str(&decorator_rebind_lines(ws, &deco_map, &rebind_decls));

    if shader_changed {
        out.push_str(&cache.shader_registrations.lua);
    }

    MirEmit { lua: out, skipped, phases: Vec::new() }
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
) -> MirEmit {
    let t0 = Instant::now();
    // `cg` is cached (interface-derived, identical on body edits) -- no rebuild.
    let t_cg = t0.elapsed();
    let t1 = Instant::now();
    // `scx` is maintained incrementally inside `update_chunk_cache` (per-body diff)
    // -- no whole-workspace rebuild.
    let t_scx = t1.elapsed();
    let t2 = Instant::now();
    let affected = cache.update_chunk_cache(ws, reinferred);
    cache.changed_since_full.extend(affected);
    let shader_changed = match cache.update_shader_registrations(ws, reinferred) {
        Ok(changed) => changed,
        Err(err) => {
            eprintln!("skull overlay emit: rock_shader registration -- {}", err.message);
            false
        }
    };
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
    // A specialization the edit reached for the FIRST time has no chunk yet, so
    // it must be BUILT before the overlay looks for one. Never swallowed: the
    // overlay's signature cannot return `Err`, so a failure warns here and the
    // next full/incremental rebuild rejects it authoritatively.
    for err in backfill_new_specializations(ws, cache) {
        eprintln!("skull overlay emit: specialization backfill -- {}", err.message);
    }
    let t_relower = t2.elapsed();
    let t3 = Instant::now();
    let mut emit = assemble_overlay(ws, cache, shader_changed);
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
    // Overlay can't return Err (its `MirEmit` return is baked into the watch
    // fast-path callers), but a body edit that mints a newly-colliding synthetic
    // name must not be swallowed. Warn (never-swallow); the next full/incremental
    // rebuild rejects it authoritatively through `assemble`.
    if let Some(err) = cache.cg.take_rejection() {
        eprintln!("skull overlay emit: reserved-name collision -- {}", err.message);
    }
    emit.phases = vec![
        ("CodegenCtx::new", t_cg),
        ("ScalarCtx::new", t_scx),
        ("update_chunk_cache (re-lower changed)", t_relower),
        ("assemble_overlay (+build_deco_map)", t_asm),
    ];
    emit
}
