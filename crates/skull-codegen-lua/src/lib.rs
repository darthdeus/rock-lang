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
//! is a later refinement; see `docs/skull-bone-mir-faithful-plan.md` Phase 5.)

mod mangle;
mod regalloc;
pub mod scope;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::fmt::Write as _;

use bone_semantic::hir::Function;
use bone_semantic::inference::Workspace;
use comfy_la_arena::Idx;
use ustr::Ustr;
use skull_mir::mir::*;

pub use mangle::{compute_overloaded_names, mangle_fn};

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
fn color_cells_timed(func: &MirFunc) -> Vec<u32> {
    let t = std::time::Instant::now();
    let colors = regalloc::color_cells(func);
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

/// Whole-workspace codegen inputs not derivable from a single [`MirFunc`]: the
/// set of overloaded names (drives mangling) and the set of every real function's
/// final Lua name (guards synthetic P3 `<name>__lambda<k>` names against clobber).
/// Computed once per emit.
pub struct CodegenCtx {
    overloaded: HashSet<Ustr>,
    /// Every real function's emitted Lua name — a hoisted lambda name colliding
    /// with one would silently clobber (last-write-wins). See `assert_no_collision`.
    fn_names: HashSet<String>,
}

impl CodegenCtx {
    pub fn new(ws: &Workspace) -> Self {
        let overloaded = compute_overloaded_names(ws);
        let fn_names = ws
            .modules
            .iter()
            .flat_map(|m| m.functions.iter())
            .map(|(_, f)| mangle_fn(ws, f, &overloaded))
            .collect();
        CodegenCtx { overloaded, fn_names }
    }

    /// Reject a hoisted-lambda name that collides with a real function's Lua name
    /// (`fn foo__lambda0` next to a `foo` whose capture-free lambda also hoists to
    /// `foo__lambda0`): the two top-level defs clobber last-write-wins. The
    /// naming scheme owns `<enclosing>__lambda<k>`; a user fn there is rejected,
    /// not silently mis-emitted. Cheap: a hash lookup per hoisted lambda.
    fn assert_no_collision(&self, hoisted_name: &str) {
        assert!(
            !self.fn_names.contains(hoisted_name),
            "hoisted lambda name `{hoisted_name}` collides with a real function of \
             the same name; rename that function (the `__lambda<k>` suffix is reserved)"
        );
    }

    /// The workspace overload set (drives mangling). Computed once in [`new`];
    /// callers that also need it (e.g. `mir_emit`'s `direct_refs`) borrow this
    /// rather than recomputing the workspace-wide scan.
    pub fn overloaded(&self) -> &HashSet<Ustr> {
        &self.overloaded
    }
}

/// Successor blocks of a terminator, in branch order (for reverse-postorder).
/// Visit each successor block of `term` (callback form — avoids the per-call
/// `Vec` alloc; this runs per block, per iteration inside the regalloc liveness
/// fixpoints, where it was a malloc/memmove hot spot). No caller needs the set
/// materialized.
fn successors(term: &Term, mut f: impl FnMut(Idx<Block>)) {
    match term {
        Term::Return(_) | Term::Unreachable => {}
        Term::Goto(b) => f(*b),
        Term::Branch { then_bb, else_bb, .. } => {
            f(*then_bb);
            f(*else_bb);
        }
        Term::Switch { cases, default, .. } => {
            for (_, b) in cases {
                f(*b);
            }
            f(*default);
        }
        Term::Yield { resume, .. } => f(*resume),
    }
}

/// Blocks reachable from `entry`, in arena-id order. Pure `goto`+labels means
/// label order is irrelevant to correctness, so emit in source-faithful id order
/// rather than RPO; unreachable blocks (dead lowering artifacts) are dropped so
/// no dangling `goto` reaches the printer. (RPO returns once counted-loop
/// recovery lands — a real `for` needs its preheader emitted just before the
/// header, which id order doesn't guarantee.)
pub(crate) fn reachable_blocks_from(func: &MirFunc, entry: Idx<Block>) -> Vec<Idx<Block>> {
    let mut visited: HashSet<Idx<Block>> = HashSet::default();
    let mut stack = vec![entry];
    visited.insert(entry);
    while let Some(b) = stack.pop() {
        successors(&func.blocks[b].term, |s| {
            if visited.insert(s) {
                stack.push(s);
            }
        });
    }
    // The entry MUST be emitted first — Lua executes top-down, so it falls into
    // the first label regardless of the `entry` field (matters when the entry is
    // not the lowest-id block, e.g. a coroutine's dispatch entry).
    let mut order = vec![entry];
    order.extend(
        func.blocks
            .iter()
            .map(|(id, _)| id)
            .filter(|id| *id != entry && visited.contains(id)),
    );
    order
}

/// Blocks that lie inside a loop (a CFG cycle): a block that can reach ITSELF via
/// successor edges. A single-assignment cell whose store is in such a block is
/// re-assigned every iteration; one stored only in non-looped blocks is assigned
/// once and never changes -- so a closure capturing it needs no by-value snapshot.
fn looped_blocks(func: &MirFunc, entry: Idx<Block>) -> HashSet<Idx<Block>> {
    let reach = reachable_blocks_from(func, entry);
    let mut looped: HashSet<Idx<Block>> = HashSet::default();
    for &b in &reach {
        let mut seen: HashSet<Idx<Block>> = HashSet::default();
        let mut stack: Vec<Idx<Block>> = Vec::new();
        successors(&func.blocks[b].term, |s| stack.push(s));
        while let Some(x) = stack.pop() {
            if x == b {
                looped.insert(b);
                break;
            }
            if seen.insert(x) {
                successors(&func.blocks[x].term, |s| stack.push(s));
            }
        }
    }
    looped
}

/// Cells eligible for a by-value snapshot when captured by a closure: a
/// single-assignment cell whose (one) store is inside a loop, so a closure
/// created in that loop must capture THIS iteration's value, not the shared
/// cell's final value. A cell stored only outside loops never changes after its
/// single assignment, so capturing it by reference is correct -- and skips the
/// snapshot IIFE entirely (the hot deferred-draw closures live here:
/// `sprite_mesh_submit`, the `DrawList` methods, `render_batches`, none in a
/// loop). Empty inside a coroutine frame (captured cells are state-table fields a
/// param can't shadow) and for funcs with no closures.
fn snapshot_eligible_cells(func: &MirFunc, entry: Idx<Block>, frame: Option<&FrameCtx>) -> HashSet<Idx<Cell>> {
    if frame.is_some() || !func.insts.iter().any(|(_, i)| matches!(i, Inst::Lambda { .. })) {
        return HashSet::default();
    }
    let looped = looped_blocks(func, entry);
    let mut stored_in_loop: HashSet<Idx<Cell>> = HashSet::default();
    for &bb in &looped {
        for &i in &func.blocks[bb].insts {
            match &func.insts[i] {
                Inst::Store { cell, .. } => {
                    stored_in_loop.insert(*cell);
                }
                Inst::MapNext { key_cell, val_cell, .. } => {
                    stored_in_loop.insert(*key_cell);
                    stored_in_loop.insert(*val_cell);
                }
                _ => {}
            }
        }
    }
    single_assignment_cells(func)
        .into_iter()
        .filter(|c| stored_in_loop.contains(c))
        .collect()
}

/// Every closure parameter cell in `func` (across all `Lambda` insts). The
/// enclosing function must NOT declare these as locals — they are each closure's
/// Lua `function(...)` argument list.
pub(crate) fn collect_lambda_params(func: &MirFunc) -> HashSet<Idx<Cell>> {
    let mut out = HashSet::default();
    for (_, inst) in func.insts.iter() {
        if let Inst::Lambda { params, .. } = inst {
            out.extend(params.iter().copied());
        }
    }
    out
}

/// Emit one MIR function as a top-level Lua `function <name>(...) ... end`.
/// `self_ref` identifies the function being emitted so its header name mangles
/// identically to a call site that targets it.
pub fn emit_function(
    ws: &Workspace,
    ctx: &CodegenCtx,
    self_ref: FuncRef,
    func: &MirFunc,
) -> String {
    let name = mangle::mangle_fn(ws, func_of(ws, self_ref), &ctx.overloaded);
    emit_named(ws, ctx, &name, func)
}

/// Emit a MIR function under the EXACT Lua name `name` (no mangling) — for
/// synthetic functions with an already-final name (a global-initializer fn).
/// Calls inside the body still mangle their `FuncRef`s via `ctx`.
pub fn emit_named(ws: &Workspace, ctx: &CodegenCtx, name: &str, func: &MirFunc) -> String {
    emit_named_framed(ws, ctx, name, func, None)
}

/// P3 (docs/closure-env-records-plan.md): the plan for hoisting capture-free
/// lambdas. A lambda whose scope captures nothing (`Ownership::captures[scope]`
/// is empty) emits as `function <name>__lambda<k>(<params>) ... end` and is
/// referenced by that global name at its creation site — a raw Lua function
/// value (no record, no `__call`). A capturing lambda is absent here and still
/// renders inline.
struct HoistPlan {
    /// Capture-free `Inst::Lambda` -> its hoisted global name.
    hoisted_name: HashMap<Idx<Inst>, String>,
    /// Cell -> the scope whose Lua function declares it: `ROOT` (the enclosing
    /// fn) or a hoisted lambda scope. A cell owned by an inline (capturing)
    /// lambda maps to the hoisted/root function that owns that inline closure.
    home: HashMap<Idx<Cell>, scope::ScopeId>,
    /// (`Inst::Lambda`, scope) per hoisted lambda in inst-arena order — the
    /// append pass renders each body as a sibling top-level fn.
    order: Vec<(Idx<Inst>, scope::ScopeId)>,
}

/// The scope whose Lua function declares cells owned by `s`: walk up through
/// inline (non-hoisted) lambda parents to the nearest `ROOT` or hoisted scope.
fn home_scope(tree: &scope::ScopeTree, hoisted: &HashSet<scope::ScopeId>, mut s: scope::ScopeId) -> scope::ScopeId {
    loop {
        if s == scope::ROOT || hoisted.contains(&s) {
            return s;
        }
        s = tree.scopes[s].parent.expect("a non-root scope has a parent");
    }
}

/// Build the [`HoistPlan`], or `None` when no lambda hoists (so `render_scope`'s
/// declaration walk skips the per-cell home probe entirely — the common case).
/// `k` is the `Inst::Lambda`'s inst-arena index order (every lambda consumes a
/// `k`, so a DCE'd one doesn't shift the live ones).
fn build_hoist_plan(func: &MirFunc, tree: &scope::ScopeTree, own: &scope::Ownership, name: &str) -> Option<HoistPlan> {
    let mut lambda_scope: HashMap<Idx<Inst>, scope::ScopeId> = HashMap::default();
    for (sid, s) in tree.scopes.iter().enumerate() {
        if let Some(lid) = s.lambda {
            lambda_scope.insert(lid, sid);
        }
    }
    let mut hoisted_name: HashMap<Idx<Inst>, String> = HashMap::default();
    let mut order = Vec::new();
    let mut k = 0usize;
    for (lid, inst) in func.insts.iter() {
        let Inst::Lambda { .. } = inst else { continue };
        let this_k = k;
        k += 1;
        let Some(&sid) = lambda_scope.get(&lid) else { continue }; // DCE'd lambda: no scope
        if own.captures[sid].is_empty() {
            hoisted_name.insert(lid, format!("{name}__lambda{this_k}"));
            order.push((lid, sid));
        }
    }
    if order.is_empty() {
        return None; // only capturing lambdas -> nothing hoists, no home split needed
    }
    let hoisted_scopes: HashSet<scope::ScopeId> = order.iter().map(|(_, s)| *s).collect();
    let mut home: HashMap<Idx<Cell>, scope::ScopeId> = HashMap::default();
    for (&c, &owner) in &own.owner {
        home.insert(c, home_scope(tree, &hoisted_scopes, owner));
    }
    Some(HoistPlan { hoisted_name, home, order })
}

/// The coroutine state-table context: a `co fn`'s `__step` reads/writes its
/// frame cells as fields of the state table `s` (the first param cell). A frame
/// cell renders `<s>.c<raw>` instead of a bare local and is NOT declared.
pub struct FrameCtx {
    pub s_cell: Idx<Cell>,
    pub fields: HashSet<Idx<Cell>>,
}

/// [`emit_named`] with an optional coroutine frame context (the `__step` fn).
pub fn emit_named_framed(
    ws: &Workspace,
    ctx: &CodegenCtx,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
) -> String {
    let lambda_params = collect_lambda_params(func);
    // Ordinary functions color cells (collapse them onto luajit's 200-local cap);
    // a coroutine `__step` keeps raw `c<raw>` names (frame-field naming depends on
    // the raw id), so it gets an empty map.
    let cell_colors = if frame.is_some() { vec![u32::MAX; func.cells.len()] } else { color_cells_timed(func) };
    // Module-scope const hoisting: an all-constant `value` ctor is built ONCE as a
    // function-owned top-level global and referenced everywhere, instead of rebuilt
    // at each use. On a trace the per-use ctor sinks anyway, but in the interpreter
    // (mag's aborted HUD/text loops) it allocates every time -- the nested
    // `outline_color: Vec2 { 0.0, 0.0 }` per glyph vertex was the entire skull-vs
    // -rock interpreter-path text gap. rock does the same (`__rock_const_*`). The
    // def is emitted in THIS function's chunk (not shared scaffolding), so an
    // incremental body-edit overlay carries it; value structs are immutable, so
    // re-running the def on reload is a harmless no-op (no preservation guard).
    // Mutable env-record captures lowered to a box `{ v = .. }` (P2 of
    // docs/closure-env-records-plan.md). Disabled under a frame (coroutine cells
    // are state-table fields, never boxed) and skipped entirely for the common
    // lambda-free fn (captures need a lambda). Boxed lambda-params are deferred
    // to P2.6 -- left as by-reference upvalues here (inert; today's correct shape).
    // P3: capture-free lambdas hoist to top-level named fns (plan below); the
    // scope analysis also drives box classification. Both need the same tree, so
    // build it once. Skipped for the common lambda-free fn, and CARVED OUT under
    // a frame: a co-fn lambda's cells are state-table fields (`c<s>.c<N>`), not
    // plain locals, so the home/declaration split can't apply -- such a lambda
    // stays an inline closure (correct, just not zero-alloc). P2.6's frame
    // handling subsumes this. (Same gate that disables boxing.)
    let (boxed, plan) = if frame.is_some() || !func.insts.iter().any(|(_, i)| matches!(i, Inst::Lambda { .. })) {
        (HashSet::default(), None)
    } else {
        let tree = scope::ScopeTree::build(func);
        let own = tree.analyze(func);
        let mut b = tree.boxed_cells(&own);
        b.retain(|c| !lambda_params.contains(c));
        let plan = build_hoist_plan(func, &tree, &own, name);
        (b, plan)
    };
    let plan_ref = plan.as_ref();
    if let Some(p) = plan_ref {
        for hoisted in p.hoisted_name.values() {
            ctx.assert_no_collision(hoisted);
        }
    }
    let (hoist, hoist_names, hoist_defs) = collect_const_hoists(func, name);
    let mut out = String::new();
    out.push_str(&hoist_defs);
    let _ = write!(out, "function {}", name);
    render_scope(ws, ctx, func, func.entry(), &func.params, &lambda_params, true, scope::ROOT, plan_ref, frame, &cell_colors, &boxed, &hoist, &hoist_names, &mut out);
    out.push('\n');
    // Append each capture-free lambda body as a sibling top-level fn in THIS
    // chunk (the const-hoist precedent -- an overlay body-edit carries them).
    // Referenced by global name at the creation site, so forward refs resolve at
    // call time. Each renders for its OWN home scope: it declares its own cells,
    // the enclosing fn no longer does.
    if let Some(plan) = plan_ref {
        for &(lid, sid) in &plan.order {
            let Inst::Lambda { params, entry } = &func.insts[lid] else { continue };
            let _ = write!(out, "function {}", plan.hoisted_name[&lid]);
            render_scope(ws, ctx, func, *entry, params, &lambda_params, true, sid, plan_ref, frame, &cell_colors, &boxed, &hoist, &hoist_names, &mut out);
            out.push('\n');
        }
    }
    out
}

/// Group every all-constant `MakeValueStruct` (every field operand a `Const`) by
/// (struct name, field literals). Returns `(inst -> group, group -> global name,
/// def lines)`: each group becomes one function-owned global
/// `__skull_kc_<fn>_<g> = <Name>_ct(<lits>)`. Hoisting is unconditional -- a
/// module-scope global is built once at chunk load, so it is never worse than a
/// per-use ctor (which it equals on a trace via SINK and beats in the
/// interpreter), unlike a per-CALL function-entry local which defeats SINK.
fn collect_const_hoists(
    func: &MirFunc,
    base: &str,
) -> (HashMap<Idx<Inst>, u32>, Vec<String>, String) {
    let mut groups: Vec<(ustr::Ustr, Vec<Lit>)> = Vec::new();
    let mut hoist: HashMap<Idx<Inst>, u32> = HashMap::default();
    for (id, inst) in func.insts.iter() {
        let Inst::MakeValueStruct { name, fields } = inst else {
            continue;
        };
        let lits: Option<Vec<Lit>> = fields
            .iter()
            .map(|(_, op)| match &func.insts[*op] {
                Inst::Const(l) => Some(l.clone()),
                _ => None,
            })
            .collect();
        let Some(lits) = lits else { continue };
        let g = match groups.iter().position(|(n, l)| *n == *name && *l == lits) {
            Some(p) => p as u32,
            None => {
                groups.push((*name, lits));
                (groups.len() - 1) as u32
            }
        };
        hoist.insert(id, g);
    }
    if groups.is_empty() {
        return (hoist, Vec::new(), String::new());
    }
    let mut names = Vec::with_capacity(groups.len());
    let mut defs = String::new();
    for (g, (name, lits)) in groups.iter().enumerate() {
        let gname = format!("__skull_kc_{base}_{g}");
        let _ = write!(defs, "{gname} = {name}_ct(");
        for (i, l) in lits.iter().enumerate() {
            if i > 0 {
                defs.push_str(", ");
            }
            write_lit(&mut defs, l);
        }
        defs.push_str(")\n");
        names.push(gname);
    }
    (hoist, names, defs)
}

/// Cells assigned by exactly one static `Store` and never by a loop `MapNext` —
/// an immutable binding. A closure may snapshot such a cell BY VALUE at creation
/// (the IIFE in the `Lambda` arm) so a closure made INSIDE a loop captures THIS
/// iteration's value rather than the shared function-level local. A cell with
/// multiple stores (e.g. a loop counter, a `var`) stays by-reference.
fn single_assignment_cells(func: &MirFunc) -> HashSet<Idx<Cell>> {
    let mut counts: HashMap<Idx<Cell>, u32> = HashMap::default();
    let mut multi: HashSet<Idx<Cell>> = HashSet::default();
    for (_, b) in func.blocks.iter() {
        for &id in &b.insts {
            match &func.insts[id] {
                Inst::Store { cell, .. } => *counts.entry(*cell).or_insert(0) += 1,
                Inst::MapNext { key_cell, val_cell, .. } => {
                    multi.insert(*key_cell);
                    multi.insert(*val_cell);
                }
                // A dest write is invisible to the Store count; a cell it hits
                // must never snapshot by value (raw lowering only dest-writes
                // fresh temps, but the inliner rebinds dests onto surviving
                // cells).
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => multi.extend(dests.iter().copied()),
                _ => {}
            }
        }
    }
    counts
        .into_iter()
        .filter(|(c, n)| *n == 1 && !multi.contains(c))
        .map(|(c, _)| c)
        .collect()
}

/// The cells a closure body (rooted at `entry`, with `params`) reads as upvalues:
/// `Load`ed in its OWN CFG but neither a param nor a body-local `Store` target.
/// Does NOT descend into nested closures (their captures resolve when THEY are
/// emitted) — `reachable_blocks_from` follows terminators, not `Lambda` operands.
fn lambda_captured_cells(func: &MirFunc, entry: Idx<Block>, params: &[Idx<Cell>]) -> HashSet<Idx<Cell>> {
    let mut loaded: HashSet<Idx<Cell>> = HashSet::default();
    let mut owned: HashSet<Idx<Cell>> = params.iter().copied().collect();
    for bb in reachable_blocks_from(func, entry) {
        for &id in &func.blocks[bb].insts {
            match &func.insts[id] {
                Inst::Load(c) => {
                    loaded.insert(*c);
                }
                Inst::Store { cell, .. } => {
                    owned.insert(*cell);
                }
                _ => {}
            }
        }
    }
    loaded.retain(|c| !owned.contains(c));
    loaded
}

/// The Lua name for a cell read/write: a coroutine frame cell renders as the
/// state-table field `<s>.c<raw>`; everything else is its colored local
/// (`c<color>`) or — when uncolored (a coroutine `__step`, passed an empty
/// color map) — the raw `c<raw>`.
fn cellname(frame: Option<&FrameCtx>, colors: &[u32], boxed: &HashSet<Idx<Cell>>, c: Idx<Cell>) -> String {
    let mut s = String::new();
    write_cellname(&mut s, frame, colors, boxed, c);
    s
}

/// Append a cell reference (frame field `c<s>.c<field>` or plain local) directly
/// into `out` — the per-operand-allocation-free primitive `cellname` wraps. A
/// boxed cell (`boxed` set, env-record capture) reads/writes through its box's
/// `.v` field. Frame cells are never boxed (`boxed` is empty under a frame).
fn write_cellname(out: &mut String, frame: Option<&FrameCtx>, colors: &[u32], boxed: &HashSet<Idx<Cell>>, c: Idx<Cell>) {
    if let Some(f) = frame {
        if f.fields.contains(&c) {
            out.push('c');
            push_uint(out, raw(f.s_cell));
            out.push_str(".c");
            push_uint(out, raw(c));
            return;
        }
    }
    write_cell_local(out, colors, c);
    if boxed.contains(&c) {
        out.push_str(".v");
    }
}

/// The Lua local name for a cell in a non-frame context (declaration, param
/// list, closure snapshot, body reference): its color `c<color>` when colored,
/// else the raw `c<raw>`. An ordinary function colors ALL its cells (one `c`
/// namespace, no raw/colored collision); a coroutine `__step` colors none.
fn cell_local(colors: &[u32], c: Idx<Cell>) -> String {
    let mut s = String::new();
    write_cell_local(&mut s, colors, c);
    s
}

fn write_cell_local(out: &mut String, colors: &[u32], c: Idx<Cell>) {
    out.push('c');
    match colors[raw(c) as usize] {
        u32::MAX => push_uint(out, raw(c)),
        col => push_uint(out, col),
    }
}

// --- relooper: counted-`for` recovery (structured control flow) ------------
//
// A `for i in lo..hi` / `for x in xs` / `enumerate(..)` lowers to a canonical
// 4-block counted-loop CFG: a preheader storing the counter, a header testing
// `i < hi` (or `<=`), a body, and a pure-increment latch jumping back. LuaJIT
// specializes a real numeric `for` (FORL induction) but UNROLLS a goto-loop ->
// mcode blowup -> blacklist -> interpreter spiral (docs/skull-throughput-
// investigation.md). So we pattern-match that exact shape and emit a Lua `for`,
// leaving every other terminator as goto. Recognition runs at EMIT time, on the
// post-transform CFG, so it is robust to the coroutine transform / inlining
// renumbering blocks (the design's Phase-8 "reloop over the same MIR").

/// The upper bound of a counted loop — the RHS of the header `i < HI` compare.
enum HiLimit {
    /// A value computed in the preheader (e.g. `#arr`); live through the header.
    Value(Idx<Inst>),
    /// A bound cell loaded in the header (`for i in lo..hi`'s `hi`).
    Cell(Idx<Cell>),
}

/// A recognized counted loop: the `for slot = <slot>, hi[-1] do <body> end` shape.
/// The init reads `slot` (the preheader's `Store{slot, lo}` set it) so it never
/// depends on `lo`'s register still being live at the loop head.
struct CountedLoop {
    slot: Idx<Cell>,
    hi: HiLimit,
    inclusive: bool,
    body_entry: Idx<Block>,
    latch: Idx<Block>,
    exit: Idx<Block>,
    /// The natural-loop body blocks (excludes header + latch).
    region: HashSet<Idx<Block>>,
}

/// Reverse-postorder of the blocks reachable from `entry`. A loop preheader
/// dominates its header, so RPO emits it first — the `for` then falls through
/// into the header position (a `for` can't be a goto target, unlike a label).
/// Unreachable lowering-artifact blocks are dropped.
fn reverse_postorder(func: &MirFunc, entry: Idx<Block>) -> Vec<Idx<Block>> {
    let mut post = Vec::new();
    let mut visited: HashSet<Idx<Block>> = HashSet::default();
    let mut stack = vec![(entry, false)];
    visited.insert(entry);
    while let Some((b, processed)) = stack.pop() {
        if processed {
            post.push(b);
            continue;
        }
        stack.push((b, true));
        successors(&func.blocks[b].term, |s| {
            if visited.insert(s) {
                stack.push((s, false));
            }
        });
    }
    post.reverse();
    post
}

/// (all predecessors, goto-only predecessors) of each reachable block. Loop
/// edges are unconditional gotos (preheader->header, latch->header), so the
/// goto-pred set isolates the two loop entries.
type PredMap = HashMap<Idx<Block>, Vec<Idx<Block>>>;
fn predecessors(func: &MirFunc, rpo: &[Idx<Block>]) -> (PredMap, PredMap) {
    let reach: HashSet<Idx<Block>> = rpo.iter().copied().collect();
    let mut all: PredMap = HashMap::default();
    let mut goto: PredMap = HashMap::default();
    for &b in rpo {
        match &func.blocks[b].term {
            Term::Goto(t) => {
                all.entry(*t).or_default().push(b);
                goto.entry(*t).or_default().push(b);
            }
            t => {
                successors(t, |s| {
                    if reach.contains(&s) {
                        all.entry(s).or_default().push(b);
                    }
                });
            }
        }
    }
    (all, goto)
}

/// Every counted loop in `func`, keyed by header block. `frame` excludes a
/// coroutine state-field cell (it can't be a Lua `for` variable).
fn recognize_counted_loops(
    func: &MirFunc,
    rpo: &[Idx<Block>],
    frame: Option<&FrameCtx>,
) -> HashMap<Idx<Block>, CountedLoop> {
    let (all_preds, goto_preds) = predecessors(func, rpo);
    let mut loops = HashMap::default();
    for &h in rpo {
        if let Some(cl) = try_match_loop(func, h, &all_preds, &goto_preds, frame) {
            loops.insert(h, cl);
        }
    }
    loops
}

fn try_match_loop(
    func: &MirFunc,
    h: Idx<Block>,
    all_preds: &PredMap,
    goto_preds: &PredMap,
    frame: Option<&FrameCtx>,
) -> Option<CountedLoop> {
    let Term::Branch { cond, then_bb, else_bb } = func.blocks[h].term else {
        return None;
    };
    // Header is entered by exactly the preheader + the latch, both via goto.
    let gp = goto_preds.get(&h)?;
    if gp.len() != 2 || all_preds.get(&h).map_or(0, |v| v.len()) != 2 {
        return None;
    }
    // Exactly one goto-pred is a pure-increment latch; the other is the preheader.
    let mut latch = None;
    let mut slot = None;
    for &p in gp {
        if let Some(s) = latch_slot(func, p, h) {
            latch = Some(p);
            slot = Some(s);
        }
    }
    let (latch, slot) = (latch?, slot?);
    if frame.is_some_and(|f| f.fields.contains(&slot)) {
        return None;
    }
    let (hi, inclusive) = match_header(func, h, cond, slot)?;
    let region = compute_region(all_preds, then_bb, h, latch)?;
    if !slot_sound(func, slot, h, latch, &region) {
        return None;
    }
    Some(CountedLoop { slot, hi, inclusive, body_entry: then_bb, latch, exit: else_bb, region })
}

/// `Some(slot)` iff block `p` is a pure 4-inst increment latch jumping to
/// `header`: `vj = load slot; one = 1; vinc = vj + one; store slot = vinc`. The
/// strictness is load-bearing — a loop whose increment is fused into the body
/// (no dedicated latch) would otherwise match and double-increment.
fn latch_slot(func: &MirFunc, p: Idx<Block>, header: Idx<Block>) -> Option<Idx<Cell>> {
    if func.blocks[p].term != Term::Goto(header) {
        return None;
    }
    let ins = &func.blocks[p].insts;
    if ins.len() != 4 {
        return None;
    }
    let (Inst::Load(slot), Inst::Const(Lit::Int(1)), Inst::IntAdd(a, b), Inst::Store { cell, value }) =
        (&func.insts[ins[0]], &func.insts[ins[1]], &func.insts[ins[2]], &func.insts[ins[3]])
    else {
        return None;
    };
    (*a == ins[0] && *b == ins[1] && *cell == *slot && *value == ins[2]).then_some(*slot)
}

/// The header must be exactly `load slot; [load hi_cell;] (slot < HI | slot <= HI)`
/// — the counter load(s) feeding the compare, nothing else (no side effects).
fn match_header(
    func: &MirFunc,
    h: Idx<Block>,
    cond: Idx<Inst>,
    slot: Idx<Cell>,
) -> Option<(HiLimit, bool)> {
    let (lhs, rhs, inclusive) = match &func.insts[cond] {
        Inst::IntLt(a, b) => (*a, *b, false),
        Inst::IntLeq(a, b) => (*a, *b, true),
        _ => return None,
    };
    let insts = &func.blocks[h].insts;
    // LHS is a Load(slot) present in the header.
    if !matches!(&func.insts[lhs], Inst::Load(s) if *s == slot) || !insts.contains(&lhs) {
        return None;
    }
    // RHS is either a bound cell loaded in the header, or a preheader value.
    let rhs_is_cell_load =
        matches!(&func.insts[rhs], Inst::Load(_)) && insts.contains(&rhs);
    let allowed: HashSet<Idx<Inst>> = if rhs_is_cell_load {
        [lhs, rhs, cond].into_iter().collect()
    } else {
        [lhs, cond].into_iter().collect()
    };
    if insts.iter().any(|i| !allowed.contains(i)) {
        return None;
    }
    let hi = match &func.insts[rhs] {
        Inst::Load(c) if rhs_is_cell_load => HiLimit::Cell(*c),
        _ => HiLimit::Value(rhs),
    };
    Some((hi, inclusive))
}

/// The natural loop of the back-edge `latch -> header`: blocks that reach the
/// latch WITHOUT crossing the header (backward reach over preds), NOT
/// "everything reachable from body_entry" — a `break`/early-return path doesn't
/// reach the latch, so it stays OUTSIDE the loop and exits via goto. Returns
/// `None` if the body can't iterate, or the loop is multi-entry (irreducible).
fn compute_region(
    all_preds: &PredMap,
    body_entry: Idx<Block>,
    header: Idx<Block>,
    latch: Idx<Block>,
) -> Option<HashSet<Idx<Block>>> {
    let mut natural: HashSet<Idx<Block>> = HashSet::default();
    natural.insert(latch);
    let mut stack = vec![latch];
    while let Some(b) = stack.pop() {
        if let Some(ps) = all_preds.get(&b) {
            for &p in ps {
                if p != header && natural.insert(p) {
                    stack.push(p);
                }
            }
        }
    }
    if !natural.contains(&body_entry) {
        return None;
    }
    natural.remove(&latch);
    // Reducible / single-entry: a region block may be entered only from within
    // region ∪ {header, latch} — else a goto jumps into the middle of the `for`.
    for &b in &natural {
        if let Some(ps) = all_preds.get(&b) {
            for &p in ps {
                if p != header && p != latch && !natural.contains(&p) {
                    return None;
                }
            }
        }
    }
    Some(natural)
}

/// The counter slot must behave like a loop-scoped `for` variable: stored
/// EXACTLY twice (preheader init + latch increment — so it's never reassigned in
/// the body), and never read outside the loop region (incl. closures, whose body
/// blocks are outside the region — a captured counter can't be the for-variable).
fn slot_sound(
    func: &MirFunc,
    slot: Idx<Cell>,
    header: Idx<Block>,
    latch: Idx<Block>,
    region: &HashSet<Idx<Block>>,
) -> bool {
    let in_loop: HashSet<Idx<Block>> =
        region.iter().copied().chain([header, latch]).collect();
    let mut stores = 0usize;
    for (bid, b) in func.blocks.iter() {
        let inside = in_loop.contains(&bid);
        for &i in &b.insts {
            match &func.insts[i] {
                Inst::Store { cell, .. } if *cell == slot => stores += 1,
                Inst::Load(c) if *c == slot && !inside => return false,
                Inst::MapNext { key_cell, val_cell, .. }
                    if *key_cell == slot || *val_cell == slot =>
                {
                    return false
                }
                _ => {}
            }
        }
    }
    stores == 2
}

/// Whether a value-producing inst is ALWAYS safe to tree-fold into its single use
/// site, regardless of what executes between def and use: a pure computation over
/// value operands only (operands are SSA — immutable once computed) with no read
/// of mutable state (cell/global/heap) and no side effect. Excludes `Load`/
/// `Global`/`Field`/`Index`/`Len`/`Call` (order-sensitive memory reads / effects)
/// and `Lambda` (rendered as its own scope) — those keep their register.
/// The splice-able shape of an `inline(lua)` body (for `@inline` splicing).
#[derive(Clone, Copy)]
enum InlineLuaShape {
    /// A single bare expression (no `return`, no statement keyword) -> the body
    /// IS the rvalue (`<result> = <body>`).
    Expr,
    /// Statements ending in a single TAIL `return X` -> emit the leading
    /// statements verbatim, rewrite the tail to `<result> = X`.
    TailReturn,
    /// Return-free statements (a unit body, e.g. `buf.ptr[i] = v`) -> verbatim,
    /// no result bound.
    UnitStmts,
}

/// `Some(rest)` if `line` (trimmed) is a Lua `return` statement, where `rest` is
/// the returned expression (`""` for a bare `return`). Word-boundary aware so
/// `returned`/`return_x` don't match. `None` otherwise.
fn inline_lua_return_rest(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("return")?;
    match rest.chars().next() {
        None => Some(""),
        Some(c) if !c.is_alphanumeric() && c != '_' => Some(rest.trim()),
        _ => None,
    }
}

/// Classify an `inline(lua)` body for splicing, or `None` if it is not
/// splice-safe (early or multiple `return`s, whose line-by-line rewrite to
/// `<result> = X` would not preserve the control flow -- fall back to a call).
fn classify_inline_lua_body(body: &str) -> Option<InlineLuaShape> {
    let t = body.trim();
    if t.is_empty() {
        return Some(InlineLuaShape::UnitStmts);
    }
    let nonempty: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
    let returns = nonempty.iter().filter(|l| inline_lua_return_rest(l).is_some()).count();
    if returns == 0 {
        return Some(if is_expr_inline_lua_body(t) {
            InlineLuaShape::Expr
        } else {
            InlineLuaShape::UnitStmts
        });
    }
    // Exactly one return, and it is the LAST non-empty line -> tail return.
    let last_is_return = nonempty.last().is_some_and(|l| inline_lua_return_rest(l).is_some());
    if returns == 1 && last_is_return {
        Some(InlineLuaShape::TailReturn)
    } else {
        None
    }
}

/// Emit the spliced (return-free / tail-return) body, rewriting the tail
/// `return X` to `<lv> = X`. Param locals are emitted by the caller; this writes
/// only the body. Each line keeps the body's own text (Lua ignores indentation).
fn render_spliced_inline_lua(out: &mut String, body: &str, shape: InlineLuaShape, lv: &str) {
    match shape {
        InlineLuaShape::Expr => {
            let _ = writeln!(out, "    {lv} = {}", body.trim());
        }
        InlineLuaShape::UnitStmts => {
            for line in body.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = writeln!(out, "    {}", line.trim_end());
            }
        }
        InlineLuaShape::TailReturn => {
            for line in body.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match inline_lua_return_rest(line) {
                    Some("") => {} // bare tail return: nothing to bind (unit)
                    Some(rest) => {
                        let _ = writeln!(out, "    {lv} = {rest}");
                    }
                    None => {
                        let _ = writeln!(out, "    {}", line.trim_end());
                    }
                }
            }
        }
    }
}

/// Is this (return-free) `inline(lua)` body a single bare expression (so it can
/// be the RHS of `lv = …`), vs a statement / multi-statement unit body? Any
/// statement-keyword start, a top-level `=` assignment, `;`, or a second
/// non-blank line => stmts.
fn is_expr_inline_lua_body(s: &str) -> bool {
    let t = s.trim();
    let mut lines = t.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false; // multi-line => statements
    }
    let f = first.trim_start();
    for kw in ["local ", "if ", "for ", "while ", "do ", "return", "::", "repeat", "break", "goto "] {
        if f.starts_with(kw) {
            return false;
        }
    }
    if f.contains(';') {
        return false;
    }
    // A top-level (paren-depth 0) `=` that is not ==/~=/<=/>= is an assignment.
    let b = f.as_bytes();
    let mut depth: i32 = 0;
    for i in 0..b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'=' if depth == 0 => {
                let prev = if i > 0 { b[i - 1] } else { 0 };
                let next = if i + 1 < b.len() { b[i + 1] } else { 0 };
                let is_cmp = next == b'=' || matches!(prev, b'=' | b'~' | b'<' | b'>' | b'!');
                if !is_cmp {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

fn always_inlinable(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::Const(_)
            | Inst::IntAdd(..) | Inst::IntSub(..) | Inst::IntMul(..) | Inst::IntDiv(..) | Inst::IntMod(..)
            | Inst::FloatAdd(..) | Inst::FloatSub(..) | Inst::FloatMul(..) | Inst::FloatDiv(..) | Inst::FloatMod(..)
            | Inst::StringConcat(..)
            | Inst::IntLt(..) | Inst::IntLeq(..) | Inst::IntGt(..) | Inst::IntGeq(..)
            | Inst::FloatLt(..) | Inst::FloatLeq(..) | Inst::FloatGt(..) | Inst::FloatGeq(..)
            | Inst::Eq(..) | Inst::NotEq(..)
            | Inst::BitAnd(..) | Inst::BitOr(..) | Inst::BitXor(..) | Inst::Shl(..) | Inst::Shr(..)
            | Inst::Neg(_) | Inst::Not(_) | Inst::BitNot(_) | Inst::ToString(_) | Inst::FloatToInt(_)
            | Inst::Format { .. } | Inst::FormatDyn { .. }
            | Inst::MakeTuple(_) | Inst::MakeArray(_) | Inst::MakeVec(_)
            | Inst::MakeStruct { .. } | Inst::MakeValueStruct { .. } | Inst::MakeEnum { .. }
            | Inst::MakeEnumDyn { .. }
    )
}

/// The set of values to TREE-FOLD into their use site (no `local rN`): an
/// `always_inlinable` value used EXACTLY ONCE within this scope, def and use in
/// the same block. Single-use keeps it size-neutral (no duplicated expression);
/// "always-safe" + same-block means the fold is correct no matter what runs in
/// between. (Values are single-block by construction, so same-block nearly always
/// holds; the guard keeps it honest.) The colorer skips these; the emitter emits
/// each value's expression at its one use via [`Emitter::opnd`].
fn compute_inlinable(func: &MirFunc, scope: &[Idx<Block>]) -> Vec<bool> {
    // Dense side-tables keyed by raw inst id (no hashing): use count, and the
    // block of the (single) use and of the def. `NONE` = unset. Insts outside
    // `scope` (other closures share the arena) stay at 0/NONE and never qualify.
    const NONE: u32 = u32::MAX;
    let ni = func.insts.len();
    let mut use_count = vec![0u32; ni];
    let mut use_block = vec![NONE; ni];
    let mut def_block = vec![NONE; ni];
    for &b in scope {
        let br = raw(b);
        for &id in &func.blocks[b].insts {
            if func.insts[id].produces_value() {
                def_block[raw(id) as usize] = br;
            }
            regalloc::inst_uses(&func.insts[id], &mut |u| {
                let k = raw(u) as usize;
                use_count[k] += 1;
                use_block[k] = br;
            });
        }
        regalloc::term_uses(&func.blocks[b].term, &mut |u| {
            let k = raw(u) as usize;
            use_count[k] += 1;
            use_block[k] = br;
        });
    }
    // Dense `raw(inst) -> bool` (no hashing); probed per operand in `write_opnd`
    // and per use in `color_values`'s liveness/interference loops.
    let mut inl = vec![false; ni];
    for &b in scope {
        for &id in &func.blocks[b].insts {
            let k = raw(id) as usize;
            if use_count[k] == 1
                && def_block[k] != NONE
                && def_block[k] == use_block[k]
                && always_inlinable(&func.insts[id])
            {
                inl[k] = true;
            }
        }
    }
    inl
}

/// Render `function(<params>) <locals> <blocks> end` for a scope rooted at
/// `entry` (the top function or a closure body), appending to `out` AFTER its
/// already-written name. Values are colored over ONLY this scope's reachable
/// blocks (a closure's values are independent of the enclosing scope; `r<n>`
/// names shadow safely since only cells cross the boundary). `is_top` declares
/// the function's cells (a closure declares none — its params are the arg list,
/// captures/locals are enclosing upvalues).
#[allow(clippy::too_many_arguments)]
fn render_scope(
    ws: &Workspace,
    ctx: &CodegenCtx,
    func: &MirFunc,
    entry: Idx<Block>,
    params: &[Idx<Cell>],
    lambda_params: &HashSet<Idx<Cell>>,
    is_top: bool,
    home: scope::ScopeId,
    plan: Option<&HoistPlan>,
    frame: Option<&FrameCtx>,
    cell_colors: &[u32],
    boxed: &HashSet<Idx<Cell>>,
    hoist: &HashMap<Idx<Inst>, u32>,
    hoist_names: &[String],
    out: &mut String,
) {
    // RPO drives both block layout (preheader-before-header, for the relooper)
    // and the reachable set the regalloc colors over.
    let rpo = reverse_postorder(func, entry);
    let loops = recognize_counted_loops(func, &rpo, frame);
    // Cells a captured closure must snapshot by value (loop-stored single
    // assignments); everything else captures by reference -- no snapshot IIFE.
    // A boxed cell is by-reference through its box, never by-value snapshotted.
    let mut snap_cells = snapshot_eligible_cells(func, entry, frame);
    snap_cells.retain(|c| !boxed.contains(c));
    // Tree-folded values get no register; the colorer sees through them.
    let inlinable = compute_inlinable(func, &rpo);
    let t_ra = std::time::Instant::now();
    let colors = regalloc::color_values(func, &rpo, &inlinable);
    REGALLOC_TIME.with(|c| c.set(c.get() + t_ra.elapsed()));

    // Param list `(a, b, c)` — written straight into the buffer (no Vec<String>).
    out.push('(');
    for (i, &c) in params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_cell_local(out, cell_colors, c);
    }
    out.push_str(")\n");

    // Declare registers in sorted color order (the dense table holds `u32::MAX`
    // for uncolored slots — inlined / non-value insts — which we skip), then the
    // cell locals. All written directly into a single `  local ...` line; we
    // stamp the keyword up front and `truncate` it back if nothing is declared.
    let mut distinct: Vec<u32> = colors.iter().copied().filter(|&c| c != u32::MAX).collect();
    distinct.sort_unstable();
    distinct.dedup();
    let decl_start = out.len();
    out.push_str("  local ");
    let mut any = false;
    for &c in &distinct {
        if any {
            out.push_str(", ");
        }
        any = true;
        out.push('r');
        push_uint(out, c);
    }
    // A cell is declared in the function whose `home` scope owns it (P3): the
    // enclosing fn (ROOT) or a hoisted lambda. With no plan (lambda-free / frame),
    // every cell homes to ROOT so this is a no-op and the top fn declares all.
    let declares_here = |cid: Idx<Cell>| -> bool {
        plan.map_or(true, |p| p.home.get(&cid).copied().unwrap_or(scope::ROOT) == home)
    };
    if is_top {
        let param_cells: HashSet<Idx<Cell>> = params.iter().copied().collect();
        let is_frame = |c: &Idx<Cell>| frame.is_some_and(|f| f.fields.contains(c));
        // Declare each cell LOCAL exactly once. Colored cells collapse onto their
        // shared `c<color>` (deduped, and excluding any color already named by a
        // param); uncolored cells (coroutine `__step`) declare per raw id.
        let param_colors: HashSet<u32> = params
            .iter()
            .map(|c| cell_colors[raw(*c) as usize])
            .filter(|&col| col != u32::MAX)
            .collect();
        let mut seen_color: HashSet<u32> = HashSet::default();
        for (cid, _) in func.cells.iter() {
            // Boxed cells get a separate `= {}` init line below, not the comma list.
            if param_cells.contains(&cid) || lambda_params.contains(&cid) || is_frame(&cid) || boxed.contains(&cid) || !declares_here(cid) {
                continue;
            }
            let (decl, col) = match cell_colors[raw(cid) as usize] {
                u32::MAX => (true, None),                        // uncolored -> c<raw>
                col if !param_colors.contains(&col) => (seen_color.insert(col), Some(col)),
                _ => (false, None),                             // color already a param's
            };
            if !decl {
                continue;
            }
            if any {
                out.push_str(", ");
            }
            any = true;
            out.push('c');
            match col {
                Some(col) => push_uint(out, col),
                None => push_uint(out, raw(cid)),
            }
        }
    }
    if any {
        out.push('\n');
    } else {
        out.truncate(decl_start); // no locals -> drop the stamped `  local `
    }

    // Boxed cells: a dedicated `local c<col> = {}` (an env-record capture cell,
    // stored/loaded through `.v`), or `local c<col> = { v = c<col> }` for a
    // captured param (seed from the param value, shadowing it). Boxed cells are
    // force-unique (regalloc) so each owns a distinct color -> no dedup needed.
    // Only the top scope declares (an inline lambda shares the top fn's cells).
    if is_top {
        let param_cells: HashSet<Idx<Cell>> = params.iter().copied().collect();
        for (cid, _) in func.cells.iter() {
            if !boxed.contains(&cid) || !declares_here(cid) {
                continue;
            }
            out.push_str("  local ");
            write_cell_local(out, cell_colors, cid);
            if param_cells.contains(&cid) {
                out.push_str(" = { v = ");
                write_cell_local(out, cell_colors, cid);
                out.push_str(" }\n");
            } else {
                out.push_str(" = {}\n");
            }
        }
    }

    let mut em = Emitter {
        ws,
        ctx,
        func,
        colors,
        cell_colors,
        lambda_params,
        frame,
        loops: &loops,
        rpo: &rpo,
        inlinable,
        emitted: vec![false; func.blocks.len()],
        hoist,
        hoist_names,
        snap_cells: &snap_cells,
        boxed,
        plan,
        home,
    };
    for &bb in &rpo {
        em.emit_block(out, bb); // self-skips an already-emitted block (loop body)
    }
    out.push_str("end");
}

fn func_of(ws: &Workspace, r: FuncRef) -> &Function {
    &ws.module(r.module).functions[r.func]
}

/// The mangled Lua name for a `FuncRef` (the same name a call site emits).
pub fn mangled(ws: &Workspace, ctx: &CodegenCtx, r: FuncRef) -> String {
    mangle::mangle_fn(ws, func_of(ws, r), &ctx.overloaded)
}

/// The Lua local name for a value: its register `r<color>` (colored values) or
/// the raw `v<raw>` fallback (an uncolored value — should not occur once every
/// value is colored, but keeps the printer total).
fn write_vname(out: &mut String, colors: &[u32], id: Idx<Inst>) {
    match colors[raw(id) as usize] {
        u32::MAX => {
            out.push('v');
            push_uint(out, raw(id));
        }
        c => {
            out.push('r');
            push_uint(out, c);
        }
    }
}

struct Emitter<'a> {
    ws: &'a Workspace,
    ctx: &'a CodegenCtx,
    func: &'a MirFunc,
    /// Dense `raw(inst) -> color` (`u32::MAX` = uncolored); read per operand by
    /// [`write_vname`] as `colors[raw(id)]` instead of a hash probe.
    colors: Vec<u32>,
    cell_colors: &'a [u32],
    lambda_params: &'a HashSet<Idx<Cell>>,
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
    /// Captured cells that need a by-value snapshot (loop-stored single
    /// assignments); a captured cell NOT in here is taken by reference (no IIFE).
    snap_cells: &'a HashSet<Idx<Cell>>,
    /// Cells lowered to a box `{ v = ... }` (mutable env-record captures); every
    /// read/write of one renders through `.v`. Empty under a frame.
    boxed: &'a HashSet<Idx<Cell>>,
    /// P3 hoist plan: a capture-free `Inst::Lambda` in `hoisted_name` renders as
    /// a bare global-name reference instead of an inline closure. `None` for
    /// lambda-free fns and frames.
    plan: Option<&'a HoistPlan>,
    /// The scope this function renders for (ROOT or a hoisted lambda) — threaded
    /// into inline-lambda renders (unused there: they declare nothing).
    home: scope::ScopeId,
}

impl Emitter<'_> {
    fn emit_block(&mut self, out: &mut String, bb: Idx<Block>) {
        if self.emitted[raw(bb) as usize] {
            return; // already emitted (e.g. as a loop body block)
        }
        self.emitted[raw(bb) as usize] = true;
        out.push_str("  ::bb");
        push_uint(out, raw(bb));
        out.push_str("::\n");
        let insts: &[Idx<Inst>] = &self.func.blocks[bb].insts;
        let mut i = 0;
        while i < insts.len() {
            let id = insts[i];
            // A NativeBuffer slot read (`__nb_checked_get`) feeding only field
            // stores is emitted as a `do local __slot = ..; __slot.f = .. end`
            // cluster -- a BLOCK-scoped local, not a reused function register. A
            // function-level register holding the slot cdata gets a loop-carried
            // PHI (it reads live at the loop header snapshot), which pins the
            // pointer cdata so LuaJIT cannot sink it (24 B/elem); a block local is
            // dead at the header, so it sinks to 0 -- matching rock. See the slot
            // cluster method.
            if let Some(consumed) = self.try_emit_nb_slot_cluster(out, bb, id, &insts[i + 1..]) {
                i += 1 + consumed;
                continue;
            }
            self.emit_inst(out, id);
            i += 1;
        }
        // A goto into an un-emitted counted-loop header becomes a structured
        // `for` (the preheader falls through into it); everything else is a goto.
        let term = &self.func.blocks[bb].term;
        if let Term::Goto(t) = term {
            let t = *t;
            if self.loops.contains_key(&t) && !self.emitted[raw(t) as usize] {
                self.expand_for(out, t);
                return;
            }
        }
        self.emit_term(out, &self.func.blocks[bb].term);
    }

    /// If `slot` is a non-inlinable `Index{NativeBuffer}` whose ONLY uses are the
    /// field stores immediately following it (a `buf[i] = ValueStruct{..}` lowered
    /// by `scalarize_native_buffer_stores`), emit it as a `do … end` cluster with
    /// a BLOCK-scoped local for the slot, and return how many following stores it
    /// consumed. Else `None` (the slot is emitted normally).
    ///
    /// Why: skull holds the slot in a function-level register, which LuaJIT keeps
    /// live at the loop-header snapshot -> a loop-carried PHI on the `buf.ptr[i]`
    /// pointer cdata -> it cannot sink (24 B/element). rock declares the slot as a
    /// fresh `local` inside the loop body, dead at the header, so it sinks to 0. A
    /// block-scoped local reproduces that.
    fn try_emit_nb_slot_cluster(
        &self,
        out: &mut String,
        bb: Idx<Block>,
        slot: Idx<Inst>,
        following: &[Idx<Inst>],
    ) -> Option<usize> {
        if self.inlinable[raw(slot) as usize]
            || !matches!(
                &self.func.insts[slot],
                Inst::Index { kind: ContainerKind::NativeBuffer, .. }
            )
        {
            return None;
        }
        // Consecutive field stores into the slot. Even a SINGLE store keeps the
        // pointer cdata live across the iteration in a function register (-> a
        // loop-carried PHI that doesn't sink, 24 B/iter); block-scoping fixes any
        // count >= 1.
        let n = following
            .iter()
            .take_while(|&&fi| matches!(&self.func.insts[fi], Inst::StoreField { base, .. } if *base == slot))
            .count();
        if n == 0 {
            return None;
        }
        // The slot must have NO use after the cluster (later insts or the term) --
        // a block-scoped local would be out of scope there.
        let mut escapes = false;
        for &fi in &following[n..] {
            regalloc::inst_uses(&self.func.insts[fi], &mut |u| escapes |= u == slot);
        }
        regalloc::term_uses(&self.func.blocks[bb].term, &mut |u| escapes |= u == slot);
        if escapes {
            return None;
        }
        let lname = format!("__nbslot{}", raw(slot));
        out.push_str("  do\n");
        let _ = write!(out, "    local {lname} = ");
        self.write_value(out, slot, &self.func.insts[slot]);
        out.push('\n');
        for &fi in &following[..n] {
            if let Inst::StoreField { field, value, .. } = &self.func.insts[fi] {
                let _ = write!(out, "    {lname}.{field} = ");
                self.write_opnd(out, *value);
                out.push('\n');
            }
        }
        out.push_str("  end\n");
        Some(n)
    }

    /// Emit a recognized counted loop as `for <slot> = <slot>, hi[-1] do .. end`.
    /// The init reads the slot cell (the preheader's store set it to `lo`), so it
    /// never depends on `lo`'s register being live at the loop head. The header's
    /// compare/branch and the latch's increment are subsumed by the `for`; the
    /// latch keeps only its label so a body `goto bb<latch>` (continue / normal
    /// fall-through) lands at the loop tail and re-iterates.
    fn expand_for(&mut self, out: &mut String, header: Idx<Block>) {
        let cl = &self.loops[&header];
        let (slot, inclusive, body_entry, latch, exit) =
            (cl.slot, cl.inclusive, cl.body_entry, cl.latch, cl.exit);
        let region: HashSet<Idx<Block>> = cl.region.iter().copied().collect();
        let slot_name = cellname(self.frame, self.cell_colors, self.boxed, slot);
        let hi = match &cl.hi {
            HiLimit::Value(v) => self.opnd_str(*v),
            HiLimit::Cell(c) => cellname(self.frame, self.cell_colors, self.boxed, *c),
        };
        let limit = if inclusive { hi } else { format!("{hi} - 1") };

        self.emitted[raw(header) as usize] = true;
        let _ = writeln!(out, "  for {slot_name} = {slot_name}, {limit} do");
        self.emit_block(out, body_entry);
        for &bb in self.rpo {
            if region.contains(&bb) {
                self.emit_block(out, bb); // self-skips body_entry / already-emitted
            }
        }
        // Latch label only (the `for` does the increment); then close + the
        // loop-done edge (header's false branch).
        self.emitted[raw(latch) as usize] = true;
        out.push_str("  ::bb");
        push_uint(out, raw(latch));
        out.push_str("::\n");
        out.push_str("  end\n");
        out.push_str("  goto bb");
        push_uint(out, raw(exit));
        out.push('\n');
    }

    fn emit_inst(&mut self, out: &mut String, id: Idx<Inst>) {
        // A tree-folded value emits NO statement; its expression is rendered at
        // its single use site by `opnd`.
        if self.inlinable[raw(id) as usize] {
            return;
        }
        // A hoisted const ctor emits NO statement -- its value is the module-scope
        // global, referenced via `write_opnd`. (Skips the dead `rN = ctor` a
        // multi-use hoisted const would otherwise materialize.)
        if self.hoist.contains_key(&id) {
            return;
        }
        // An `@inline inline(lua)` call is spliced inline (body in the caller's
        // scope) instead of emitting a `callee(args)` call -- see the method.
        if matches!(&self.func.insts[id], Inst::Call { .. })
            && self.try_emit_inline_lua_splice(out, id)
        {
            return;
        }
        let inst = &self.func.insts[id];
        if inst.produces_value() {
            out.push_str("  ");
            write_vname(out, &self.colors, id);
            out.push_str(" = ");
            self.write_value(out, id, inst);
            out.push('\n');
        } else {
            self.write_effect(out, inst);
        }
    }

    /// Splice an `@inline inline(lua)` `Call` inline, if `id` is one. Returns
    /// true iff it spliced; else the caller emits the normal `callee(args)`.
    fn try_emit_inline_lua_splice(&self, out: &mut String, id: Idx<Inst>) -> bool {
        let Inst::Call { func, args } = &self.func.insts[id] else {
            return false;
        };
        let callee = func_of(self.ws, *func);
        self.splice_inline_lua(out, callee, args, id)
    }

    /// Emit `callee`'s `inline(lua)` body inline at this invocation -- a `do … end`
    /// block: each param bound to a `local` = its arg, a tail `return X` rewritten
    /// to `<result> = X`. Returns true iff spliced (false leaves the call to the
    /// caller). Splicing keeps any cdata the body produces in the CALLER's scope
    /// so LuaJIT can sink it; a call boundary forces it to materialize.
    ///
    /// Conservative: only `@inline`, plain-param inline(lua) (no `@scalar_abi`
    /// leaf expansion) whose body is a single bare expression, a statement-shaped
    /// unit body, or a single TAIL `return`. Early / multiple returns fall back to
    /// a call -- rewriting those line-by-line would drop the control flow.
    fn splice_inline_lua(
        &self,
        out: &mut String,
        callee: &Function,
        args: &[Idx<Inst>],
        result: Idx<Inst>,
    ) -> bool {
        let Some(body) = callee.lua_body else {
            return false;
        };
        let ann = |n: &str| callee.annotations.iter().any(|a| a.name.name.as_str() == n);
        if !ann("inline") || ann("scalar_abi") {
            return false;
        }
        if callee.param_decls.len() != args.len() {
            return false;
        }
        let Some(shape) = classify_inline_lua_body(body.as_str()) else {
            return false;
        };
        // A `Call`/`Index` value is never tree-folded here (`always_inlinable`
        // excludes them), so `result` has a register.
        let mut lv = String::new();
        write_vname(&mut lv, &self.colors, result);

        out.push_str("  do\n");
        for (p, a) in callee.param_decls.iter().zip(args.iter()) {
            let _ = write!(out, "    local {} = ", p.name);
            self.write_opnd(out, *a);
            out.push('\n');
        }
        render_spliced_inline_lua(out, body.as_str(), shape, &lv);
        out.push_str("  end\n");
        true
    }

    /// Render a reference to value `id` as an operand directly into `out`: its
    /// register `r<color>`, or — if it was tree-folded — its expression in
    /// parentheses, recursively. Every value-operand reference goes through here.
    fn write_opnd(&self, out: &mut String, id: Idx<Inst>) {
        // A hoisted all-constant value ctor renders as its module-scope global,
        // whether it would otherwise inline or materialize a register.
        if let Some(&g) = self.hoist.get(&id) {
            out.push_str(&self.hoist_names[g as usize]);
            return;
        }
        if self.inlinable[raw(id) as usize] {
            // Always parenthesize the folded expression so it composes safely in
            // the parent context regardless of precedence/adjacency. (Notably a
            // bare negative literal under unary `-` would form `--` — a Lua
            // comment — without the wrap.)
            out.push('(');
            self.write_value(out, id, &self.func.insts[id]);
            out.push(')');
        } else {
            write_vname(out, &self.colors, id);
        }
    }

    /// Collect the flat operand list of a `StringConcat` tree, descending into
    /// child `StringConcat`s that are inlined here (single-use, folded into this
    /// expression). A non-inlinable child stays a leaf -- it is a separate `..`
    /// already materialized in its own cell, so flattening past it would be wrong.
    fn collect_concat(&self, parts: &mut Vec<Idx<Inst>>, id: Idx<Inst>) {
        if self.inlinable[raw(id) as usize] {
            if let Inst::StringConcat(a, b) = &self.func.insts[id] {
                self.collect_concat(parts, *a);
                self.collect_concat(parts, *b);
                return;
            }
        }
        parts.push(id);
    }

    /// `write_opnd` into a fresh `String` — for the few cold sites that need an
    /// owned operand string (loop-limit arithmetic in `expand_for`).
    fn opnd_str(&self, id: Idx<Inst>) -> String {
        let mut s = String::new();
        self.write_opnd(&mut s, id);
        s
    }

    /// The RHS expression for a value-producing instruction.
    fn write_value(&self, out: &mut String, id: Idx<Inst>, inst: &Inst) {
        match inst {
            Inst::Const(l) => write_lit(out, l),
            Inst::Global(g) => out.push_str(g.as_str()),
            Inst::Load(c) => write_cellname(out, self.frame, self.cell_colors, self.boxed, *c),

            Inst::IntAdd(a, b) | Inst::FloatAdd(a, b) => self.write_bin(out, "+", a, b),
            Inst::IntSub(a, b) | Inst::FloatSub(a, b) => self.write_bin(out, "-", a, b),
            Inst::IntMul(a, b) | Inst::FloatMul(a, b) => self.write_bin(out, "*", a, b),
            // int `/` is floor-division (rock semantics); float `/` is real.
            Inst::IntDiv(a, b) => {
                out.push_str("math.floor(");
                self.write_opnd(out, *a);
                out.push_str(" / ");
                self.write_opnd(out, *b);
                out.push(')');
            }
            Inst::FloatDiv(a, b) => self.write_bin(out, "/", a, b),
            Inst::IntMod(a, b) | Inst::FloatMod(a, b) => self.write_bin(out, "%", a, b),
            Inst::StringConcat(a, b) => {
                // Emit the whole inlinable concat tree as ONE flat `..` run
                // (`x .. y .. z`) rather than left-nested binary concats
                // (`((x .. y) .. z)`). LuaJIT's BC_CAT folds a flat run into a
                // SINGLE string allocation; nested binary `..` allocates an
                // intermediate per step (interpolation is ~2x the allocations
                // otherwise -- the dominant skull-vs-rock HUD/text alloc gap).
                let mut parts = Vec::new();
                self.collect_concat(&mut parts, *a);
                self.collect_concat(&mut parts, *b);
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        out.push_str(" .. ");
                    }
                    self.write_opnd(out, *p);
                }
            }
            Inst::ToString(x) => {
                out.push_str("tostring(");
                self.write_opnd(out, *x);
                out.push(')');
            }
            Inst::Format { value, fmt } => {
                out.push_str("string.format(");
                write_lua_string(out, fmt.as_str());
                out.push_str(", ");
                self.write_opnd(out, *value);
                out.push(')');
            }
            Inst::FormatDyn { value, fmt } => {
                out.push_str("string.format(");
                self.write_opnd(out, *fmt);
                out.push_str(", ");
                self.write_opnd(out, *value);
                out.push(')');
            }
            Inst::FloatToInt(x) => {
                out.push_str("math.floor(");
                self.write_opnd(out, *x);
                out.push(')');
            }

            Inst::IntLt(a, b) | Inst::FloatLt(a, b) => self.write_bin(out, "<", a, b),
            Inst::IntLeq(a, b) | Inst::FloatLeq(a, b) => self.write_bin(out, "<=", a, b),
            Inst::IntGt(a, b) | Inst::FloatGt(a, b) => self.write_bin(out, ">", a, b),
            Inst::IntGeq(a, b) | Inst::FloatGeq(a, b) => self.write_bin(out, ">=", a, b),
            Inst::Eq(a, b) => self.write_bin(out, "==", a, b),
            Inst::NotEq(a, b) => self.write_bin(out, "~=", a, b),

            Inst::Neg(x) => {
                out.push('-');
                self.write_opnd(out, *x);
            }
            Inst::Not(x) => {
                out.push_str("not ");
                self.write_opnd(out, *x);
            }
            Inst::BitNot(x) => {
                out.push_str("bit.bnot(");
                self.write_opnd(out, *x);
                out.push(')');
            }

            Inst::BitAnd(a, b) => self.write_bitfn(out, "bit.band", a, b),
            Inst::BitOr(a, b) => self.write_bitfn(out, "bit.bor", a, b),
            Inst::BitXor(a, b) => self.write_bitfn(out, "bit.bxor", a, b),
            Inst::Shl(a, b) => self.write_bitfn(out, "bit.lshift", a, b),
            Inst::Shr(a, b) => self.write_bitfn(out, "bit.rshift", a, b),

            Inst::Call { func, args } => {
                let callee = mangle::mangle_fn(self.ws, func_of(self.ws, *func), &self.ctx.overloaded);
                out.push_str(&callee);
                out.push('(');
                self.write_csv(out, args);
                out.push(')');
            }
            Inst::CallValue { callee, args } => {
                self.write_opnd(out, *callee);
                out.push('(');
                self.write_csv(out, args);
                out.push(')');
            }
            Inst::Lambda { params, entry } => {
                // P3: a capture-free lambda is a hoisted top-level fn -- reference
                // it by global name (a raw Lua function value, zero alloc). Its
                // body is appended as a sibling def by `emit_named_framed`.
                if let Some(name) = self.plan.and_then(|p| p.hoisted_name.get(&id)) {
                    out.push_str(name);
                    return;
                }
                // Inline `function(<params>) <body> end`. The body is rendered as
                // its own scope (independent value coloring); captured cells are
                // upvalues declared in the enclosing function.
                //
                // Snapshot captured cells by value (an IIFE whose params shadow
                // them) ONLY when they need it -- a loop-stored single assignment,
                // so a closure created in the loop captures THIS iteration's value.
                // A cell stored once outside every loop never changes after its
                // assignment, so it captures by reference with no IIFE (the hot
                // deferred-draw closures). `snap_cells` is precomputed per scope
                // (empty in a coroutine frame). See `snapshot_eligible_cells`.
                let mut snap: Vec<Idx<Cell>> = lambda_captured_cells(self.func, *entry, params)
                    .into_iter()
                    .filter(|c| self.snap_cells.contains(c))
                    .collect();
                snap.sort_by_key(|c| raw(*c));
                if snap.is_empty() {
                    out.push_str("function");
                    render_scope(self.ws, self.ctx, self.func, *entry, params, self.lambda_params, false, self.home, self.plan, self.frame, self.cell_colors, self.boxed, self.hoist, self.hoist_names, out);
                } else {
                    let argl = snap.iter().map(|c| cell_local(self.cell_colors, *c)).collect::<Vec<_>>().join(", ");
                    let _ = write!(out, "(function({argl}) return function");
                    render_scope(self.ws, self.ctx, self.func, *entry, params, self.lambda_params, false, self.home, self.plan, self.frame, self.cell_colors, self.boxed, self.hoist, self.hoist_names, out);
                    let _ = write!(out, " end)({argl})");
                }
            }

            Inst::MakeTuple(vs) | Inst::MakeArray(vs) => {
                out.push('{');
                self.write_csv(out, vs);
                out.push('}');
            }
            Inst::MakeVec(vs) => {
                out.push_str("{_data = {");
                self.write_csv(out, vs);
                let _ = write!(out, "}}, len = {}, __ty = \"Vec\"}}", vs.len());
            }
            Inst::MakeStruct { ty, fields } => {
                out.push('{');
                let mut first = true;
                if let Some(name) = ty {
                    out.push_str("__ty = ");
                    write_lua_string(out, name.as_str());
                    first = false;
                }
                for (n, id) in fields {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    let _ = write!(out, "{n} = ");
                    self.write_opnd(out, *id);
                }
                out.push('}');
            }
            Inst::MakeValueStruct { name, fields } => {
                // Positional ctor in declared field order; field names are for MIR
                // passes only (SROA), not the `<name>_ct(...)` wire shape.
                let _ = write!(out, "{name}_ct(");
                for (i, (_, id)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.write_opnd(out, *id);
                }
                out.push(')');
            }
            Inst::MakeEnum {
                tag,
                ty,
                positional,
                named,
            } => {
                out.push_str("{__variant = ");
                write_lua_string(out, tag.as_str());
                out.push_str(", __ty = ");
                write_lua_string(out, ty.as_str());
                for id in positional {
                    out.push_str(", ");
                    self.write_opnd(out, *id);
                }
                for (n, id) in named {
                    let _ = write!(out, ", {n} = ");
                    self.write_opnd(out, *id);
                }
                out.push('}');
            }
            Inst::MakeEnumDyn { tag, ty, positional, named } => {
                out.push_str("{__variant = ");
                self.write_opnd(out, *tag);
                out.push_str(", __ty = ");
                write_lua_string(out, ty.as_str());
                for id in positional {
                    out.push_str(", ");
                    self.write_opnd(out, *id);
                }
                for (n, id) in named {
                    let _ = write!(out, ", {n} = ");
                    self.write_opnd(out, *id);
                }
                out.push('}');
            }

            Inst::Field { base, field } => {
                self.write_opnd(out, *base);
                let _ = write!(out, ".{field}");
            }
            Inst::TupleIndex { base, index } => {
                self.write_opnd(out, *base);
                out.push('[');
                push_uint(out, (index + 1) as u32);
                out.push(']');
            }

            Inst::Index { base, index, kind } => self.write_index(out, *base, *index, *kind),
            Inst::Len { base, kind } => match kind {
                ContainerKind::Vec | ContainerKind::NativeBuffer => {
                    self.write_opnd(out, *base);
                    out.push_str(".len");
                }
                ContainerKind::NativeArray => {
                    out.push('#');
                    self.write_opnd(out, *base);
                }
                ContainerKind::Map => {
                    // No O(1) length on a Map; lowering shouldn't emit this kind.
                    out.push('#');
                    self.write_opnd(out, *base);
                }
            },

            // Effects don't produce values.
            Inst::Store { .. }
            | Inst::StoreField { .. }
            | Inst::StoreIndex { .. }
            | Inst::StoreGlobal { .. }
            | Inst::MapNext { .. }
            | Inst::CallMulti { .. }
            | Inst::CallValueMulti { .. }
            | Inst::CoroSetInner { .. }
            | Inst::CoroHook { .. }
            | Inst::ArrayPush { .. } => unreachable!("effect has no value"),

            // Substituted by the inliner (or rejected by mir_emit's residual
            // check) long before codegen.
            Inst::CallBoundMulti { .. } => {
                unreachable!("placeholder bound-call must be substituted before emit")
            }
        }
    }

    /// `a <op> b` written directly into `out` (no per-operand allocation).
    fn write_bin(&self, out: &mut String, op: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        self.write_opnd(out, *a);
        out.push(' ');
        out.push_str(op);
        out.push(' ');
        self.write_opnd(out, *b);
    }

    /// `f(a, b)` (bit-library call shape) written directly into `out`.
    fn write_bitfn(&self, out: &mut String, f: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push_str(f);
        out.push('(');
        self.write_opnd(out, *a);
        out.push_str(", ");
        self.write_opnd(out, *b);
        out.push(')');
    }

    /// READ index `base[index]`. NativeArray/NativeBuffer go through the
    /// debug-mode bounds-checked stdlib helpers (`__narr_get_checked` /
    /// `__nb_checked_get`) — matching the old backend (skull runs debug; release
    /// stripping is future release-mode work). Vec/Map index raw.
    fn write_index(&self, out: &mut String, base: Idx<Inst>, index: Idx<Inst>, kind: ContainerKind) {
        match kind {
            ContainerKind::Vec => {
                self.write_opnd(out, base);
                out.push_str("._data[");
                self.write_opnd(out, index);
                out.push_str(" + 1]");
            }
            ContainerKind::NativeArray => {
                out.push_str("__narr_get_checked(");
                self.write_opnd(out, base);
                out.push_str(", ");
                self.write_opnd(out, index);
                out.push(')');
            }
            ContainerKind::NativeBuffer => {
                out.push_str("__nb_checked_get(");
                self.write_opnd(out, base);
                out.push_str(", ");
                self.write_opnd(out, index);
                out.push(')');
            }
            ContainerKind::Map => {
                self.write_opnd(out, base);
                out.push('[');
                self.write_opnd(out, index);
                out.push(']');
            }
        }
    }

    fn write_effect(&self, out: &mut String, inst: &Inst) {
        match inst {
            Inst::Store { cell, value } => {
                out.push_str("  ");
                write_cellname(out, self.frame, self.cell_colors, self.boxed, *cell);
                out.push_str(" = ");
                self.write_opnd(out, *value);
                out.push('\n');
            }
            Inst::StoreField { base, field, value } => {
                out.push_str("  ");
                self.write_opnd(out, *base);
                let _ = write!(out, ".{field} = ");
                self.write_opnd(out, *value);
                out.push('\n');
            }
            Inst::StoreIndex {
                base,
                index,
                value,
                kind,
            } => {
                match kind {
                    // NativeBuffer write -> debug-mode bounds-checked helper.
                    ContainerKind::NativeBuffer => {
                        out.push_str("  __nb_checked_set(");
                        self.write_opnd(out, *base);
                        out.push_str(", ");
                        self.write_opnd(out, *index);
                        out.push_str(", ");
                        self.write_opnd(out, *value);
                        out.push_str(")\n");
                    }
                    ContainerKind::Vec => {
                        out.push_str("  ");
                        self.write_opnd(out, *base);
                        out.push_str("._data[");
                        self.write_opnd(out, *index);
                        out.push_str(" + 1] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                    ContainerKind::NativeArray => {
                        out.push_str("  ");
                        self.write_opnd(out, *base);
                        out.push('[');
                        self.write_opnd(out, *index);
                        out.push_str(" + 1] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                    ContainerKind::Map => {
                        out.push_str("  ");
                        self.write_opnd(out, *base);
                        out.push('[');
                        self.write_opnd(out, *index);
                        out.push_str("] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                }
            }
            Inst::StoreGlobal { name, value } => {
                let _ = write!(out, "  {name} = ");
                self.write_opnd(out, *value);
                out.push('\n');
            }
            Inst::ArrayPush { arr, value, kind } => {
                // `arr` is referenced up to 4x; render it once.
                let a = self.opnd_str(*arr);
                match kind {
                    ContainerKind::Vec => {
                        // increment len BEFORE assign (1-indexed data table).
                        let _ = writeln!(out, "  {a}.len = {a}.len + 1");
                        let _ = write!(out, "  {a}._data[{a}.len] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                    _ => {
                        let _ = write!(out, "  {a}[#{a} + 1] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                }
            }
            Inst::MapNext {
                key_cell,
                val_cell,
                table,
            } => {
                // `key_cell` is referenced twice; render it once.
                let k = cellname(self.frame, self.cell_colors, self.boxed, *key_cell);
                let _ = write!(out, "  {k}, ");
                write_cellname(out, self.frame, self.cell_colors, self.boxed, *val_cell);
                let _ = write!(out, " = next(");
                self.write_opnd(out, *table);
                let _ = writeln!(out, ", {k})");
            }
            // Scalar-ABI multi-return: `d0, d1, ... = callee(args)`. The callee is
            // a value-returning fn whose return is scalarized to N leaf values.
            Inst::CallMulti { dests, func, args } => {
                out.push_str("  ");
                for (i, c) in dests.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_cellname(out, self.frame, self.cell_colors, self.boxed, *c);
                }
                let callee = mangle::mangle_fn(self.ws, func_of(self.ws, *func), &self.ctx.overloaded);
                let _ = write!(out, " = {callee}(");
                self.write_csv(out, args);
                out.push_str(")\n");
            }
            // Indirect multi-return: `d0, d1, ... = callee(args)` -- a tuple
            // returned through a fn-value. Multi-assign so Lua doesn't truncate.
            Inst::CallValueMulti { dests, callee, args } => {
                out.push_str("  ");
                for (i, c) in dests.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_cellname(out, self.frame, self.cell_colors, self.boxed, *c);
                }
                out.push_str(" = ");
                self.write_opnd(out, *callee);
                out.push('(');
                self.write_csv(out, args);
                out.push_str(")\n");
            }
            // Rewritten to `s.__inner = ..` by the coroutine transform; if it
            // reaches codegen the transform didn't run (a bug).
            Inst::CoroSetInner(_) => unreachable!("CoroSetInner must be removed by the coroutine transform"),
            // Relocated to the constructor by the coroutine transform.
            Inst::CoroHook { .. } => unreachable!("CoroHook must be relocated by the coroutine transform"),
            _ => unreachable!("not an effect"),
        }
    }

    fn emit_term(&self, out: &mut String, term: &Term) {
        match term {
            // luajit rule 2: `return` wrapped `do ... end` so it is never the
            // block's mid-sequence statement (and is legal as the last one too).
            Term::Return(vals) if !vals.is_empty() => {
                out.push_str("  do return ");
                self.write_csv(out, vals);
                out.push_str(" end\n");
            }
            Term::Return(_) | Term::Unreachable => {
                out.push_str("  do return end\n");
            }
            Term::Goto(b) => {
                out.push_str("  goto bb");
                push_uint(out, raw(*b));
                out.push('\n');
            }
            Term::Branch {
                cond,
                then_bb,
                else_bb,
            } => {
                out.push_str("  if ");
                self.write_opnd(out, *cond);
                out.push_str(" then goto bb");
                push_uint(out, raw(*then_bb));
                out.push_str(" else goto bb");
                push_uint(out, raw(*else_bb));
                out.push_str(" end\n");
            }
            Term::Switch {
                discr,
                cases,
                default,
            } => {
                // `discr` is compared in every case arm; render it once.
                let d = self.opnd_str(*discr);
                let mut first = true;
                for (l, b) in cases {
                    let kw = if first { "if" } else { "elseif" };
                    first = false;
                    let _ = write!(out, "  {kw} {d} == ");
                    write_lit(out, l);
                    out.push_str(" then goto bb");
                    push_uint(out, raw(*b));
                    out.push('\n');
                }
                if first {
                    out.push_str("  goto bb");
                    push_uint(out, raw(*default));
                    out.push('\n');
                } else {
                    out.push_str("  else goto bb");
                    push_uint(out, raw(*default));
                    out.push_str(" end\n");
                }
            }
            Term::Yield { .. } => {
                unreachable!("Yield must be removed by the coroutine transform before codegen")
            }
        }
    }

    fn write_csv(&self, out: &mut String, ids: &[Idx<Inst>]) {
        for (i, id) in ids.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            self.write_opnd(out, *id);
        }
    }
}

fn write_lit(out: &mut String, l: &Lit) {
    match l {
        Lit::Int(i) => {
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
    }
}

/// Escape `s` as a double-quoted Lua string literal (also used by the workspace
/// emitter for inline(lua) default-value rendering).
pub fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_lua_string(&mut out, s);
    out
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
