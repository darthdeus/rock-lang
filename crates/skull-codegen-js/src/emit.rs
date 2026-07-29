//! Minimal MIR->JS emitter (docs/js-backend-plan.md S2) for the conformance
//! subset: the scalar fallback bodies of the eight differential intrinsics,
//! plus the NativeBuffer four-fn surface (TypedArray-backed `{ptr, len}`
//! structs, checked `buf[i]` via the `__nb_checked_*` shim names --
//! `crate::JS_NB_CHECK_SHIMS` supplies the bodies). Structured (goto-free) output modeled on the GLSL backend's
//! `emit_seq`/`emit_branch`, NOT the goto-linear Lua backend -- JS, like GLSL,
//! has no goto, so a `Branch` recovers into `if/else` and its arms rejoin at the
//! block's immediate post-dominator. Any MIR op outside the subset is a spanned
//! `CompilerError` (reject, don't emit-and-hope); the subset grows with the
//! backend.

use std::collections::{BTreeSet, HashMap, HashSet};

use bone_semantic::hir::{TyId, TypeCtor, TypeExpr};
use bone_semantic::inference::Workspace;
use comfy_la_arena::Idx;
use skull_mir::intrinsics::{Emission, IntrinsicTable};
use skull_mir::mir::{Block, Cell, ContainerKind, FuncRef, Inst, Lit, MirFunc, Term};
use skull_parser::{CompilerError, Span};

use crate::structure::{self, LoopRegion, Structure};
use crate::{fill_template, JsIntrinsics};

fn raw<T>(idx: Idx<T>) -> u32 {
    idx.into_raw().into()
}

/// The module-global surface an emitted body may reference through `Inst::Global`
/// / `Inst::StoreGlobal`. Module-level `let`/`global` STATE renders into a scoped
/// `__G` object (`__G.<name>`) so no user global can collide with or be shadowed
/// by an emitted body local (`r<N>`/`v<N>`/`c<N>`/`__rock_env`); a top-level fn
/// referenced as a VALUE renders as its co-emitted JS name (a bare function, so
/// `__rock_call` invokes it directly). Built once per program in
/// [`emit_with_closure`]; empty for the standalone [`emit_fn`] entry.
#[derive(Default)]
pub struct GlobalCtx {
    /// Names that are module-level STATE (-> `__G.<name>`).
    state: HashSet<ustr::Ustr>,
    /// A top-level fn used as a value -> its co-emitted JS name.
    fns: HashMap<ustr::Ustr, String>,
    /// `__rock_enum_<E>_<V>` singletons / `__rock_enum_<E>_names` tables the body
    /// reads (rendered as a bare top-level name; the def is emitted per program).
    enum_globals: HashSet<ustr::Ustr>,
}

fn err(msg: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(Span::default(), msg.into())
}

/// The one rejection for every column-storage site, so the reason reads the same
/// whichever one a program reaches first.
///
/// Only a `nobox` element gets here: an UNMARKED one keeps the boxed
/// representation on this target (`skull_mir::lower::columns_available`), so
/// reaching a column form means the program asked for the guarantee on a backend
/// that cannot serve it. Naming the element and the marker is the difference
/// between a diagnosis and a skip-histogram line.
fn column_reject(elem: ustr::Ustr) -> String {
    format!(
        "JS subset: `[{elem}]` stores as columns because `{elem}` is `nobox`, and column \
         storage has no JS emitter arm yet"
    )
}

/// A `(message, site)` rejection from a MIR-level check, anchored at the
/// offending instruction when the check could name one.
fn sited_err((msg, site): (String, Option<Span>)) -> CompilerError {
    CompilerError::new_with_label(site.unwrap_or_default(), msg)
}

/// Every `nobox` materialization in one body, as one error carrying the rest as
/// siblings. Mirrors the luajit path: a body with several sinks reports several
/// findings, so neither backend's count of what is left is a floor.
fn nobox_errs(
    ws: &bone_semantic::inference::Workspace,
    mf: &skull_mir::mir::MirFunc,
) -> Result<(), CompilerError> {
    let mut vs = skull_transform::nobox_materialization_violations(ws, mf).into_iter();
    let Some(first) = vs.next() else { return Ok(()) };
    let mut e = sited_err(first);
    e.siblings = vs.map(sited_err).collect();
    Err(e)
}

fn nobox_nominal_ty(ws: &Workspace, ty: TyId) -> Option<ustr::Ustr> {
    let te = match ty {
        TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
        TyId::Local(_) => return None,
    };
    match te {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } => {
            let sid = ws.struct_by_name.get(&name)?;
            ws.modules[sid.module.index()].structs[sid.def]
                .is_nobox
                .then_some(name)
        }
        TypeExpr::Nullable(inner) => nobox_nominal_ty(ws, ty.with_idx(inner)),
        _ => None,
    }
}

fn validate_js_nobox_return(
    ws: &Workspace,
    f: &bone_semantic::hir::Function,
    mf: &MirFunc,
) -> Result<(), CompilerError> {
    let Some(name) = f.return_ty.and_then(|ty| nobox_nominal_ty(ws, ty)) else {
        return Ok(());
    };
    let packs_return = mf
        .blocks
        .iter()
        .any(|(_, block)| matches!(&block.term, Term::Return(values) if values.len() > 1));
    if packs_return {
        return Err(err(format!(
            "JS subset: nobox value `{name}` return requires an implicit array materialization"
        )));
    }
    Ok(())
}

/// A cell's JS identifier (`c<raw>`). Params and locals share the scheme; params
/// materialize in the fn arg list, other stored cells as hoisted `let`s.
fn cell_name<T>(cell: Idx<T>) -> String {
    format!("c{}", raw(cell))
}

/// A finite-float JS numeric literal, or a reject for inf/NaN (outside the
/// subset). `f64::to_string` already renders `3.0` as `3` and `0.5` as `0.5`,
/// both valid JS.
fn js_number(f: f64) -> Result<String, CompilerError> {
    if f.is_finite() {
        Ok(f.to_string())
    } else {
        Err(err("JS subset: non-finite float literal"))
    }
}

/// A JS double-quoted string literal escaping the control chars a JS lexer
/// needs. Non-ASCII chars encode as `\u` escapes (UTF-16 code units, the same
/// scheme the driver's `js_str` uses) -- JS string LENGTH then counts UTF-16
/// units where Lua counts bytes, a known cross-backend divergence for
/// non-ASCII content; rejecting outright bricked whole game builds over an
/// em-dash in a log message.
fn js_string_lit(s: &str) -> Result<String, CompilerError> {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if c.is_ascii() && !c.is_ascii_control() => o.push(c),
            c => {
                for u in c.encode_utf16(&mut [0u16; 2]) {
                    o.push_str(&format!("\\u{:04x}", u));
                }
            }
        }
    }
    o.push('"');
    Ok(o)
}

/// A field name that is not a bare JS identifier -- flattened enum/embed slots
/// are dotted physical keys (`active.#`, `active.0`) -- must be quoted in an
/// object literal and bracketed in a member access.
fn is_js_ident(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// `f` as an object-literal key: bare when it is an identifier, quoted otherwise.
fn js_key(f: &str) -> Result<String, CompilerError> {
    if is_js_ident(f) { Ok(f.to_string()) } else { js_string_lit(f) }
}

/// `base.f` / `base["f"]` -- property access that survives a non-identifier key.
fn js_member(base: &str, f: &str) -> Result<String, CompilerError> {
    if is_js_ident(f) {
        Ok(format!("{base}.{f}"))
    } else {
        Ok(format!("{base}[{}]", js_string_lit(f)?))
    }
}

/// Emit `mf` as `function <js_name>(...) { ... }`. `js_name` is the caller's
/// chosen JS identity (the S3/S4 harness names each fallback fn).
pub fn emit_fn(ws: &Workspace, mf: &MirFunc, js_name: &str) -> Result<String, CompilerError> {
    emit_fn_with_callees(ws, mf, js_name, &HashMap::new(), &HashSet::new(), &GlobalCtx::default())
}

/// [`emit_fn`] with a set of known co-emitted plain-Rock callees: a call whose
/// `FuncRef` is in `callees` renders as `<js name>(args)` (the caller promises
/// to emit that fn under that name in the same program). Anything neither a
/// mapped intrinsic nor a known callee still rejects.
pub fn emit_fn_with_callees(
    ws: &Workspace,
    mf: &MirFunc,
    js_name: &str,
    callees: &HashMap<FuncRef, String>,
    hoisted: &HashSet<ustr::Ustr>,
    globals: &GlobalCtx,
) -> Result<String, CompilerError> {
    if mf.is_coroutine {
        return Err(err("JS subset: coroutine fn not supported"));
    }
    let (use_count, inlinable) = compute_inlinable(mf);
    let st = structure::analyze(mf);
    // A range-`for` (`loop_binding` slot + pure-increment latch) emits as a real
    // JS `for (; slot < bound; slot = slot + 1)`: `continue` there re-runs the
    // increment, which a `while (true)` shape cannot express (a bare `continue`
    // would skip it) -- and multiple `continue`s share the latch, so a while
    // shape would walk into it repeatedly. Every other natural loop takes the
    // general `while (true)` path, whose header effects re-materialize through
    // the value model so a cell read+written across the back-edge does not fold.
    let mut counted: HashMap<Idx<Block>, LoopInfo> = HashMap::new();
    let mut general: HashMap<Idx<Block>, LoopRegion> = HashMap::new();
    for (&h, region) in &st.loops {
        match match_counted_loop(mf, h) {
            Some(li) if li.exit == region.exit && region.body.contains(&li.latch) => {
                counted.insert(h, li);
            }
            _ => {
                general.insert(h, region.clone());
            }
        }
    }
    let mut em = Emitter {
        ws,
        mf,
        callees,
        hoisted,
        globals,
        use_count,
        inlinable,
        structure: st,
        counted,
        general,
        active: HashSet::new(),
        visited: HashSet::new(),
        loop_stack: Vec::new(),
        loop_markers: Vec::new(),
        labels_used: HashSet::new(),
        shared_tail_reject: false,
        pack_multi_return: false,
        hoisted_temps: HashSet::new(),
        nbv_bypass: compute_nbv_bypass(mf, &reachable_blocks(mf)),
        out: String::new(),
        indent: 1,
    };

    let params: Vec<String> = mf.params.iter().map(|&c| cell_name(c)).collect();
    let mut header = format!("function {js_name}({}) {{\n", params.join(", "));

    // Hoist declarations for every stored cell that is not a param (JS block
    // scope would otherwise not carry a cell written in one arm and read in the
    // merge -- the `sign` shape).
    let param_set: HashSet<u32> = mf.params.iter().map(|&c| raw(c)).collect();
    let mut declared: Vec<u32> = Vec::new();
    let mut seen = HashSet::new();
    // Only cells stored in REACHABLE blocks -- a dead block's `store` never
    // emits, so declaring its cell would leave an unused `let`.
    for &b in &reachable_blocks(mf) {
        for &iid in &mf.blocks[b].insts {
            for cell in inst_writes(&mf.insts[iid]) {
                if !param_set.contains(&raw(cell)) && seen.insert(raw(cell)) {
                    declared.push(raw(cell));
                }
            }
        }
    }
    if !declared.is_empty() {
        // Declare in cell-id order (stable, traversal-independent output).
        declared.sort_unstable();
        let names: Vec<String> = declared.iter().map(|id| format!("c{id}")).collect();
        header.push_str(&format!("  let {};\n", names.join(", ")));
    }

    match em.emit_seq(mf.entry, None, LoopCtx::default()) {
        Ok(()) => {
            let body = format!("{header}{}}}\n", em.out);
            Ok(finalize_labels(body, &em.loop_markers, &em.labels_used))
        }
        // The structured relooper cannot express this CFG (a shared branch tail
        // holding a nested loop, or one over the tail-dup cap). Re-emit the SAME
        // body through the block trampoline, which accepts arbitrary (incl.
        // irreducible) CFGs. Emission is pure/idempotent per call, so the
        // discarded structured attempt leaves no residue. Every OTHER reject
        // rides `?` out untouched (`shared_tail_reject` is set only at the one
        // reject site), and a trampoline reject surfaces its own reason.
        Err(_) if em.shared_tail_reject => {
            emit_fn_trampoline(ws, mf, js_name, callees, hoisted, globals, mf.entry, &mf.params, true)
        }
        Err(e) => Err(e),
    }
}

/// Emit a region of `mf` as a block trampoline `for(;;) switch(__bb)` -- the
/// coroutine-step shape (dispatch-into-loops is irreducible by construction, so
/// the structured relooper cannot apply). `entry` is the region root (the step
/// entry, or a detached hook/defer region's block); `params` the emitted fn's
/// param cells (the step's `[s, resume_arg]`, an aux body's `[s]`). Case bodies
/// are braced so `let v<N>` temps stay case-local; values are same-block by the
/// value model + the transform's cross-block spill, so no cross-case temps exist.
pub fn emit_fn_trampoline(
    ws: &Workspace,
    mf: &MirFunc,
    js_name: &str,
    callees: &HashMap<FuncRef, String>,
    hoisted: &HashSet<ustr::Ustr>,
    globals: &GlobalCtx,
    entry: Idx<Block>,
    params: &[Idx<Cell>],
    pack_multi_return: bool,
) -> Result<String, CompilerError> {
    let region = reachable_from(mf, entry);
    let (use_count, inlinable) = compute_inlinable_over(mf, &region);
    let hoisted_temps = cross_block_temps(mf, &region, &use_count, &inlinable);
    let mut em = Emitter {
        ws,
        mf,
        callees,
        hoisted,
        globals,
        use_count,
        inlinable,
        structure: structure::Structure::empty(mf.blocks.len()),
        counted: HashMap::new(),
        general: HashMap::new(),
        active: HashSet::new(),
        visited: HashSet::new(),
        loop_stack: Vec::new(),
        loop_markers: Vec::new(),
        labels_used: HashSet::new(),
        shared_tail_reject: false,
        pack_multi_return,
        hoisted_temps,
        nbv_bypass: compute_nbv_bypass(mf, &region),
        out: String::new(),
        indent: 1,
    };
    let pnames: Vec<String> = params.iter().map(|&c| cell_name(c)).collect();
    let mut header = format!("function {js_name}({}) {{\n", pnames.join(", "));
    let param_set: HashSet<u32> = params.iter().map(|&c| raw(c)).collect();
    let mut declared: Vec<u32> = Vec::new();
    let mut seen = HashSet::new();
    for &b in &region {
        for &iid in &mf.blocks[b].insts {
            for cell in inst_writes(&mf.insts[iid]) {
                if !param_set.contains(&raw(cell)) && seen.insert(raw(cell)) {
                    declared.push(raw(cell));
                }
            }
        }
    }
    let mut hoisted_names: Vec<u32> = em.hoisted_temps.iter().copied().collect();
    hoisted_names.sort_unstable();
    if !declared.is_empty() || !hoisted_names.is_empty() {
        declared.sort_unstable();
        let names: Vec<String> = declared
            .iter()
            .map(|id| format!("c{id}"))
            .chain(hoisted_names.iter().map(|id| format!("v{id}")))
            .collect();
        header.push_str(&format!("  let {};\n", names.join(", ")));
    }
    em.push(&format!("var __bb = {};", raw(entry)));
    em.push("for (;;) {");
    em.indent += 1;
    em.push("switch (__bb) {");
    em.indent += 1;
    let mut cases = region.clone();
    cases.sort_by_key(|b| raw(*b));
    for b in cases {
        em.push(&format!("case {}: {{", raw(b)));
        em.indent += 1;
        em.emit_block_effects(b)?;
        em.emit_trampoline_term(b)?;
        em.indent -= 1;
        em.push("}");
    }
    em.indent -= 1;
    em.push("}");
    em.indent -= 1;
    em.push("}");
    Ok(format!("{header}{}}}\n", em.out))
}

/// A unique, JS-invalid sentinel a loop emits in place of its label; resolved by
/// [`finalize_labels`] to `L<h>: ` if a multi-level jump targeted it, else stripped.
/// `\x01` cannot appear in emitted JS source, so the replace is unambiguous.
fn loop_marker(h: Idx<Block>) -> String {
    format!("\u{1}L{}\u{1}", raw(h))
}

/// Resolve loop-label sentinels: a targeted loop's marker becomes `L<h>: `; the
/// rest are stripped, keeping single-level loops byte-identical.
fn finalize_labels(mut s: String, markers: &[Idx<Block>], used: &HashSet<Idx<Block>>) -> String {
    for &h in markers {
        let m = loop_marker(h);
        let repl = if used.contains(&h) { format!("L{}: ", raw(h)) } else { String::new() };
        s = s.replace(&m, &repl);
    }
    s
}

struct Emitter<'a> {
    ws: &'a Workspace,
    mf: &'a MirFunc,
    /// Known co-emitted plain-Rock callees (`FuncRef` -> emitted JS name).
    callees: &'a HashMap<FuncRef, String>,
    /// Names of hoisted closure bodies (`convert_closures`' `<base>__lambda<k>`)
    /// emitted somewhere in this program: the ONLY `Inst::Global` a capture-free
    /// lambda creation site legitimately references. Any other global read (a
    /// module `let`, an enum name table, a named fn as a value) is out of subset.
    hoisted: &'a HashSet<ustr::Ustr>,
    /// Module-global surface: state globals (`__G.<name>`) and fn-value globals
    /// (co-emitted JS name). See [`GlobalCtx`].
    globals: &'a GlobalCtx,
    /// Times each inst is used as an operand (by an inst or terminator), dense
    /// by raw inst id. 0 == statement-position (an effectful `Call`) or dead.
    use_count: Vec<u32>,
    /// Whether each inst folds into its single use site (dense by raw id):
    /// pure, single-use, same-block. A non-inlinable USED value inst is bound to
    /// a `v<raw>` temp at its program position instead -- so a cell read (`Load`)
    /// captures the cell's value THEN, not at the (possibly-later) use, and an
    /// effectful `Call` keeps its order (the defer/increment miscompiles the
    /// all-inline model produced). Mirrors codegen-lua's `compute_inlinable`.
    inlinable: Vec<bool>,
    /// Dominators / immediate post-dominators / natural loops.
    structure: Structure,
    /// Range-`for` headers -> their counted-loop info (the `for (;;)` fast path).
    counted: HashMap<Idx<Block>, LoopInfo>,
    /// Other natural-loop headers -> their region (general `while (true)` path).
    general: HashMap<Idx<Block>, LoopRegion>,
    /// Loop headers currently being emitted, so the header-region walk (a
    /// compound `while a && b` condition) does not re-wrap the header as a fresh
    /// loop.
    active: HashSet<Idx<Block>>,
    visited: HashSet<Idx<Block>>,
    /// The stack of enclosing loops (innermost last), for multi-level `continue`/
    /// `break` to a non-innermost loop (`labeled_jump`).
    loop_stack: Vec<LoopFrame>,
    /// Every loop that emitted a `loop_marker` prefix, in emission order.
    loop_markers: Vec<Idx<Block>>,
    /// Loop headers a `labeled_jump` actually targeted -- these get a real
    /// `L<header>:` label at finalization; the rest have their marker stripped, so
    /// single-level loops stay byte-identical. Discovered DURING emission (the
    /// runtime `loop_stack` nesting), avoiding a fragile static pre-pass.
    labels_used: HashSet<Idx<Block>>,
    /// Set when [`Self::emit_seq`] raises the shared-branch-tail reject (the ONE
    /// reject the per-body trampoline fallback retries -- every other reject rides
    /// `?` out untouched). A typed signal, not a message-string compare.
    shared_tail_reject: bool,
    /// Whether a multi-value `Term::Return` in the trampoline printer packs an
    /// array (the ordinary-body fallback ABI, mirroring the structured
    /// `emit_return`) or rejects. A coroutine step's multi-value return keeps the
    /// reject -- its return channels through the step protocol, not a raw array.
    pack_multi_return: bool,
    /// Materialized `v<N>` temps (raw inst ids) whose use crosses their defining
    /// block: the flat trampoline switch puts each block in a SIBLING case, so a
    /// per-case `let v<N>` would be out of scope at a cross-case use. These are
    /// hoist-declared at the fn top and ASSIGNED in-case; block-local temps keep
    /// their per-case `let`. Empty for the structured path (block nesting keeps a
    /// temp in scope) and the spilled coroutine step (no cross-block temps).
    hoisted_temps: HashSet<u32>,
    /// Value-element NativeBuffer slot refs (raw inst ids) whose EVERY use is a
    /// `Field`/`StoreField` base: those consumers render flat-slot access from
    /// the ref's own operands, so the ref itself emits no statement (its Lua
    /// analogue is a cdata slot local; on JS there is no per-element view to
    /// bind). A ref with any OTHER use materializes as a whole-element COPY.
    nbv_bypass: HashSet<u32>,
    out: String,
    indent: usize,
}

/// The enclosing loop's jump targets, mirroring skull-mir's `loop_targets`:
/// `Goto(cont)` -> `continue`, `Goto(brk)` -> `break`.
#[derive(Clone, Copy, Default)]
struct LoopCtx {
    cont: Option<Idx<Block>>,
    brk: Option<Idx<Block>>,
}

/// One entry in the enclosing-loop stack (`Emitter::loop_stack`), for MULTI-LEVEL
/// jumps: a `Goto` to a non-innermost loop's `cont`/`brk` emits `continue L<h>;`/
/// `break L<h>;`. Arises from an inlined iterator `next()` whose skip-loop nests
/// inside the for-loop and `continue`s the OUTER loop. `header` names the JS label
/// (`L<header>`); the loop is labeled only if [`Emitter::needs_label`] holds.
#[derive(Clone, Copy)]
struct LoopFrame {
    cont: Idx<Block>,
    brk: Idx<Block>,
    header: Idx<Block>,
}

/// A recognized range-`for` loop (the shape `lower_for_range` emits).
#[derive(Clone, Copy)]
struct LoopInfo {
    slot: Idx<Cell>,
    bound: Idx<Inst>,
    inclusive: bool,
    body_entry: Idx<Block>,
    latch: Idx<Block>,
    exit: Idx<Block>,
}

impl Emitter<'_> {
    fn push(&mut self, line: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(line);
        self.out.push('\n');
    }

    /// A block that emits no statement and just forwards (`Some(target)`): an
    /// empty `if`/`else` arm.
    fn empty_forward(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        if let Term::Goto(t) = self.mf.blocks[b].term {
            if !self.mf.blocks[b].insts.iter().any(|&i| self.inst_emits(i)) {
                return Some(t);
            }
        }
        None
    }

    /// Whether inst `iid` produces a statement in [`Self::emit_block_effects`]:
    /// a store, a materialized (bound) value, or a statement-position call.
    /// Inlinable and dead-pure insts emit nothing.
    fn inst_emits(&self, iid: Idx<Inst>) -> bool {
        let inst = &self.mf.insts[iid];
        if self.nbv_bypass.contains(&raw(iid)) {
            return false;
        }
        if matches!(inst, Inst::Store { .. } | Inst::StoreIndex { .. }) {
            return true;
        }
        let k = raw(iid) as usize;
        if inst.produces_value() {
            (!self.inlinable[k] && self.use_count[k] > 0)
                || (matches!(inst, Inst::Call { .. } | Inst::CallValue { .. })
                    && self.use_count[k] == 0)
        } else {
            true // a non-value effect inst (rejected at emit if unsupported)
        }
    }

    fn is_nil(&self, iid: Idx<Inst>) -> bool {
        matches!(self.mf.insts[iid], Inst::Const(Lit::Nil))
    }

    fn emit_seq(
        &mut self,
        start: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<(), CompilerError> {
        let mut cur = start;
        loop {
            if Some(cur) == stop {
                return Ok(());
            }
            // Entering the enclosing loop's exit block IS a break: a `loop {}`
            // whose `break` is a branch arm directly into the exit (no `Goto(exit)`
            // edge to catch below) relies on this. Checked before the visited mark
            // so the exit stays available for the post-loop emission the header's
            // dispatcher resumes at.
            if Some(cur) == ctx.brk {
                self.push("break;");
                return Ok(());
            }
            // A natural-loop header is dispatched structurally (before the
            // visited guard): `emit_general` re-enters `emit_seq` on the header
            // for its condition region, so the header must not be pre-marked.
            if !self.active.contains(&cur) {
                if let Some(li) = self.counted.get(&cur).copied() {
                    self.emit_counted(cur, li)?;
                    cur = li.exit;
                    continue;
                }
                if let Some(region) = self.general.get(&cur).cloned() {
                    self.emit_general(&region, ctx)?;
                    cur = region.exit;
                    continue;
                }
            }
            if !self.visited.insert(cur) {
                // Natural loops are dispatched above, so a revisit here is a
                // reducible-but-unstructured shape. A FORWARD shared tail -- a
                // block reached from 2+ branch arms that is not the branch's ipdom
                // (an or-pattern success arm, a multiple-`continue` latch tail) --
                // has no back-edge through it, so re-emitting it inline (tail
                // DUPLICATION) terminates and is semantically identical: each
                // dynamic execution follows exactly one copy. Bounded by `stop`,
                // small, and lambda-/loop-free (`dup_region`). An enclosing active
                // loop header (a multi-level jump) is NOT a forward tail -- that is
                // the labeled-`continue` path in the `Goto` arm below, and if it
                // reaches here it is genuinely unstructured. Anything else rejects.
                if !self.active.contains(&cur) {
                    if let Some(region) = self.dup_region(cur, stop, ctx) {
                        return self.emit_dup(cur, stop, ctx, &region);
                    }
                }
                self.shared_tail_reject = true;
                return Err(err("JS subset: unstructured control flow (shared branch tail)"));
            }
            self.emit_block_effects(cur)?;
            match self.mf.blocks[cur].term.clone() {
                Term::Return(vs) => {
                    self.emit_return(&vs)?;
                    return Ok(());
                }
                Term::Unreachable => return Ok(()),
                Term::Goto(t) => {
                    if Some(t) == ctx.cont {
                        self.push("continue;");
                        return Ok(());
                    }
                    if Some(t) == ctx.brk {
                        self.push("break;");
                        return Ok(());
                    }
                    if Some(t) == stop {
                        return Ok(());
                    }
                    // A `Goto` to an ENCLOSING (non-innermost) loop's header/exit:
                    // a labeled `continue L<h>;`/`break L<h>;` (the inlined iterator
                    // next() skip-loop that continues the outer for-loop).
                    if let Some(stmt) = self.labeled_jump(t) {
                        self.push(&stmt);
                        return Ok(());
                    }
                    cur = t;
                }
                Term::Branch { cond, then_bb, else_bb } => {
                    match self.emit_branch(cur, cond, then_bb, else_bb, stop, ctx)? {
                        Some(m) => cur = m,
                        None => return Ok(()),
                    }
                }
                Term::Switch { .. } => return Err(err("JS subset: `match`/switch not supported")),
                Term::Yield { .. } => return Err(err("JS subset: `yield` not supported")),
            }
        }
    }

    /// The set of blocks a forward shared tail `start` spans up to `stop` (the
    /// merge the duplicated copies rejoin at), or `None` if duplicating it would
    /// be unsound or too large. Boundaries: `stop`, and the enclosing loop's
    /// `cont`/`brk` (a `Goto` there is a `continue`/`break`, not a region block).
    /// Rejected regions (kept as an honest shared-tail reject):
    /// - larger than 8 blocks / 32 insts (a pathological blow-up),
    /// - a block that is a registered loop header (a back-edge -> non-terminating
    ///   re-walk),
    /// - a materialized (`let v<N>`) region value read from OUTSIDE the region
    ///   (a temp that would be undefined in the second copy's scope).
    fn dup_region(
        &self,
        start: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Option<HashSet<Idx<Block>>> {
        const MAX_BLOCKS: usize = 8;
        const MAX_INSTS: usize = 32;
        let boundary = |b: Idx<Block>| Some(b) == stop || Some(b) == ctx.cont || Some(b) == ctx.brk;
        let mut region: HashSet<Idx<Block>> = HashSet::new();
        let mut insts = 0usize;
        let mut stack = vec![start];
        while let Some(b) = stack.pop() {
            if boundary(b) || !region.insert(b) {
                continue;
            }
            if region.len() > MAX_BLOCKS {
                return None;
            }
            if self.counted.contains_key(&b) || self.general.contains_key(&b) {
                return None;
            }
            insts += self.mf.blocks[b].insts.len();
            if insts > MAX_INSTS {
                return None;
            }
            skull_mir::cfg::successors(&self.mf.blocks[b].term, |t| {
                if !boundary(t) {
                    stack.push(t);
                }
            });
        }
        if self.region_temp_escapes(&region) {
            return None;
        }
        Some(region)
    }

    /// Whether a `let v<N>` materialized inside `region` is used by any inst or
    /// terminator OUTSIDE it. Cross-block values normally route through hoisted
    /// cells (the `sign` shape), so this should never fire -- but a temp that DID
    /// escape would be undefined in the duplicated copy's block scope, so the guard
    /// makes duplication sound regardless of the value model's guarantees.
    fn region_temp_escapes(&self, region: &HashSet<Idx<Block>>) -> bool {
        let mut mat: HashSet<Idx<Inst>> = HashSet::new();
        for &b in region {
            for &iid in &self.mf.blocks[b].insts {
                let k = raw(iid) as usize;
                if self.mf.insts[iid].produces_value()
                    && !self.inlinable[k]
                    && self.use_count[k] > 0
                {
                    mat.insert(iid);
                }
            }
        }
        if mat.is_empty() {
            return false;
        }
        for (b, blk) in self.mf.blocks.iter() {
            if region.contains(&b) {
                continue;
            }
            let mut hit = false;
            for &iid in &blk.insts {
                for_each_operand(&self.mf.insts[iid], &mut |op| hit |= mat.contains(&op));
            }
            for_each_term_operand(&blk.term, &mut |op| hit |= mat.contains(&op));
            if hit {
                return true;
            }
        }
        false
    }

    /// Re-emit the forward shared-tail region `start..stop` inline (tail
    /// duplication). A self-contained re-walk that does NOT consult/mark
    /// `visited` (the whole point is to duplicate already-emitted blocks); it
    /// terminates because `dup_region` proved the region loop-free and bounded by
    /// `stop`. Its `let v<N>` temps live in the caller's fresh arm scope, so a
    /// same-named temp from the first copy does not collide.
    fn emit_dup(
        &mut self,
        start: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
        region: &HashSet<Idx<Block>>,
    ) -> Result<(), CompilerError> {
        let mut cur = start;
        loop {
            if Some(cur) == stop {
                return Ok(());
            }
            if Some(cur) == ctx.brk {
                self.push("break;");
                return Ok(());
            }
            self.emit_block_effects(cur)?;
            match self.mf.blocks[cur].term.clone() {
                Term::Return(vs) => {
                    self.emit_return(&vs)?;
                    return Ok(());
                }
                Term::Unreachable => return Ok(()),
                Term::Goto(t) => {
                    if Some(t) == ctx.cont {
                        self.push("continue;");
                        return Ok(());
                    }
                    if Some(t) == ctx.brk {
                        self.push("break;");
                        return Ok(());
                    }
                    if Some(t) == stop {
                        return Ok(());
                    }
                    if let Some(stmt) = self.labeled_jump(t) {
                        self.push(&stmt);
                        return Ok(());
                    }
                    cur = t;
                }
                Term::Branch { cond, then_bb, else_bb } => {
                    let cond_s = self.render_val(cond)?;
                    let merge = self
                        .structure
                        .ipdom(cur)
                        .filter(|&m| Some(m) != ctx.cont && Some(m) != ctx.brk && region.contains(&m));
                    let arm_stop = merge.map(Some).unwrap_or(stop);
                    self.push(&format!("if ({cond_s}) {{"));
                    self.indent += 1;
                    self.emit_dup(then_bb, arm_stop, ctx, region)?;
                    self.indent -= 1;
                    self.push("} else {");
                    self.indent += 1;
                    self.emit_dup(else_bb, arm_stop, ctx, region)?;
                    self.indent -= 1;
                    self.push("}");
                    match merge {
                        Some(m) => cur = m,
                        None => return Ok(()),
                    }
                }
                Term::Switch { .. } => return Err(err("JS subset: `match`/switch not supported")),
                Term::Yield { .. } => return Err(err("JS subset: `yield` not supported")),
            }
        }
    }

    /// A `Branch` block whose insts are all pure, non-erroring, side-effect-free
    /// (`Store`/`Call`/`Index`-bounds-check excluded), so its condition can be
    /// folded into an enclosing `&&`/`||` -- any statement it materializes (a
    /// tuple/struct projection) is hoisted before the `if` and computed
    /// unconditionally, which is observably identical for a pure read. Returns the
    /// branch's `(cond, then, else)`.
    fn foldable_branch(&self, b: Idx<Block>) -> Option<(Idx<Inst>, Idx<Block>, Idx<Block>)> {
        let Term::Branch { cond, then_bb, else_bb } = self.mf.blocks[b].term else {
            return None;
        };
        let pure = self.mf.blocks[b].insts.iter().all(|&i| {
            matches!(
                &self.mf.insts[i],
                Inst::Load(_) | Inst::TupleIndex { .. } | Inst::Field { .. } | Inst::Len { .. }
            ) || always_inlinable(&self.mf.insts[i])
        });
        pure.then_some((cond, then_bb, else_bb))
    }

    /// Emit an `if`-diamond. Returns the join to continue at, or `None` when the
    /// arms diverge (the join is the virtual exit) and there is no fall-through.
    fn emit_branch(
        &mut self,
        blk: Idx<Block>,
        cond: Idx<Inst>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<Option<Idx<Block>>, CompilerError> {
        // Short-circuit fold: a `match` range/tuple pattern lowers to nested
        // branches whose failure arms share one tail block (`if A { if B { X }
        // else { S } } else { S }`) -- reducible but not a natural loop, so the
        // relooper cannot structure the doubly-reached `S`. Recognizing the chain
        // (`A && B`; the `||` dual for a shared success arm) collapses it to a
        // clean diamond that rejoins at the ipost-dominator. A folded inner
        // block's pure projections hoist before the `if`.
        let (cond_s, then_bb, else_bb) = {
            let mut cond_s = self.render_val(cond)?;
            let (mut then_bb, mut else_bb) = (then_bb, else_bb);
            loop {
                if let Some((c2, t2, e2)) = self.foldable_branch(then_bb) {
                    if e2 == else_bb {
                        self.emit_block_effects(then_bb)?;
                        self.visited.insert(then_bb);
                        cond_s = format!("({cond_s} && {})", self.render_val(c2)?);
                        then_bb = t2;
                        continue;
                    }
                }
                if let Some((c2, t2, e2)) = self.foldable_branch(else_bb) {
                    if t2 == then_bb {
                        self.emit_block_effects(else_bb)?;
                        self.visited.insert(else_bb);
                        cond_s = format!("({cond_s} || {})", self.render_val(c2)?);
                        else_bb = e2;
                        continue;
                    }
                }
                break (cond_s, then_bb, else_bb);
            }
        };
        // Empty else -> then-only `if`; continue at the else target. Valid ONLY
        // when the empty arm forwards to the branch's actual merge (the FILTERED
        // ipdom `merge` below): the shortcut stops the live arm at `j` and resumes
        // there, correct only if that arm also reconverges at `j`. Two ways `j`
        // (the empty arm's forward target) can differ from the real merge:
        //   - the live arm merges LATER, past an empty arm that forwards to an
        //     intermediate block (the inlined bounds-check
        //     `if oob { r = nil } else { r = data[i] }` shape) -- `j` is not the
        //     join, so both arms would independently reach the merge and
        //     double-visit it;
        //   - `j` is a loop back/exit edge (`ctx.cont`/`ctx.brk`) -- a `Goto`
        //     there must lower to `continue;`/`break;` (which the full-diamond
        //     path does via `emit_seq`'s terminator handling), not fall through to
        //     the header and re-enter it.
        // The empty-arm shortcut (`if (cond) { live_arm }`, resume at the empty
        // arm's forward target `j`) is sound when either:
        //   - the branch has a real forward merge and `j` IS it (`merge ==
        //     Some(j)`) -- both arms rejoin at `j`; or
        //   - the branch has NO forward merge (`merge` is None): both arms diverge
        //     (a `break`/`continue`/`return`), so the empty arm's forward is a
        //     genuine divergence (e.g. `if (i == 3) { break }` in a `for`, whose
        //     empty else falls through to `continue`) and the live arm diverges
        //     too -- there is no shared tail to double-visit.
        // It is UNSOUND when the branch HAS a forward merge but `j` is a plain
        // intermediate block the live arm skips PAST to reach it (the inlined
        // bounds-check `if oob { r = nil } else { r = data[i] }` tail: `j` is not
        // the join). And in all cases `j` must not be the ACTIVE header -- resuming
        // at a `loop {}`'s own header (its `continue` target) re-enters it.
        let merge = self
            .structure
            .ipdom(blk)
            .filter(|&m| Some(m) != ctx.cont && Some(m) != ctx.brk);
        let forward_ok = |j: Idx<Block>| {
            (merge == Some(j) || merge.is_none()) && !self.active.contains(&j)
        };
        if let Some(j) = self.empty_forward(else_bb).filter(|&j| forward_ok(j)) {
            self.push(&format!("if ({cond_s}) {{"));
            self.indent += 1;
            self.emit_seq(then_bb, Some(j), ctx)?;
            self.indent -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        // Empty then -> invert and emit the else arm only (same forward guard).
        if let Some(j) = self.empty_forward(then_bb).filter(|&j| forward_ok(j)) {
            self.push(&format!("if (!({cond_s})) {{"));
            self.indent += 1;
            self.emit_seq(else_bb, Some(j), ctx)?;
            self.indent -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        // Full diamond. The merge is ipdom, unless an arm diverges (ipdom
        // collapses to the virtual exit or a loop target) -- then the arms absorb
        // the continuation up to the enclosing stop and there is no fall-through.
        let arm_stop = merge.map(Some).unwrap_or(stop);
        self.push(&format!("if ({cond_s}) {{"));
        self.indent += 1;
        self.emit_seq(then_bb, arm_stop, ctx)?;
        self.indent -= 1;
        self.push("} else {");
        self.indent += 1;
        self.emit_seq(else_bb, arm_stop, ctx)?;
        self.indent -= 1;
        self.push("}");
        Ok(merge)
    }


    /// A range-`for`: `for (; slot < bound; slot = slot + 1) { body }`. The slot
    /// and bound were initialized in the preheader (hoisted cells), so the `for`
    /// init clause is empty. `continue` targets the latch (the `for` increment
    /// runs it); `break` targets the exit.
    fn emit_counted(&mut self, header: Idx<Block>, li: LoopInfo) -> Result<(), CompilerError> {
        let slot = cell_name(li.slot);
        // The bound is a `Load` of a hoisted preheader cell (or a const),
        // rendered inline so it never depends on a header-block temp we skip.
        let bound = self.render_inst_expr(li.bound)?;
        let cmp = if li.inclusive { "<=" } else { "<" };
        self.loop_markers.push(header);
        let mark = loop_marker(header);
        self.push(&format!("{mark}for (; {slot} {cmp} {bound}; {slot} = {slot} + 1) {{"));
        self.indent += 1;
        let ctx = LoopCtx { cont: Some(li.latch), brk: Some(li.exit) };
        self.loop_stack.push(LoopFrame { cont: li.latch, brk: li.exit, header });
        let body_len = self.out.len();
        self.emit_seq(li.body_entry, None, ctx)?;
        self.trim_trailing_continue(body_len);
        self.loop_stack.pop();
        self.indent -= 1;
        self.push("}");
        Ok(())
    }

    /// The JS statement for a `Goto(t)` that is a `continue`/`break` of an
    /// enclosing loop: bare for the innermost, labeled (`continue L<h>;`) for an
    /// outer one (a multi-level jump), recording that header so `finalize_labels`
    /// materializes its `L<h>:`. `None` if `t` is no enclosing loop target.
    fn labeled_jump(&mut self, t: Idx<Block>) -> Option<String> {
        let innermost = self.loop_stack.last().map(|f| f.header);
        let hit = self
            .loop_stack
            .iter()
            .rev()
            .find_map(|f| (t == f.cont).then_some((f.header, true)).or((t == f.brk).then_some((f.header, false))))?;
        let (header, is_cont) = hit;
        let bare = Some(header) == innermost;
        if bare {
            return Some(if is_cont { "continue;".to_string() } else { "break;".to_string() });
        }
        self.labels_used.insert(header);
        Some(if is_cont {
            format!("continue L{};", raw(header))
        } else {
            format!("break L{};", raw(header))
        })
    }

    /// A general loop. Two shapes:
    /// - `while cond`/`for` (`region.test` set): `while (true) { <cond region>;
    ///   if (!(cond)) break; <body> }`. The condition region may be a multi-block
    ///   diamond (`while a && b`), so it goes through the structurizer recursively.
    /// - `loop {}` (`region.test` None): a headerless `while (true) { <body> }`;
    ///   the loop's `break`s and `continue`s fall out of the body's own branch
    ///   structure (a `Goto(exit)` -> `break`, a `Goto(header)` -> `continue`, and
    ///   a branch arm ENTERING the exit block breaks via the `cur == brk` guard in
    ///   [`Self::emit_seq`]). Reached when a `continue` precedes the `break`, so no
    ///   single test dominates the back-edges.
    fn emit_general(&mut self, region: &LoopRegion, outer: LoopCtx) -> Result<(), CompilerError> {
        let h = region.header;
        let exit = region.exit;
        let ctx = LoopCtx { cont: Some(h), brk: Some(exit) };

        self.active.insert(h);
        self.loop_markers.push(h);
        let mark = loop_marker(h);
        self.push(&format!("{mark}while (true) {{"));
        self.indent += 1;
        self.loop_stack.push(LoopFrame { cont: h, brk: exit, header: h });

        if let Some(test) = region.test {
            let (cond, body_start, break_when_true) = self.test_arms(test, exit)?;
            self.emit_seq(h, Some(test), outer)?;
            self.emit_block_effects(test)?;
            self.visited.insert(test);
            let cond_s = self.render_val(cond)?;
            if break_when_true {
                self.push(&format!("if ({cond_s}) {{ break; }}"));
            } else {
                self.push(&format!("if (!({cond_s})) {{ break; }}"));
            }
            let body_len = self.out.len();
            self.emit_seq(body_start, None, ctx)?;
            self.trim_trailing_continue(body_len);
        } else {
            let body_len = self.out.len();
            self.emit_seq(h, None, ctx)?;
            self.trim_trailing_continue(body_len);
        }

        self.loop_stack.pop();
        self.indent -= 1;
        self.push("}");
        self.active.remove(&h);
        Ok(())
    }

    /// Decompose the `while cond`/`for` header test `test` (a `Branch` with one
    /// arm directly to `exit`): returns `(cond, body entry, break-when-cond-true)`.
    fn test_arms(
        &self,
        test: Idx<Block>,
        exit: Idx<Block>,
    ) -> Result<(Idx<Inst>, Idx<Block>, bool), CompilerError> {
        if let Term::Branch { cond, then_bb, else_bb } = self.mf.blocks[test].term {
            if then_bb == exit {
                return Ok((cond, else_bb, true));
            }
            if else_bb == exit {
                return Ok((cond, then_bb, false));
            }
        }
        Err(err("JS subset: unstructured loop (condition test has no direct exit arm)"))
    }

    /// Drop a redundant trailing `continue;` that is the loop body's natural
    /// back-edge fall-through (indented exactly at the body level).
    fn trim_trailing_continue(&mut self, start: usize) {
        let line = format!("{}continue;\n", "  ".repeat(self.indent));
        if self.out.len() >= start + line.len() && self.out.ends_with(&line) {
            let cut = self.out.len() - line.len();
            if cut >= start && (cut == 0 || self.out.as_bytes()[cut - 1] == b'\n') {
                self.out.truncate(cut);
            }
        }
    }

    /// A multi-value assignment `[d0, d1, ...] = <call>;` (a single dest is a
    /// plain `d0 = <call>;` -- the scalar ABI returns one leaf as a scalar, not a
    /// one-element array, matching [`Self::emit_return`]).
    fn multi_assign(&self, dests: &[Idx<Cell>], call: &str) -> String {
        match dests {
            [d] => format!("{} = {call};", cell_name(*d)),
            _ => {
                let names: Vec<String> = dests.iter().map(|&d| cell_name(d)).collect();
                format!("[{}] = {call};", names.join(", "))
            }
        }
    }

    fn emit_block_effects(&mut self, b: Idx<Block>) -> Result<(), CompilerError> {
        for iid in self.mf.blocks[b].insts.clone() {
            match &self.mf.insts[iid] {
                Inst::Store { cell, value } => {
                    let rhs = self.render_val(*value)?;
                    let line = format!("{} = {};", cell_name(*cell), rhs);
                    self.push(&line);
                }
                Inst::StoreIndex { base, index, value, kind } => {
                    let (b, i, v) = (
                        self.render_val(*base)?,
                        self.render_val(*index)?,
                        self.render_val(*value)?,
                    );
                    let line = match kind {
                        ContainerKind::NativeBuffer => format!("__nb_checked_set({b}, {i}, {v});"),
                        // Column storage has no JS emitter arm yet -- reject so
                        // the case lands in the SKIP histogram (the emitter
                        // worklist) instead of emitting a wrong single-slot write.
                        ContainerKind::VecValueColumns(elem) => {
                            return Err(err(column_reject(*elem)));
                        }
                        ContainerKind::VecColumn => {
                            return Err(err("JS subset: column-stored [T] not supported"));
                        }
                        ContainerKind::NativeBufferLeaf => {
                            return Err(err("JS subset: NativeBuffer leaf views not supported"));
                        }
                        // Whole value-struct element write: per-leaf stores into
                        // the flat backing from the value object (a MakeValueStruct
                        // renders as an object literal; a copy read is an object).
                        ContainerKind::NativeBufferValue(elem) => {
                            self.nbv_write_stmt(&b, &i, &v, *elem, 0, *elem)?
                        }
                        ContainerKind::Vec => format!("__vec_checked_set({b}, {i}, {v});"),
                        ContainerKind::Map => format!("{b}.set({i}, {v});"),
                    };
                    self.push(&line);
                }
                // `k, v = next(t, k)` -- one step of Map iteration. JS has no
                // `next`, so a cursor-cached shim (`__map_next`, JS_MAP_ITER_RUNTIME)
                // returns the next key in the native Map's INSERTION order (null at
                // exhaustion; Map order is backend-defined by doctrine). The value
                // side reads `t.get(k)` -- no per-step pair allocation. The `table`
                // operand renders to a Load/Global/Field (idempotent) or a
                // materialized `v<N>` temp, so re-rendering it is safe.
                Inst::MapNext { key_cell, val_cell, table } => {
                    let t = self.render_val(*table)?;
                    let k = cell_name(*key_cell);
                    let v = cell_name(*val_cell);
                    self.push(&format!("{k} = __map_next({t}, {k});"));
                    self.push(&format!("{v} = ({k} === null ? null : {t}.get({k}));"));
                }
                // `base.field = value` (struct field / cursor mutation). A base
                // that projects a value-element NativeBuffer slot (the scalarized
                // element-store shape) writes the flat backing instead: a float
                // leaf via `__nbv_set`, a nested value-struct field via the
                // per-leaf write IIFE (matching Lua's whole-cdata field assign).
                Inst::StoreField { base, field, value } => {
                    if let Some((b_iid, i_iid, root, off, cur)) = self.nbv_proj(*base)? {
                        let (k, fields) = self.nbv_layout(cur)?;
                        let _ = k;
                        let f = fields.iter().find(|f| f.name == *field).ok_or_else(|| {
                            err(format!("JS subset: value struct `{cur}` has no field `{field}`"))
                        })?;
                        let (root_k, _) = self.nbv_layout(root)?;
                        let b = self.render_val(b_iid)?;
                        let i = self.render_val(i_iid)?;
                        let v = self.render_val(*value)?;
                        let line = match f.nested {
                            None => format!(
                                "__nbv_set({b}, {i}, {root_k}, {}, {v});",
                                off + f.offset
                            ),
                            Some(n) => self.nbv_write_stmt(&b, &i, &v, root, off + f.offset, n)?,
                        };
                        self.push(&line);
                        continue;
                    }
                    let line = format!(
                        "{} = {};",
                        js_member(&self.render_val(*base)?, field)?,
                        self.render_val(*value)?
                    );
                    self.push(&line);
                }
                // A call in statement position (value unused): `nb_set(b, i, v)`
                // or a closure invoked for effect, `(f)(x);`.
                Inst::Call { .. } | Inst::CallValue { .. }
                    if self.use_count[raw(iid) as usize] == 0 =>
                {
                    let line = format!("{};", self.render_inst_expr(iid)?);
                    self.push(&line);
                }
                // A multi-value call (scalarized value-struct / tuple return):
                // `[d0, d1, ...] = callee(args);` destructures the array the
                // multi-return callee packs. Dest cells are hoist-declared at the
                // fn top, so this is an assignment, not a `let`. The leaves are
                // read back via `Load` of each dest.
                Inst::CallMulti { dests, func, args } => {
                    let call = self.render_call(*func, args)?;
                    self.push(&self.multi_assign(dests, &call));
                }
                Inst::CallValueMulti { dests, callee, args } => {
                    let mut rendered = vec![self.render_val(*callee)?];
                    for a in args {
                        rendered.push(self.render_val(*a)?);
                    }
                    let call = format!("__rock_call({})", rendered.join(", "));
                    self.push(&self.multi_assign(dests, &call));
                }
                // Module-level `let`/`global` state write: `__G.<name> = value;`
                // (scoped so no body local can collide). The name must be in the
                // program's state set (`emit_with_closure` registers it before
                // emit); an unregistered write is a backend bug, not a skip.
                Inst::StoreGlobal { name, value } => {
                    if !self.globals.state.contains(name) {
                        return Err(err(format!(
                            "JS subset: write to unregistered global `{name}`"
                        )));
                    }
                    let v = self.render_val(*value)?;
                    self.push(&format!("__G.{name} = {v};"));
                }
                // A checked Vec/NativeBuffer index whose value is UNUSED: its
                // bounds check throws on OOB, so it must be emitted as a statement
                // rather than dropped (luajit keeps the checked read -- dropping
                // it would silently lose the OOB error, diverging under
                // `try_call(|| xs[oob])`). Map index is a raw miss-returns-nil VM
                // op with no side effect, so it stays droppable below.
                Inst::Index { kind, .. }
                    if (matches!(kind, ContainerKind::Vec) || kind.is_native_buffer())
                        && self.use_count[raw(iid) as usize] == 0 =>
                {
                    let line = format!("{};", self.render_inst_expr(iid)?);
                    self.push(&line);
                }
                // A value-element slot ref consumed entirely by Field/StoreField
                // renders through its consumers -- no statement here (the copy
                // materialize would be dead work per scalarized element store).
                Inst::Index { kind: ContainerKind::NativeBufferValue(_), .. }
                    if self.nbv_bypass.contains(&raw(iid)) => {}
                Inst::Field { .. } if self.nbv_bypass.contains(&raw(iid)) => {}
                // A leaf VIEW is a luajit-representation artifact: an `ffi.cast`
                // pointer over the element storage, consumed only by the buffer
                // literal's view fields, which JS drops (it backs a NativeBuffer
                // with a TypedArray and has no pointer views). Nothing on JS can
                // read one -- a path that needed leaf views hits the
                // `NativeBufferLeaf` reject -- so it emits no statement.
                Inst::FfiViewCast { .. } => {}
                // A used, non-inlinable value inst: materialize it at its
                // program position as `let v<raw> = <expr>` so cell reads capture
                // their value here and effectful calls keep their order. Its use
                // sites read the temp via `render_val`.
                other if other.produces_value()
                    && !self.inlinable[raw(iid) as usize]
                    && self.use_count[raw(iid) as usize] > 0 =>
                {
                    let expr = self.render_inst_expr(iid)?;
                    // A cross-case temp is hoist-declared at the fn top (see
                    // `hoisted_temps`); assign, don't re-`let`, so it stays live
                    // across the flat switch cases. Block-local temps `let` here.
                    if self.hoisted_temps.contains(&raw(iid)) {
                        self.push(&format!("v{} = {};", raw(iid), expr));
                    } else {
                        self.push(&format!("let v{} = {};", raw(iid), expr));
                    }
                }
                // An inlinable value inst renders at its use; an unused pure inst
                // (dead `Load`/`Index`/`Field`/`Len`) is dropped. Remaining
                // EFFECT insts (StoreField, StoreGlobal, ArrayPush, MapNext,
                // CallMulti, coroutine ops, ...) are outside the subset.
                other if other.produces_value() => {}
                other => {
                    return Err(err(format!(
                        "JS subset: unsupported effect {other:?}"
                    )))
                }
            }
        }
        Ok(())
    }

    fn emit_return(&mut self, vs: &[Idx<Inst>]) -> Result<(), CompilerError> {
        match vs {
            [] => self.push("return;"),
            [v] => {
                let s = self.render_val(*v)?;
                self.push(&format!("return {s};"));
            }
            // The scalar ABI returns a scalarized value struct / tuple as N leaf
            // values; JS has no multi-return, so pack them into an array the
            // `CallMulti` caller destructures.
            _ => {
                let parts: Result<Vec<String>, CompilerError> =
                    vs.iter().map(|&v| self.render_val(v)).collect();
                self.push(&format!("return [{}];", parts?.join(", ")));
            }
        }
        Ok(())
    }

    /// A trampoline case's terminator: `__bb` reassignment + `continue` for
    /// in-region edges, `return` for exits. `Switch` is a literal if-chain (the
    /// discriminant is a temp or a pure inlinable, so re-rendering is safe).
    fn emit_trampoline_term(&mut self, b: Idx<Block>) -> Result<(), CompilerError> {
        match self.mf.blocks[b].term.clone() {
            Term::Goto(t) => self.push(&format!("__bb = {}; continue;", raw(t))),
            Term::Branch { cond, then_bb, else_bb } => {
                let c = self.render_val(cond)?;
                self.push(&format!(
                    "if ({c}) {{ __bb = {}; }} else {{ __bb = {}; }} continue;",
                    raw(then_bb),
                    raw(else_bb)
                ));
            }
            Term::Switch { discr, cases, default } => {
                let d = self.render_val(discr)?;
                let mut s = String::new();
                for (lit, bb) in &cases {
                    let cmp = match lit {
                        Lit::Nil => format!("{d} == null"),
                        Lit::Int(i) => format!("{d} === {i}"),
                        Lit::Bool(v) => format!("{d} === {v}"),
                        Lit::Float(f) => format!("{d} === {}", js_number(*f)?),
                        Lit::Str(v) => format!("{d} === {}", js_string_lit(v.as_str())?),
                    };
                    s.push_str(&format!("if ({cmp}) {{ __bb = {}; }} else ", raw(*bb)));
                }
                s.push_str(&format!("{{ __bb = {}; }} continue;", raw(default)));
                self.push(&s);
            }
            Term::Return(vs) => match vs.as_slice() {
                [] => self.push("return;"),
                [v] => {
                    let s = self.render_val(*v)?;
                    self.push(&format!("return {s};"));
                }
                // Scalarized value-struct / tuple return: pack the leaves into an
                // array the `CallMulti` caller destructures, mirroring the
                // structured `emit_return`. Only the ordinary-body fallback opts
                // in; a coroutine step's return channels through the step
                // protocol, so it keeps the reject.
                _ if self.pack_multi_return => {
                    let parts: Result<Vec<String>, CompilerError> =
                        vs.iter().map(|&v| self.render_val(v)).collect();
                    self.push(&format!("return [{}];", parts?.join(", ")));
                }
                _ => return Err(err("JS subset: multi-value coroutine return")),
            },
            Term::Unreachable => self.push("return;"),
            Term::Yield { .. } => {
                return Err(err(
                    "trampoline: Term::Yield must not survive transform_coroutine (pass bug)",
                ))
            }
        }
        Ok(())
    }

    /// The JS expression for a value operand: the bound `v<raw>` temp if the
    /// inst was materialized at its position, else its expression folded inline.
    fn render_val(&self, iid: Idx<Inst>) -> Result<String, CompilerError> {
        if self.inlinable[raw(iid) as usize] {
            self.render_inst_expr(iid)
        } else {
            Ok(format!("v{}", raw(iid)))
        }
    }

    /// Render an inst's own JS expression (its operands routed through
    /// [`Self::render_val`], so a non-inlinable operand reads its temp). JS is
    /// untyped, so the int/float split collapses to one operator and
    /// `IntToFloat` is identity.
    fn render_inst_expr(&self, iid: Idx<Inst>) -> Result<String, CompilerError> {
        let bin = |a: &Idx<Inst>, op: &str, b: &Idx<Inst>, s: &Self| -> Result<String, CompilerError> {
            Ok(format!("({} {} {})", s.render_val(*a)?, op, s.render_val(*b)?))
        };
        Ok(match &self.mf.insts[iid] {
            Inst::Const(Lit::Int(i)) => i.to_string(),
            Inst::Const(Lit::Float(f)) => js_number(*f)?,
            Inst::Const(Lit::Bool(b)) => b.to_string(),
            Inst::Const(Lit::Nil) => "null".to_string(),
            Inst::Const(Lit::Str(s)) => js_string_lit(s.as_str())?,
            Inst::Load(c) => cell_name(*c),
            Inst::IntAdd(a, b) | Inst::FloatAdd(a, b) => bin(a, "+", b, self)?,
            Inst::IntSub(a, b) | Inst::FloatSub(a, b) => bin(a, "-", b, self)?,
            Inst::IntMul(a, b) | Inst::FloatMul(a, b) => bin(a, "*", b, self)?,
            Inst::FloatDiv(a, b) => bin(a, "/", b, self)?,
            // Rock int `/` floors toward -inf (Lua); JS `/` is real division.
            Inst::IntDiv(a, b) => format!("Math.floor({} / {})", self.render_val(*a)?, self.render_val(*b)?),
            Inst::IntLt(a, b) | Inst::FloatLt(a, b) => bin(a, "<", b, self)?,
            Inst::IntLeq(a, b) | Inst::FloatLeq(a, b) => bin(a, "<=", b, self)?,
            Inst::IntGt(a, b) | Inst::FloatGt(a, b) => bin(a, ">", b, self)?,
            Inst::IntGeq(a, b) | Inst::FloatGeq(a, b) => bin(a, ">=", b, self)?,
            // Loose `== null` / `!= null` when either side is `nil`: it matches
            // JS `undefined` too, so an OMITTED defaulted arg (JS passes nothing
            // -> `undefined`, not `null`) still trips the callee's `x == nil`
            // default-fill guard, exactly as Lua's absent-arg `nil` does.
            Inst::Eq(a, b) if self.is_nil(*a) || self.is_nil(*b) => bin(a, "==", b, self)?,
            Inst::NotEq(a, b) if self.is_nil(*a) || self.is_nil(*b) => bin(a, "!=", b, self)?,
            Inst::Eq(a, b) => bin(a, "===", b, self)?,
            Inst::NotEq(a, b) => bin(a, "!==", b, self)?,
            // Space after `-`: `Neg` of a negative literal would otherwise emit
            // `(--1.5)`, which JS lexes as the `--` decrement operator.
            Inst::Neg(x) => format!("(- {})", self.render_val(*x)?),
            Inst::Not(x) => format!("(!{})", self.render_val(*x)?),
            // Identity on JS (all doubles), like codegen-lua on luajit.
            Inst::IntToFloat(x) => self.render_val(*x)?,
            // Rock float->int truncates toward -inf == floor.
            Inst::FloatToInt(x) => format!("Math.floor({})", self.render_val(*x)?),
            // Lua `..` coerces each operand via `tostring` (number formatting is
            // `%.14g`); JS `+` does not, so every leaf routes through
            // `lua_tostring` for parity. Emitted as one flat `+` run over the
            // whole concat tree (mirrors the Lua backend's flat `..`).
            Inst::StringConcat(a, b) => {
                let mut parts = Vec::new();
                self.collect_concat(&mut parts, *a);
                self.collect_concat(&mut parts, *b);
                let rendered: Result<Vec<String>, CompilerError> = parts
                    .iter()
                    .map(|p| Ok(format!("lua_tostring({})", self.render_val(*p)?)))
                    .collect();
                format!("({})", rendered?.join(" + "))
            }
            // The scalar `{x}` interpolation stringify (int/bool/string) -- same
            // `%.14g`-parity path as a concat leaf.
            Inst::ToString(x) => format!("lua_tostring({})", self.render_val(*x)?),
            // Rock `%` is Lua floored modulo; JS `%` is truncated remainder
            // (differs for mixed signs). `((a % b) + b) % b` is the floored form.
            Inst::IntMod(a, b) | Inst::FloatMod(a, b) => {
                let (a, b) = (self.render_val(*a)?, self.render_val(*b)?);
                format!("((({a} % {b}) + {b}) % {b})")
            }
            Inst::Call { func, args } => self.render_call(*func, args)?,
            // A capture-free hoisted closure: a bare reference to its top-level
            // fn (`convert_closures` emits `Inst::Global(<name>__lambda<k>)`).
            // Same-site references share one value -> `==` (P3 identity parity).
            // Any OTHER global read (module `let`, enum name table, named fn as a
            // value) is not emitted, so it stays out of subset. `error` is the
            // one runtime global: the coroutine transform's layout raise and
            // `await`'s re-raise read it as a value (`JS_CORO_RUNTIME` defines it).
            Inst::Global(name) if self.hoisted.contains(name) || name.as_str() == "error" => {
                name.to_string()
            }
            // A top-level fn referenced as a value: its co-emitted JS name (a
            // bare function; `__rock_call` invokes it directly).
            Inst::Global(name) if self.globals.fns.contains_key(name) => {
                self.globals.fns[name].clone()
            }
            // Module-level `let`/`global` state: scoped so no body local can
            // collide with or shadow it.
            Inst::Global(name) if self.globals.state.contains(name) => {
                format!("__G.{name}")
            }
            // A boxed-enum singleton / names table: a bare top-level name whose def
            // (`var __rock_enum_...`) `emit_with_closure` prepends to the program.
            Inst::Global(name) if self.globals.enum_globals.contains(name) => name.to_string(),
            Inst::Global(name) => {
                return Err(err(format!("JS subset: global read `{name}` not supported")))
            }
            // A closure ENV record `{ __fn, c<A>.. }` (capturing lambda) or a box
            // `{ v }` (a shared mutable capture) -- built by `convert_closures`.
            Inst::MakeEnv { fn_ref, fields } => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(name) = fn_ref {
                    parts.push(format!("__fn: {name}"));
                }
                for (f, v) in fields {
                    parts.push(format!("{f}: {}", self.render_val(*v)?));
                }
                format!("({{{}}})", parts.join(", "))
            }
            // An indirect call: a raw fn value calls directly, an env record routes
            // through its `__fn` -- the `__rock_call` dispatch helper handles both.
            Inst::CallValue { callee, args } => {
                let mut rendered = vec![self.render_val(*callee)?];
                for a in args {
                    rendered.push(self.render_val(*a)?);
                }
                format!("__rock_call({})", rendered.join(", "))
            }
            // Structs are plain objects tagged with `__ty` (mirrors the Lua
            // `__rock_create_struct`: sets `obj.__ty`, no metatable). NativeBuffer
            // keeps its bespoke `{ptr, len}` (no `__ty`) so the nb differential's
            // pins hold; every other named struct gets `{__ty: "Name", ...}`.
            Inst::MakeStruct { ty, fields } => {
                let name = ty.map(|t| t.to_string()).unwrap_or_default();
                let mut parts: Vec<String> = Vec::new();
                if !name.is_empty() && !is_native_buffer(&name) {
                    parts.push(format!("__ty: {}", js_string_lit(&name)?));
                }
                for (f, v) in fields {
                    // Leaf VIEWS are a luajit-representation detail (`ffi.cast`
                    // pointers over the element storage). JS backs a NativeBuffer
                    // with a TypedArray and lowers element access through its own
                    // flat-slot arithmetic, so a pointer view has no meaning here
                    // and no JS path reads these keys -- one that needed them
                    // would hit the `NativeBufferLeaf` reject instead.
                    if skull_mir::lower::is_leaf_view_field(f.as_str()) {
                        continue;
                    }
                    parts.push(format!("{}: {}", js_key(f)?, self.render_val(*v)?));
                }
                format!("({{{}}})", parts.join(", "))
            }
            // A `value` struct is byte-value cdata on luajit; on JS it is a plain
            // `{__ty, ...}` object, same shape as a reference struct. An all-scalar
            // (GC-free) one compares via the generic structural deep-eq
            // (`JS_STRUCTURAL_EQ`), matching luajit's byte-value eq. A GC-carrying
            // one compares its pointer fields BY REFERENCE on luajit, which the
            // generic contents-eq cannot reproduce; the per-type `__rock_vt_eq`
            // registry (`build_vt_eq_defs`) supplies that split and `__op_eq`
            // consults it before the generic walk -- mirroring luajit's `_vt.__eq`
            // metatable-authority rule. Construction is identical for both tiers.
            Inst::MakeValueStruct { name, fields }
            | Inst::MaterializeValueStruct { name, fields } => {
                // Dotted keys are the post-normalization LEAF form (a nested
                // nobox field flattened into its parent so it never boxes). JS
                // has no cdata layout to write leaves into -- a value struct is
                // a plain object -- so rebuild the nested objects the declared
                // shape expects. luajit instead calls the generated `_lct`.
                if fields.iter().any(|(f, _)| f.contains('.')) {
                    // Same strictness as the Lua emitter: a PARTIAL flattening
                    // must reject, not silently emit a half-shaped object.
                    let canonical =
                        skull_mir::lower::value_leaf_paths_of(self.ws, *name).unwrap_or_default();
                    let got: Vec<ustr::Ustr> = fields.iter().map(|(f, _)| *f).collect();
                    if got != canonical {
                        return Err(CompilerError::new_no_location(format!(
                            "`{name}` ctor carries dotted field names that are not its canonical \
                             leaf sequence: {got:?} vs {canonical:?}"
                        )));
                    }
                    return self.render_leaf_form_value_struct(*name, fields);
                }
                let mut parts = vec![format!("__ty: {}", js_string_lit(&name.to_string())?)];
                for (f, v) in fields {
                    // Leaf VIEWS are a luajit-representation detail (`ffi.cast`
                    // pointers over the element storage). JS backs a NativeBuffer
                    // with a TypedArray and lowers element access through its own
                    // flat-slot arithmetic, so a pointer view has no meaning here
                    // and no JS path reads these keys -- one that needed them
                    // would hit the `NativeBufferLeaf` reject instead.
                    if skull_mir::lower::is_leaf_view_field(f.as_str()) {
                        continue;
                    }
                    parts.push(format!("{}: {}", js_key(f)?, self.render_val(*v)?));
                }
                format!("({{{}}})", parts.join(", "))
            }
            // A non-cdata enum boxes as `{__variant, __ty, 0:.., 1:.., <name>:..}`
            // via the nil-dropping ctor (`JS_ENUM_RUNTIME`). Positional payloads take
            // native 0-based keys (luajit's 1-based table keys are Lua table-ctor
            // semantics; `TupleIndex` reads `base[index]` uniformly here). A unit
            // variant of a mixed enum is its interned singleton (an `Inst::Global`),
            // so a bare `MakeEnum` here always carries payload.
            Inst::MakeEnum { tag, ty, positional, named } => {
                self.render_make_enum(&js_string_lit(tag.as_str())?, ty, positional, named)?
            }
            Inst::MakeEnumDyn { tag, ty, positional, named } => {
                self.render_make_enum(&self.render_val(*tag)?, ty, positional, named)?
            }
            // A tuple is a plain JS array; `.N` reads index N. (Most tuples are
            // scalarized by SROA before emission; these cover the residue.)
            Inst::MakeTuple(elems) => {
                let rendered: Result<Vec<String>, CompilerError> =
                    elems.iter().map(|e| self.render_val(*e)).collect();
                format!("[{}]", rendered?.join(", "))
            }
            Inst::TupleIndex { base, index } => {
                format!("{}[{index}]", self.render_val(*base)?)
            }
            // A Vec literal: `{_data, len, __ty}` with `_data` a 0-based dense
            // array (the JS-native repr; the base-agnostic `__vec_data_*`
            // accessors read `_data[i]`).
            Inst::MakeVec(elems) => {
                let rendered: Result<Vec<String>, CompilerError> =
                    elems.iter().map(|e| self.render_val(*e)).collect();
                let rendered = rendered?;
                let data = format!("[{}]", rendered.join(", "));
                format!("({{_data: {data}, len: {}, __ty: \"Vec\"}})", elems.len())
            }
            Inst::Index { base, index, kind } => {
                let (b, i) = (self.render_val(*base)?, self.render_val(*index)?);
                match kind {
                    ContainerKind::NativeBuffer => format!("__nb_checked_get({b}, {i})"),
                    // A whole value-struct element read materializes a COPY object
                    // from the flat slots (immutable in Rock, so indistinguishable
                    // from Lua's cdata ref). Field/StoreField consumers never see
                    // this render -- they lower flat access from the operands.
                    ContainerKind::NativeBufferValue(elem) => {
                        self.nbv_copy_expr(&b, &i, *elem, 0, *elem)?
                    }
                    ContainerKind::Vec => format!("__vec_checked_get({b}, {i})"),
                    ContainerKind::VecValueColumns(elem) => {
                        return Err(err(column_reject(*elem)));
                    }
                    ContainerKind::VecColumn => {
                        return Err(err("JS subset: column-stored [T] not supported"));
                    }
                    // Leaf addressing is Lua-side today (`ffi.cast` views). The
                    // JS analogue is a TypedArray per leaf C type over one
                    // ArrayBuffer -- see docs/native-buffer-leaf-addressing-plan.md
                    // S6. Until that lands, skip rather than emit a wrong shape.
                    ContainerKind::NativeBufferLeaf => {
                        return Err(err("JS subset: NativeBuffer leaf views not supported"));
                    }
                    // Map miss returns nil (raw VM op in Lua); JS Map.get returns
                    // undefined on miss, which `== null` treats as nil.
                    ContainerKind::Map => format!("{b}.get({i})"),
                }
            }
            Inst::Len { base, kind } => match kind {
                ContainerKind::NativeBuffer
                | ContainerKind::NativeBufferValue(_)
                | ContainerKind::Vec
                | ContainerKind::VecValueColumns(_) => {
                    format!("{}.len", self.render_val(*base)?)
                }
                ContainerKind::VecColumn => format!("{}.length", self.render_val(*base)?),
                ContainerKind::NativeBufferLeaf => {
                    return Err(err("JS subset: NativeBuffer leaf views not supported"));
                }
                ContainerKind::Map => format!("{}.size", self.render_val(*base)?),
            },
            // The `native_buffer_alloc<T>` primitive lowers to this MIR node
            // (skull-mir intercepts the call and resolves the element ctype).
            // `Ptr<T>` IS a TypedArray on JS, matching the luajit `ffi.new` cdata;
            // value-struct / bool element ctypes have no TypedArray, so reject.
            Inst::FfiNewArray { ctype, count } => match ctype.as_str() {
                "float" => format!("new Float32Array({})", self.render_val(*count)?),
                "int" => format!("new Int32Array({})", self.render_val(*count)?),
                // An all-float value struct (e.g. the vertex pool) has no per-struct
                // ctype on JS: back it onto one flat Float32Array of `n * k` floats
                // (k = flattened float leaf count). The `.ptr` is that array; the
                // hot path reads it through a cached `Ptr<float>` view (a js sibling
                // of `__vbo_as_float_ptr` returns `.ptr` itself). A value struct with
                // any non-float leaf still rejects.
                other => match skull_mir::lower::value_struct_all_float_leaf_count(self.ws, *ctype) {
                    Some(k) => format!("new Float32Array(({}) * {k})", self.render_val(*count)?),
                    None => {
                        return Err(err(format!(
                            "JS subset: native_buffer_alloc element ctype `{other}` has no \
                             TypedArray (only int, float, and all-float value structs map)"
                        )))
                    }
                },
            },
            // A general field read is JS property access. NativeBuffer `.ptr`/
            // `.len` and Vec `._data`/`.len` are the same shape. A base that
            // projects a value-element NativeBuffer slot lowers to flat-slot
            // access instead: a float leaf reads `__nbv_get`, a nested value
            // struct materializes its sub-object copy.
            Inst::Field { base, field } => {
                if let Some((b_iid, i_iid, root, off, cur)) = self.nbv_proj(*base)? {
                    let (_, fields) = self.nbv_layout(cur)?;
                    let f = fields.iter().find(|f| f.name == *field).ok_or_else(|| {
                        err(format!("JS subset: value struct `{cur}` has no field `{field}`"))
                    })?;
                    let (root_k, _) = self.nbv_layout(root)?;
                    let b = self.render_val(b_iid)?;
                    let i = self.render_val(i_iid)?;
                    return Ok(match f.nested {
                        None => format!("__nbv_get({b}, {i}, {root_k}, {})", off + f.offset),
                        Some(n) => self.nbv_copy_expr(&b, &i, root, off + f.offset, n)?,
                    });
                }
                js_member(&self.render_val(*base)?, field)?
            }
            // `{x:spec}` interpolation: `string.format(fmt, value)` for a single
            // value (Lua path). `__rock_sprintf` is the single-value C printf port
            // (d/i/u/x/X/o/f/F/e/E/g/G/s/c, flags, width, precision; half-even
            // rounding). Format's fmt is a static string, FormatDyn's is computed.
            Inst::Format { value, fmt } => {
                format!("__rock_sprintf({}, {})", js_string_lit(fmt.as_str())?, self.render_val(*value)?)
            }
            Inst::FormatDyn { value, fmt } => {
                format!("__rock_sprintf({}, {})", self.render_val(*fmt)?, self.render_val(*value)?)
            }
            Inst::MakeVecColumns { elem, .. } => return Err(err(column_reject(*elem))),
            other => {
                return Err(err(format!(
                    "JS subset: cannot render {other:?} as a JS expression"
                )))
            }
        })
    }

    /// Flatten a `StringConcat` tree into its leaf operands (descending into
    /// child `StringConcat`s), mirroring the Lua backend's flat-`..` collection.
    fn collect_concat(&self, parts: &mut Vec<Idx<Inst>>, id: Idx<Inst>) {
        if let Inst::StringConcat(a, b) = &self.mf.insts[id] {
            self.collect_concat(parts, *a);
            self.collect_concat(parts, *b);
        } else {
            parts.push(id);
        }
    }

    /// Box an enum: `__rock_make_enum(<tag>, "<ty>", [0, p0, 1, p1, "name", v, ..])`.
    /// `tag_expr` is a rendered JS expression (a string literal for `MakeEnum`, the
    /// dynamic tag operand for `MakeEnumDyn`). The ctor drops nil `kv` slots so a
    /// union-wide rebox owns exactly the active variant's keys.
    /// Rebuild the declared nested-object shape from a flattened leaf field list
    /// (`("pos.x", ..), ("pos.y", ..)` -> `{pos: {__ty:"NV2", x:.., y:..}}`).
    /// The leaf form exists only between the end of `cleanup` and emit; luajit
    /// consumes it via a generated leaf ctor, JS has to reassemble.
    /// The struct type of `name`'s field `head`, when that field is itself a
    /// value struct -- so a rebuilt sub-object carries the `__ty` the non-leaf
    /// path would give it, and the inspect renderer can recurse into a
    /// flattened group. Fails closed: a non-struct field yields `None`.
    fn nested_field_struct_name(&self, name: ustr::Ustr, head: &str) -> Option<ustr::Ustr> {
        let sid = self.ws.struct_by_name.get(&name)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        let f = sd.fields.iter().find(|f| f.name.as_str() == head)?;
        let rendered = self.ws.render(None, f.ty);
        let n = ustr::Ustr::from(rendered.trim_end_matches('?'));
        self.ws.struct_by_name.contains_key(&n).then_some(n)
    }

    /// Split a possibly-FLATTENED field list into exact fields and per-head
    /// groups of dotted leaves. Shared by the leaf-form object renderer and the
    /// inspect renderer so the two read the same shape the same way.
    fn split_leaf_fields(
        fields: &[(ustr::Ustr, Idx<Inst>)],
    ) -> (
        HashMap<ustr::Ustr, Idx<Inst>>,
        Vec<(String, Vec<(ustr::Ustr, Idx<Inst>)>)>,
    ) {
        let mut exact: HashMap<ustr::Ustr, Idx<Inst>> = HashMap::new();
        let mut groups: Vec<(String, Vec<(ustr::Ustr, Idx<Inst>)>)> = Vec::new();
        for (f, v) in fields {
            match f.as_str().split_once('.') {
                None => {
                    exact.insert(*f, *v);
                }
                Some((head, rest)) => {
                    let rest = ustr::Ustr::from(rest);
                    match groups.iter_mut().find(|(h, _)| h == head) {
                        Some((_, sub)) => sub.push((rest, *v)),
                        None => groups.push((head.to_string(), vec![(rest, *v)])),
                    }
                }
            }
        }
        (exact, groups)
    }

    fn render_leaf_form_value_struct(
        &self,
        name: ustr::Ustr,
        fields: &[(ustr::Ustr, Idx<Inst>)],
    ) -> Result<String, CompilerError> {
        // Nested groups in first-seen order, so the emitted object matches the
        // declared field order the non-leaf path produces.
        let mut top: Vec<(ustr::Ustr, String)> = Vec::new();
        let mut groups: Vec<(String, Vec<(ustr::Ustr, Idx<Inst>)>)> = Vec::new();
        for (f, v) in fields {
            match f.as_str().split_once('.') {
                None => top.push((*f, self.render_val(*v)?)),
                Some((head, rest)) => {
                    let rest = ustr::Ustr::from(rest);
                    match groups.iter_mut().find(|(h, _)| h == head) {
                        Some((_, sub)) => sub.push((rest, *v)),
                        None => groups.push((head.to_string(), vec![(rest, *v)])),
                    }
                }
            }
        }
        let field_ty = |head: &str| self.nested_field_struct_name(name, head);
        let mut parts = vec![format!("__ty: {}", js_string_lit(&name.to_string())?)];
        for (f, rendered) in &top {
            parts.push(format!("{}: {rendered}", js_key(f)?));
        }
        for (head, sub) in &groups {
            let sub_name = field_ty(head).ok_or_else(|| {
                CompilerError::new_no_location(format!(
                    "leaf-form ctor for `{name}`: field `{head}` is not a value struct"
                ))
            })?;
            let inner = self.render_leaf_form_value_struct(sub_name, sub)?;
            parts.push(format!("{}: {inner}", js_key(&ustr::Ustr::from(head))?));
        }
        Ok(format!("({{{}}})", parts.join(", ")))
    }

    fn render_make_enum(
        &self,
        tag_expr: &str,
        ty: &ustr::Ustr,
        positional: &[Idx<Inst>],
        named: &[(ustr::Ustr, Idx<Inst>)],
    ) -> Result<String, CompilerError> {
        let mut kv: Vec<String> = Vec::new();
        for (i, id) in positional.iter().enumerate() {
            kv.push(i.to_string());
            kv.push(self.render_val(*id)?);
        }
        for (name, id) in named {
            kv.push(js_string_lit(name.as_str())?);
            kv.push(self.render_val(*id)?);
        }
        Ok(format!(
            "__rock_make_enum({tag_expr}, {}, [{}])",
            js_string_lit(ty.as_str())?,
            kv.join(", ")
        ))
    }

    /// Flat-slot layout of value-element `elem`: `(stride k, immediate fields)`.
    /// Errs (fail-closed) when `elem` has a non-float leaf -- such a buffer cannot
    /// be allocated on JS (`FfiNewArray` rejects), so access to one is a backend
    /// gap, never a silent scalar read.
    fn nbv_layout(
        &self,
        elem: ustr::Ustr,
    ) -> Result<(usize, Vec<skull_mir::lower::ValueSlotField>), CompilerError> {
        let k = skull_mir::lower::value_struct_all_float_leaf_count(self.ws, elem);
        let fields = skull_mir::lower::value_struct_slot_fields(self.ws, elem);
        match (k, fields) {
            (Some(k), Some(fields)) => Ok((k, fields)),
            _ => Err(err(format!(
                "JS subset: NativeBuffer element `{elem}` is not an all-float value \
                 struct -- no flat Float32Array layout to index"
            ))),
        }
    }

    /// Resolve `iid` as a value-element NativeBuffer projection: the root
    /// `Index{NativeBufferValue}` or a nested-struct `Field` chain off one.
    /// `Ok(None)` = not NBV-related; `Err` = NBV but unlowerable (fail-closed).
    /// Returns `(buf inst, index inst, root elem, cumulative offset, cur struct)`.
    fn nbv_proj(
        &self,
        iid: Idx<Inst>,
    ) -> Result<Option<(Idx<Inst>, Idx<Inst>, ustr::Ustr, usize, ustr::Ustr)>, CompilerError> {
        match &self.mf.insts[iid] {
            Inst::Index { base, index, kind: ContainerKind::NativeBufferValue(e) } => {
                self.nbv_layout(*e)?;
                Ok(Some((*base, *index, *e, 0, *e)))
            }
            Inst::Field { base, field } => {
                let Some((b, i, root, off, cur)) = self.nbv_proj(*base)? else {
                    return Ok(None);
                };
                let (_, fields) = self.nbv_layout(cur)?;
                let f = fields.iter().find(|f| f.name == *field).ok_or_else(|| {
                    err(format!("JS subset: value struct `{cur}` has no field `{field}`"))
                })?;
                match f.nested {
                    Some(n) => Ok(Some((b, i, root, off + f.offset, n))),
                    // A float leaf terminates the projection chain -- the Field
                    // render handles it directly; it is never itself a base.
                    None => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    /// `{__ty: "S", f: __b.ptr[__o + N], nested: {...}}` -- the object literal
    /// reading struct `cur`'s leaves from the flat backing, relative to the free
    /// vars `__b` (buffer) and `__o` (element base slot). `base` is the cumulative
    /// float offset of `cur` within the root element.
    fn nbv_obj_literal(&self, cur: ustr::Ustr, base: usize) -> Result<String, CompilerError> {
        let (_, fields) = self.nbv_layout(cur)?;
        let mut parts = vec![format!("__ty: {}", js_string_lit(cur.as_str())?)];
        for f in &fields {
            let part = match f.nested {
                Some(n) => self.nbv_obj_literal(n, base + f.offset)?,
                None => format!("__b.ptr[__o + {}]", base + f.offset),
            };
            parts.push(format!("{}: {part}", js_key(&f.name)?));
        }
        Ok(format!("{{{}}}", parts.join(", ")))
    }

    /// Whole-element (or nested sub-struct) COPY read: a bounds-checked IIFE
    /// materializing the value-struct object from the flat slots. Value structs
    /// are immutable in Rock, so a copy is indistinguishable from Lua's cdata ref.
    fn nbv_copy_expr(
        &self,
        b: &str,
        i: &str,
        root: ustr::Ustr,
        off: usize,
        cur: ustr::Ustr,
    ) -> Result<String, CompilerError> {
        let (k, _) = self.nbv_layout(root)?;
        let obj = self.nbv_obj_literal(cur, off)?;
        Ok(format!(
            "(function(__b, __i) {{ __nbv_check(__b, __i); var __o = __i * {k}; return {obj}; }})({b}, {i})"
        ))
    }

    /// Leaf-store lines writing struct `cur` (at cumulative offset `base`) from
    /// the object expression `__v{path}` -- used by the whole-element write IIFE.
    fn nbv_store_lines(
        &self,
        cur: ustr::Ustr,
        base: usize,
        path: &str,
        out: &mut Vec<String>,
    ) -> Result<(), CompilerError> {
        let (_, fields) = self.nbv_layout(cur)?;
        for f in &fields {
            match f.nested {
                Some(n) => self.nbv_store_lines(n, base + f.offset, &format!("{path}.{}", f.name), out)?,
                None => out.push(format!("__b.ptr[__o + {}] = __v{path}.{};", base + f.offset, f.name)),
            }
        }
        Ok(())
    }

    /// Whole-element (or nested sub-struct) write: bounds-checked IIFE storing
    /// every leaf of `cur` (at offset `off` within root) from the value object.
    fn nbv_write_stmt(
        &self,
        b: &str,
        i: &str,
        v: &str,
        root: ustr::Ustr,
        off: usize,
        cur: ustr::Ustr,
    ) -> Result<String, CompilerError> {
        let (k, _) = self.nbv_layout(root)?;
        let mut lines = Vec::new();
        self.nbv_store_lines(cur, off, "", &mut lines)?;
        Ok(format!(
            "(function(__b, __i, __v) {{ __nbv_check(__b, __i); var __o = __i * {k}; {} }})({b}, {i}, {v});",
            lines.join(" ")
        ))
    }

    /// Colon-format render of a GC-free value struct `inspect` arg, matching the
    /// luajit cdata `__tostring` (`Name { f: v }`, declaration field order). Nested
    /// value-struct fields recurse; primitive leaves defer to the generic `inspect`.
    fn render_value_struct_inspect(
        &self,
        id: Idx<Inst>,
        cyc: &str,
        prec: &str,
    ) -> Result<String, CompilerError> {
        // `MaterializeValueStruct` is the same box, explicitly authorized -- it
        // inspects identically.
        if let Inst::MakeValueStruct { name, fields }
        | Inst::MaterializeValueStruct { name, fields } = &self.mf.insts[id]
        {
            if skull_mir::lower::gc_free_value_struct_field_order(self.ws, *name).is_some() {
                return self.render_value_struct_inspect_fields(*name, fields, cyc, prec);
            }
        }
        Ok(format!("inspect({}, {cyc}, {prec})", self.render_val(id)?))
    }

    /// `Name { f: v, .. }` in DECLARED field order, from a field list that may be
    /// the flattened leaf form. A nested `nobox` field arrives as dotted leaves
    /// (`pos.x`, `pos.y`) with no `pos` entry, so a lookup by declared name alone
    /// misses it -- and a missed lookup here is an emit REJECTION, which turns
    /// every print of such a struct into a skip rather than a diagnosis.
    fn render_value_struct_inspect_fields(
        &self,
        name: ustr::Ustr,
        fields: &[(ustr::Ustr, Idx<Inst>)],
        cyc: &str,
        prec: &str,
    ) -> Result<String, CompilerError> {
        let order = skull_mir::lower::gc_free_value_struct_field_order(self.ws, name)
            .ok_or_else(|| err(format!("`{name}` is not a GC-free value struct")))?;
        let (exact, groups) = Self::split_leaf_fields(fields);
        let mut expr = js_string_lit(&format!("{name} {{ "))?;
        for (i, fname) in order.iter().enumerate() {
            if i > 0 {
                expr.push_str(&format!(" + {}", js_string_lit(", ")?));
            }
            expr.push_str(&format!(" + {}", js_string_lit(&format!("{fname}: "))?));
            let rendered = if let Some(vid) = exact.get(fname) {
                self.render_value_struct_inspect(*vid, cyc, prec)?
            } else if let Some((_, sub)) = groups.iter().find(|(h, _)| h == fname.as_str()) {
                let sub_name = self.nested_field_struct_name(name, fname).ok_or_else(|| {
                    err(format!(
                        "value struct `{name}` field `{fname}` arrives as leaves but is not a value struct"
                    ))
                })?;
                self.render_value_struct_inspect_fields(sub_name, sub, cyc, prec)?
            } else {
                return Err(err(format!(
                    "value struct `{name}` field `{fname}` absent at inspect boundary"
                )));
            };
            expr.push_str(&format!(" + {rendered}"));
        }
        expr.push_str(&format!(" + {}", js_string_lit(" }")?));
        Ok(format!("({expr})"))
    }

    fn render_call(&self, func: FuncRef, args: &[Idx<Inst>]) -> Result<String, CompilerError> {
        let cf = &self.ws.module(func.module).functions[func.func];
        // `inspect`/`inspect_formatted` of a GC-FREE value struct still diverges:
        // it is cdata on luajit, rendered `Name { f: v }` (colon, via the cdata
        // `__tostring`) where the JS `{__ty,..}`-object walker emits `f = v`
        // (equals) -- reject at the box boundary. A GC-CARRYING value struct is a
        // `_vt` TABLE on luajit; `inspect` walks it via the SAME generic
        // equals-format walker as the JS object (NOT the `_vt.__tostring`, which it
        // never calls -- oracle-verified: luajit renders `Bag { n = 5, xs = Vec {..} }`),
        // so it inspects faithfully. A boxed ENUM likewise renders faithfully (T8e).
        // `inspect` of a GC-FREE value struct: luajit renders it via the cdata
        // `__tostring` as `Name { f: v }` (COLON, declaration field order), where the
        // generic `{__ty,..}`-object walker would emit `f = v` (equals, sorted keys).
        // A GC-free value struct's fields are bounded to primitives + nested value
        // structs (no strings/GC by the C-layout rule), so an emit-time recursive
        // colon renderer is exact -- primitive leaves go through the generic
        // `inspect` (lua_tostring / precision parity), nested value structs recurse.
        if matches!(cf.name.as_str(), "inspect" | "inspect_formatted") {
            if let Some(&arg0) = args.first() {
                if let Inst::MakeValueStruct { name, .. } = &self.mf.insts[arg0] {
                    if skull_mir::lower::gc_free_value_struct_field_order(self.ws, *name).is_some() {
                        let (cyc, prec) = if cf.name.as_str() == "inspect_formatted" {
                            ("true".to_string(), self.render_val(args[1])?)
                        } else {
                            (
                                args.get(1).map(|a| self.render_val(*a)).transpose()?.unwrap_or_else(|| "true".to_string()),
                                args.get(2).map(|a| self.render_val(*a)).transpose()?.unwrap_or_else(|| "(-1)".to_string()),
                            )
                        };
                        return self.render_value_struct_inspect(arg0, &cyc, &prec);
                    }
                    // A scalarized ENUM box reaches here (name is an enum value-shape,
                    // not a struct): its numeric tag / named variant cannot be rebuilt
                    // from the `{__tag, _n}` object -- reject honestly.
                    if skull_mir::lower::value_struct_is_gc_free(self.ws, *name) {
                        return Err(err(format!(
                            "JS subset: `inspect` of scalarized enum `{name}` \
                             (numeric-tag box cannot reproduce the named variant)"
                        )));
                    }
                }
            }
        }
        let rargs: Result<Vec<String>, CompilerError> =
            args.iter().map(|a| self.render_val(*a)).collect();
        let rargs = rargs?;
        // The overloaded `get`/2 dispatches on its receiver: NativeBuffer via the
        // intrinsic template (below), Map via the JS `Map.get` (miss -> undefined,
        // which `== null` reads as nil), Vec co-emits its pure-Rock body. Any
        // other receiver rejects rather than kinda-working.
        // Only the BUILTIN Map (`TypeCtor::Map`) renders inline as `.get` -- a user
        // struct named `Map` (a `Nominal`) with its own `get` fn is a plain callee
        // and co-emits like any other. NativeBuffer falls to its intrinsic
        // template; Vec and user receivers fall to the co-emit path.
        if cf.name.as_str() == "get"
            && cf.param_decls.len() == 2
            && first_param_is_builtin_map(self.ws, cf)
        {
            return Ok(format!("{}.get({})", rargs[0], rargs[1]));
        }
        // Host-bodied stdlib fns (extern / inline(lua)) with a hand-written JS
        // binding: `print` (variadic -> the harness global), the fixed-arity
        // templates (`tostring`, `__rock_format_float`).
        if is_host_print(cf) {
            return Ok(format!("print({})", rargs.join(", ")));
        }
        if is_host_put(cf) {
            return Ok(format!("put({})", rargs.join(", ")));
        }
        // Logging dedup/throttle impls -> their JS runtime port, same name.
        if is_host_log_fn(cf) {
            return Ok(format!("{}({})", cf.name, rargs.join(", ")));
        }
        if let Some(tmpl) = host_fn_template(cf) {
            // A defaulted trailing param (e.g. `string.find`'s `from = 0`) is not
            // materialized into the call args, so a template `$N` for it would
            // stay literal. Pad to the declared arity with `undefined`: JS reads
            // an omitted arg as `undefined`, and for the templated fns that carry
            // a default (`find` -> indexOf, whose `from` defaults to 0) that
            // coincides with the Rock default. Fixed-arity templates are already
            // full, so this is a no-op there.
            let mut padded = rargs.clone();
            while padded.len() < cf.param_decls.len() {
                padded.push("undefined".to_string());
            }
            let refs: Vec<&str> = padded.iter().map(|s| s.as_str()).collect();
            return Ok(fill_template(tmpl, &refs));
        }
        // `map_new` is pure Rock (`transmute(table_new())`) but must be a real JS
        // `Map`, not a plain object -- a plain object coerces keys to strings, so
        // Lua's 1 vs "1" distinction would be lost. Intercept it (and skip its
        // co-emission in the closure walk).
        if is_map_new(cf) {
            return Ok("new Map()".to_string());
        }
        // Templates apply only to stdlib @intrinsic decls (`is_mapped_intrinsic`):
        // a user fn sharing an entry's (name, arity) must co-emit, not become
        // `Math.abs`.
        match if is_mapped_intrinsic(cf) { JsIntrinsics.lookup(cf) } else { None } {
            Some(Emission::Template(tmpl)) => {
                let refs: Vec<&str> = rargs.iter().map(|s| s.as_str()).collect();
                Ok(fill_template(tmpl, &refs))
            }
            _ => match self.callees.get(&func) {
                Some(name) => Ok(format!("{name}({})", rargs.join(", "))),
                None => Err(err(format!(
                    "JS subset: call to `{}` is not a mapped JS intrinsic",
                    cf.name
                ))),
            },
        }
    }
}

// --- counted-loop recognition (mirrors skull-codegen-glsl's / -lua's, for the
// range-for shape `lower_for_range` emits) -------------------------------------

/// `Some(LoopInfo)` iff `h` is a `for i in lo..hi` header: a `Branch` on
/// `IntLt/IntLeq(load slot, bound)` where the `slot` cell is `loop_binding`
/// (desugar provenance only `lower_for_range` sets, so a manual
/// `while i < n { ..; i = i + 1 }` is NOT mistaken for a range-for) and `h` has a
/// pure-increment latch. The bound must render inline (a `Load` of a hoisted
/// preheader cell, or a const) since the counted path skips the header block.
fn match_counted_loop(f: &MirFunc, h: Idx<Block>) -> Option<LoopInfo> {
    let Term::Branch { cond, then_bb, else_bb } = f.blocks[h].term else {
        return None;
    };
    let (slot, bound, inclusive) = match &f.insts[cond] {
        Inst::IntLt(a, b) => (load_cell(f, *a)?, *b, false),
        Inst::IntLeq(a, b) => (load_cell(f, *a)?, *b, true),
        _ => return None,
    };
    if !f.cells[slot].loop_binding {
        return None;
    }
    if !matches!(f.insts[bound], Inst::Load(_) | Inst::Const(_)) {
        return None; // non-inline bound would reference a skipped header temp
    }
    let mut latch = None;
    for (bid, blk) in f.blocks.iter() {
        if blk.term == Term::Goto(h) && is_incr_latch(f, bid, slot) {
            latch = Some(bid);
        }
    }
    let latch = latch?;
    Some(LoopInfo { slot, bound, inclusive, body_entry: then_bb, latch, exit: else_bb })
}

fn load_cell(f: &MirFunc, iid: Idx<Inst>) -> Option<Idx<Cell>> {
    match f.insts[iid] {
        Inst::Load(c) => Some(c),
        _ => None,
    }
}

/// `p`: `load slot; 1; slot + 1; store slot; goto h` -- a pure `slot += 1` latch.
fn is_incr_latch(f: &MirFunc, p: Idx<Block>, slot: Idx<Cell>) -> bool {
    for &iid in &f.blocks[p].insts {
        if let Inst::Store { cell, value } = f.insts[iid] {
            if cell == slot {
                if let Inst::IntAdd(a, b) = f.insts[value] {
                    let is_one = |x: Idx<Inst>| matches!(f.insts[x], Inst::Const(Lit::Int(1)));
                    let loads_slot = |x: Idx<Inst>| matches!(f.insts[x], Inst::Load(c) if c == slot);
                    return (loads_slot(a) && is_one(b)) || (loads_slot(b) && is_one(a));
                }
                return false;
            }
        }
    }
    false
}

/// Deterministic JS name for a co-emitted callee -- the SHARED mangle keyed on
/// `(name, declared signature)`, identical to the Lua and GLSL backends. Exempt
/// (`extern`/`@nomangle`/`@test`/`main`/`test_*`) fns keep their bare name: the
/// discipline is "called by original name from inline bodies / compiler
/// codegen", and on JS an inline(js) body or a driver-injected line is verbatim
/// text that can only use the bare name (same rule as the lua backend).
pub fn callee_js_name(ws: &Workspace, fr: FuncRef) -> String {
    let f = &ws.module(fr.module).functions[fr.func];
    skull_mir::mangle::mangle_fn(ws, f)
}

/// A `@nomangle` fn keeps its bare name, so an inline(js) body referencing it by
/// its literal source name resolves to that bare global (the verbatim-name
/// discipline).
fn is_nomangle(f: &bone_semantic::hir::Function) -> bool {
    f.annotations.iter().any(|a| a.name.name.as_str() == "nomangle")
}

/// Emit `roots` (each under its given JS name) plus the transitive plain-Rock
/// callee closure of each root, callees under [`callee_js_name`]s. Mapped
/// intrinsics stay call-site templates. Any callee that is neither (inline(lua)
/// without a mapping, extern) and any op outside the emitter subset rejects
/// with the emitter's `CompilerError` -- the caller turns that into SKIP(reason).
/// Function declarations hoist in JS, so definition order is irrelevant.
pub fn emit_with_closure(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    roots: &[(FuncRef, String)],
) -> Result<String, CompilerError> {
    let mut st = WorklistState::new(ws, roots, false);
    drain_worklists(ws, scx, &mut st)?;
    st.canonicalize_emission_order();
    let bodies = st.top_bodies();
    check_scalar_abi(ws, scx, &st.lowered, &bodies)?;
    let hoisted = collect_hoisted(&bodies);

    let mut out = String::new();
    out.push_str(&emit_enum_singletons(&st.gctx, &st.enum_defs));
    out.push_str(&emit_vt_eq(ws, &bodies)?);
    // Global initializers first (`function __ginit_<name>() {...}`), then the
    // load-time assignment lines in dependency order, then the fn defs. (The
    // program path reorders these; per-test order is pinned by the JS suite.)
    for g in &st.ginits {
        out.push_str(&emit_fn_with_callees(ws, &g.mf, &g.init_name, &st.names, &hoisted, &st.gctx)?);
        for nf in &g.mf.nested_fns {
            out.push_str(&emit_fn_with_callees(ws, nf, nf.name.as_str(), &st.names, &hoisted, &st.gctx)?);
        }
    }
    for init in order_global_inits(&st.ginits, &st.lowered, &st.names)? {
        out.push_str(&init.js);
    }
    for (fr, mf) in &st.lowered {
        out.push_str(&emit_lowered_fn(ws, *fr, mf, &st.names[fr], &st.names, &hoisted, &st.gctx)?);
    }
    for fr in &st.inline_js {
        out.push_str(&emit_inline_js_fn(ws, scx, *fr, &st.names[fr]));
    }
    Ok(out)
}

/// The whole workspace as ONE JS program (M1, `docs/js-deployment-plan.md`):
/// module-driven assembly mirroring skull's Lua `assemble`, but reachability-
/// driven (roots = `main` + every DECORATED fn + every `deco` body; the whole
/// stdlib is NOT emitted, only what those roots reach). Emission order:
/// singletons -> `__deco_<name>` def fns -> fn defs -> `__ginit_<name>` def fns
/// -> load-time init lines (dep-ordered) -> decorator APPLICATION lines (after
/// the inits so a deco body can push into a ginit-initialized registry). The
/// driver prepends the runtime preludes + `var __G = {}` + a direct
/// `print`/`put`, and appends `<main>();` when `main_js_name` is `Some`.
///
/// Unlike [`emit_with_closure`], a decorator-populated global read does NOT
/// reject here -- the applications ARE emitted, so the registry is populated at
/// module load exactly as on luajit.
pub fn emit_program(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    user_mid: bone_semantic::prelude::ModuleId,
) -> Result<JsProgram, CompilerError> {
    // The program entry: a nullary `main` in the entry module (D3). Passed as the
    // one extra root; the module-load effects (deco applications, ginits) run
    // regardless of whether a `main` exists.
    let mut extra: Vec<(FuncRef, String)> = Vec::new();
    let main_js_name = nullary_main(ws, user_mid).map(|fr| {
        let nm = callee_js_name(ws, fr);
        extra.push((fr, nm.clone()));
        nm
    });
    let build = build_program_core(ws, scx, &extra)?;
    Ok(JsProgram { program: build.program, main_js_name })
}

pub fn emit_program_cached(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    user_mid: bone_semantic::prelude::ModuleId,
) -> Result<(JsProgram, JsEmitCache), CompilerError> {
    let mut extra = Vec::new();
    let main_js_name = nullary_main(ws, user_mid).map(|fr| {
        let name = callee_js_name(ws, fr);
        extra.push((fr, name.clone()));
        name
    });
    let build = build_program_core(ws, scx, &extra)?;
    let static_keys = build
        .chunks
        .values()
        .flat_map(|chunk| chunk.statics.iter())
        .map(|cell| (cell.cell, static_cell_key(cell)))
        .collect();
    let program = JsProgram {
        program: build.program.clone(),
        main_js_name,
    };
    let cache = JsEmitCache {
        build,
        changed_since_full: HashSet::new(),
        static_keys,
    };
    Ok((program, cache))
}

/// [`emit_program`] with EXPLICIT extra roots and no `main` handling -- the
/// per-test path's whole-file emission (roots = the file's runnable `@test`
/// fns), so decorator applications populate the registries a test reads. Returns
/// just the program string; the caller invokes each root by the JS name it gave.
pub fn emit_program_with_roots(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    roots: &[(FuncRef, String)],
) -> Result<String, CompilerError> {
    emit_program_core(ws, scx, roots)
}

/// The shared program assembly for [`emit_program`] / [`emit_program_with_roots`]:
/// `extra_roots` (main, or the test fns) plus every DECORATED fn workspace-wide
/// (its application line names the fn as `func`, so it must be emitted even if no
/// extra root calls it), the APPLIED `deco` def bodies, ginits, and the
/// application lines. `extra_roots` seed the `names` map FIRST so their chosen
/// JS names win over the `callee_js_name` a discovery pass would assign.
fn emit_program_core(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    extra_roots: &[(FuncRef, String)],
) -> Result<String, CompilerError> {
    Ok(build_program_core(ws, scx, extra_roots)?.program)
}

fn build_program_core(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    extra_roots: &[(FuncRef, String)],
) -> Result<ProgramBuild, CompilerError> {
    let deco_names = decorator_names(ws);
    let mut roots: Vec<(FuncRef, String)> = extra_roots.to_vec();
    let mut root_set: HashSet<FuncRef> = extra_roots.iter().map(|(fr, _)| *fr).collect();
    for module in ws.modules.iter() {
        if module.id == bone_semantic::prelude::ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            if f.annotations.iter().any(|a| deco_names.contains(&a.name.name)) {
                let fr = FuncRef { module: module.id, func: fid };
                if root_set.insert(fr) {
                    roots.push((fr, callee_js_name(ws, fr)));
                }
            }
        }
    }

    let mut st = WorklistState::new(ws, &roots, true);

    force_shader_register_sibling(ws, &mut st)?;

    // Decorator DEFINITION bodies (`deco <kind> <name> { .. }` -> `__deco_<name>`),
    // for the APPLIED decorators only (D1 reachability: an unapplied `deco` -- e.g.
    // stdlib `on_reload` -- contributes no application line and must not force its
    // possibly-unsupported body into the program). Lowered via `BodyOwner::
    // Decorator`, then the SAME per-body pipeline a normal fn runs (incl. inlining,
    // so `@inline` no-ops like `transmute` splice away -- the luajit path leaves
    // them as top-level calls it emits whole-stdlib, which JS cannot). Their
    // callees/globals feed the shared worklists BEFORE the drain.
    let applied = applied_decorator_names(ws, &deco_names);
    let mut deco_bodies: Vec<(String, MirFunc)> = Vec::new();
    for module in ws.modules.iter() {
        for (did, d) in module.decorators.iter() {
            if !applied.contains(&d.name) {
                continue;
            }
            let deco_name = decorator_js_name(d.name.as_str());
            let mut mf =
                skull_mir::lower::lower_decorator(ws, scx, module.id, did, ustr::Ustr::from(deco_name.as_str()))?;
            let self_ref = safe_self_ref(ws, &mf);
            skull_transform::inline_functions(ws, scx, self_ref, &mut mf, &mut st.cache)?;
            skull_transform::cleanup_with_nobox(ws, &mut mf);
            skull_transform::convert_closures(&mut mf, &deco_name);
            skull_transform::recover_counted_loops(ws, &mut mf);
            for nf in &mut mf.nested_fns {
                skull_transform::recover_counted_loops(ws, nf);
            }
            register_statics(&mf, &mut st.gctx);
            discover_callees(ws, &mf, &mut st.names, &mut st.queue, &mut st.seen, &mut st.inline_js)?;
            discover_globals(
                ws, &mf, &st.state_index, &st.deco_globals, &st.enum_defs, &mut st.gctx,
                &mut st.ginit_queue, &mut st.ginit_seen, &mut st.names, &mut st.queue, &mut st.seen,
                &mut st.inline_js,
            )?;
            deco_bodies.push((deco_name, mf));
        }
    }

    drain_worklists(ws, scx, &mut st)?;
    st.canonicalize_emission_order();

    let mut bodies = st.top_bodies();
    bodies.extend(deco_bodies.iter().map(|(_, mf)| mf));
    check_scalar_abi(ws, scx, &st.lowered, &bodies)?;
    let hoisted = collect_hoisted(&bodies);

    let mut out = String::new();
    let mut stable = String::new();
    let enum_singletons = emit_enum_singletons(&st.gctx, &st.enum_defs);
    out.push_str(&enum_singletons);
    stable.push_str(&enum_singletons);
    let vt_eq = emit_vt_eq(ws, &bodies)?;
    out.push_str(&vt_eq);
    stable.push_str(&vt_eq);
    // 1. deco def fns.
    for (name, mf) in &deco_bodies {
        let definition = emit_fn_with_callees(ws, mf, name, &st.names, &hoisted, &st.gctx)?;
        out.push_str(&definition);
        stable.push_str(&definition);
        for nf in &mf.nested_fns {
            let definition =
                emit_fn_with_callees(ws, nf, nf.name.as_str(), &st.names, &hoisted, &st.gctx)?;
            out.push_str(&definition);
            stable.push_str(&definition);
        }
    }
    // 2. fn defs (main, decorated fns, transitive callees).
    let mut chunks = HashMap::new();
    for (fr, mf) in &st.lowered {
        let js = emit_lowered_fn(ws, *fr, mf, &st.names[fr], &st.names, &hoisted, &st.gctx)?;
        out.push_str(&js);
        chunks.insert(*fr, JsChunk { js, statics: collect_statics(mf) });
    }
    // 2b. inline(js) fns, emitted verbatim.
    for fr in &st.inline_js {
        let definition = emit_inline_js_fn(ws, scx, *fr, &st.names[fr]);
        out.push_str(&definition);
        stable.push_str(&definition);
    }
    // 3. ginit def fns.
    let mut const_chunks = HashMap::new();
    for g in &st.ginits {
        let definition =
            emit_fn_with_callees(ws, &g.mf, &g.init_name, &st.names, &hoisted, &st.gctx)?;
        out.push_str(&definition);
        if g.is_const {
            const_chunks.entry(g.name).or_insert_with(String::new).push_str(&definition);
        } else {
            stable.push_str(&definition);
        }
        for nf in &g.mf.nested_fns {
            let definition =
                emit_fn_with_callees(ws, nf, nf.name.as_str(), &st.names, &hoisted, &st.gctx)?;
            out.push_str(&definition);
            if g.is_const {
                const_chunks.entry(g.name).or_insert_with(String::new).push_str(&definition);
            } else {
                stable.push_str(&definition);
            }
        }
    }
    // 4. load-time init lines (dependency-ordered).
    let mut const_order = Vec::new();
    for init in order_global_inits(&st.ginits, &st.lowered, &st.names)? {
        out.push_str(&init.js);
        if let Some(chunk) = const_chunks.get_mut(&init.name) {
            chunk.push_str(&init.js);
            const_order.push(init.name);
        } else {
            stable.push_str(&init.js);
        }
    }
    // 5. decorator applications (AFTER inits: a deco body pushes into a ginit-
    //    initialized registry -- D4).
    let applications = decorator_application_lines_js(ws, &deco_names, &st.names)?;
    out.push_str(&applications);
    stable.push_str(&applications);

    Ok(ProgramBuild {
        program: out,
        stable,
        chunks,
        const_chunks,
        const_order,
    })
}

/// `rock_shader(vert, frag)` collapses to a pipeline key const; the driver emits
/// one top-level `__rock_shader_register(key, glsl)` per distinct pair (the JS
/// assembler mirrors skull's Lua `shader_registration_lines`). Those calls are
/// raw driver text, not MIR, so the register fn is never discovered
/// transitively -- force its js sibling into the program under its bare
/// `@nomangle` name whenever a pair exists, so the register lines resolve. The
/// native Rock body (WATCHER lookup + GL compile) is not JS-emittable, so a
/// program with a shader pair but no reachable js sibling REJECTS rather than
/// silently dropping the registration (P2, docs/stone-web-plan.md).
fn force_shader_register_sibling(
    ws: &Workspace,
    st: &mut WorklistState,
) -> Result<(), CompilerError> {
    if skull_mir::lower::collect_shader_pairs(ws)?.is_empty() {
        return Ok(());
    }
    let name = ustr::Ustr::from("__rock_shader_register");
    let cands = ws.functions_by_name.get(&name).map(|l| l.as_slice()).unwrap_or(&[]);
    // Exact-JS sibling first (the intrinsics precedence doctrine), else the
    // portable plain-Rock body (the engine's real watcher.rock registration
    // path, JS-emittable once its inline(lua) helpers have js siblings).
    let target = cands
        .iter()
        .find(|(m, f)| {
            let cf = &ws.module(*m).functions[*f];
            cf.backend == bone_semantic::hir::FnBackend::Js
                && (matches!(cf.inline_body, Some((bone_semantic::hir::FnBackend::Js, _))) || cf.is_extern)
        })
        .or_else(|| {
            cands.iter().find(|(m, f)| {
                let cf = &ws.module(*m).functions[*f];
                cf.inline_body.is_none() && !cf.is_extern
            })
        });
    let &(m, f) = target.ok_or_else(|| {
        err(
            "JS: `rock_shader` pipelines emit `__rock_shader_register` calls, but no \
             JS-servable `__rock_shader_register` is reachable (inline(js)/extern(js) \
             sibling or a plain-Rock body); refusing to drop the shader registration",
        )
    })?;
    let fr = FuncRef { module: m, func: f };
    if st.seen.insert(fr) {
        // Bare @nomangle name: the driver register lines call it verbatim.
        st.names.insert(fr, name.to_string());
        let cf = &ws.module(m).functions[f];
        if matches!(cf.inline_body, Some((bone_semantic::hir::FnBackend::Js, _))) {
            st.inline_js.push(fr);
            scan_inline_js_body(ws, fr, &mut st.names, &mut st.queue, &mut st.seen, &mut st.inline_js);
        } else if cf.inline_body.is_none() && !cf.is_extern {
            st.queue.push(fr);
        }
        // extern(js): runtime provides the global -- no def emitted.
    }
    Ok(())
}

/// A whole-program JS emission plus the entry fn's JS name (`None` when the entry
/// module has no nullary `main`; the driver then appends nothing and the program
/// is module-load effects only, matching luajit).
pub struct JsProgram {
    pub program: String,
    pub main_js_name: Option<String>,
}

struct JsChunk {
    js: String,
    statics: Vec<skull_mir::mir::StaticCell>,
}

struct ProgramBuild {
    program: String,
    stable: String,
    chunks: HashMap<FuncRef, JsChunk>,
    const_chunks: HashMap<ustr::Ustr, String>,
    const_order: Vec<ustr::Ustr>,
}

pub struct JsEmitCache {
    build: ProgramBuild,
    changed_since_full: HashSet<FuncRef>,
    static_keys: HashMap<ustr::Ustr, String>,
}

impl JsEmitCache {
    pub fn current_program(&self) -> &str {
        &self.build.program
    }
}

pub enum JsReload {
    Overlay(String),
    FullRequired,
    NoChange,
}

impl JsReload {
    pub fn expect_overlay(self) -> String {
        match self {
            Self::Overlay(js) => js,
            Self::FullRequired => panic!("JS overlay required a full reload"),
            Self::NoChange => panic!("JS overlay reported no emitted change"),
        }
    }
}

pub fn emit_overlay(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    user_mid: bone_semantic::prelude::ModuleId,
    cache: &mut JsEmitCache,
    reinferred: &[(bone_semantic::prelude::ModuleId, bone_semantic::hir::BodyOwner)],
) -> Result<JsReload, CompilerError> {
    if reinferred
        .iter()
        .any(|(_, owner)| {
            !matches!(
                owner,
                bone_semantic::hir::BodyOwner::Function(_)
                    | bone_semantic::hir::BodyOwner::Global(_)
            )
        })
    {
        return Ok(JsReload::FullRequired);
    }

    let mut roots = Vec::new();
    if let Some(main) = nullary_main(ws, user_mid) {
        roots.push((main, callee_js_name(ws, main)));
    }
    let current = build_program_core(ws, scx, &roots)?;
    if current.stable != cache.build.stable {
        return Ok(JsReload::FullRequired);
    }
    if current.const_chunks.keys().collect::<HashSet<_>>()
        != cache.build.const_chunks.keys().collect::<HashSet<_>>()
    {
        return Ok(JsReload::FullRequired);
    }

    let mut changed_now = HashSet::new();
    for (fref, chunk) in &current.chunks {
        if cache
            .build
            .chunks
            .get(fref)
            .is_none_or(|previous| previous.js != chunk.js)
        {
            changed_now.insert(*fref);
        }
    }
    let changed_consts: HashSet<ustr::Ustr> = current
        .const_chunks
        .iter()
        .filter_map(|(name, chunk)| {
            (cache.build.const_chunks.get(name) != Some(chunk)).then_some(*name)
        })
        .collect();
    if changed_now.is_empty() && changed_consts.is_empty() {
        cache.build = current;
        return Ok(JsReload::NoChange);
    }
    cache.changed_since_full.extend(changed_now.iter().copied());

    let mut out = String::from("// skull JS incremental overlay\n");
    for name in &current.const_order {
        if changed_consts.contains(name) {
            out.push_str(&current.const_chunks[name]);
        }
    }
    let mut reset_cells = HashSet::new();
    for fref in &changed_now {
        let Some(chunk) = current.chunks.get(fref) else { continue };
        for cell in &chunk.statics {
            let key = static_cell_key(cell);
            if cache
                .static_keys
                .get(&cell.cell)
                .is_none_or(|previous| *previous != key)
                && reset_cells.insert(cell.cell)
            {
                out.push_str(&format!(
                    "__G.{} = undefined; __G.{} = false;\n",
                    cell.cell, cell.done_flag
                ));
            }
            cache.static_keys.insert(cell.cell, key);
        }
    }

    let mut order: Vec<FuncRef> = cache.changed_since_full.iter().copied().collect();
    order.sort_by_key(|fref| (fref.module.index(), u32::from(fref.func.into_raw())));
    for fref in &order {
        if let Some(chunk) = current.chunks.get(fref) {
            out.push_str(&chunk.js);
        }
    }

    let decorator_names = decorator_names(ws);
    let decorated: Vec<FuncRef> = order
        .iter()
        .copied()
        .filter(|fref| {
            ws.module(fref.module).functions[fref.func]
                .annotations
                .iter()
                .any(|annotation| decorator_names.contains(&annotation.name.name))
        })
        .collect();
    if !decorated.is_empty() {
        let Some(rebind) = exact_inline_js(ws, "__rock_rebind_system") else {
            return Ok(JsReload::FullRequired);
        };
        out.push_str(&emit_inline_js_fn(ws, scx, rebind, "__rock_rebind_system"));
        for fref in decorated {
            let function = &ws.module(fref.module).functions[fref.func];
            out.push_str(&format!(
                "__rock_rebind_system({}, {});\n",
                js_string_lit(function.name.as_str())?,
                callee_js_name(ws, fref)
            ));
        }
    }

    if let Some(wire) = find_plain_function(ws, "run_wire_systems") {
        let wire_name = callee_js_name(ws, wire);
        if current.chunks.contains_key(&wire) {
            out.push_str(&format!("{wire_name}();\n"));
        }
    }

    cache.build = current;
    Ok(JsReload::Overlay(out))
}

fn static_cell_key(cell: &skull_mir::mir::StaticCell) -> String {
    format!("{}@{:x}", cell.type_str, cell.init_hash)
}

fn collect_statics(function: &MirFunc) -> Vec<skull_mir::mir::StaticCell> {
    let mut cells = function.statics.clone();
    for nested in &function.nested_fns {
        cells.extend(collect_statics(nested));
    }
    cells
}

fn exact_inline_js(ws: &Workspace, name: &str) -> Option<FuncRef> {
    ws.functions_by_name
        .get(&ustr::Ustr::from(name))?
        .iter()
        .find_map(|(module, function)| {
            matches!(
                ws.module(*module).functions[*function].inline_body,
                Some((bone_semantic::hir::FnBackend::Js, _))
            )
            .then_some(FuncRef { module: *module, func: *function })
        })
}

fn find_plain_function(ws: &Workspace, name: &str) -> Option<FuncRef> {
    ws.functions_by_name
        .get(&ustr::Ustr::from(name))?
        .iter()
        .find_map(|(module, function)| {
            let function_ref = &ws.module(*module).functions[*function];
            (function_ref.inline_body.is_none() && !function_ref.is_extern)
                .then_some(FuncRef { module: *module, func: *function })
        })
}

/// The shared worklists driving [`emit_with_closure`] and [`emit_program`]:
/// a fn queue (roots + transitively discovered plain-Rock callees) and a global-
/// init queue (module globals the emitted bodies read), plus the accumulated
/// `gctx`/`names` a body needs to render its `Inst::Global` reads.
struct WorklistState {
    cache: skull_transform::InlineCache,
    names: HashMap<FuncRef, String>,
    queue: Vec<FuncRef>,
    seen: HashSet<FuncRef>,
    lowered: Vec<(FuncRef, MirFunc)>,
    /// `inline(js)` callees, emitted verbatim (their raw JS body, not MIR-lowered).
    inline_js: Vec<FuncRef>,
    state_index: HashMap<ustr::Ustr, GlobalSite>,
    /// Empty in program mode (applications ARE emitted, so a registry read is
    /// legitimate); the per-test reject set otherwise.
    deco_globals: HashSet<ustr::Ustr>,
    enum_defs: HashMap<ustr::Ustr, String>,
    gctx: GlobalCtx,
    ginit_queue: Vec<(ustr::Ustr, GlobalSite)>,
    ginit_seen: HashSet<ustr::Ustr>,
    ginits: Vec<GlobalInit>,
}

impl WorklistState {
    fn new(ws: &Workspace, roots: &[(FuncRef, String)], program_mode: bool) -> Self {
        WorklistState {
            cache: skull_transform::InlineCache::default(),
            names: roots.iter().cloned().collect(),
            queue: roots.iter().map(|(fr, _)| *fr).collect(),
            seen: roots.iter().map(|(fr, _)| *fr).collect(),
            lowered: Vec::new(),
            inline_js: Vec::new(),
            state_index: build_state_index(ws),
            deco_globals: if program_mode { HashSet::new() } else { decorator_populated_globals(ws) },
            enum_defs: build_enum_global_defs(ws),
            gctx: GlobalCtx::default(),
            ginit_queue: Vec::new(),
            ginit_seen: HashSet::new(),
            ginits: Vec::new(),
        }
    }

    /// Every emitted top-level body (fn defs + ginit init fns) for the cross-
    /// cutting passes (arity guard, hoisted-name set, vt_eq). Excludes nested
    /// (lambda) fns, matching the pre-refactor scan.
    fn top_bodies(&self) -> Vec<&MirFunc> {
        self.lowered
            .iter()
            .map(|(_, mf)| mf)
            .chain(self.ginits.iter().map(|g| &g.mf))
            .collect()
    }

    fn canonicalize_emission_order(&mut self) {
        self.lowered
            .sort_by_key(|(fr, _)| (fr.module.index(), u32::from(fr.func.into_raw())));
        self.inline_js
            .sort_by_key(|fr| (fr.module.index(), u32::from(fr.func.into_raw())));
        self.ginits.sort_by_key(|g| g.order);
    }
}

/// Emit an `inline(js)` fn verbatim: `function <js_name>(p1, p2) { <raw js> }`.
/// The body is the author's raw JS, spliced unchanged (mirrors the luajit path's
/// verbatim `inline(lua)` emission in `skull::mir_emit`).
fn emit_inline_js_fn(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    fr: FuncRef,
    js_name: &str,
) -> String {
    let f = &ws.module(fr.module).functions[fr.func];
    // A @scalar_abi fn's value-struct params explode into their
    // `<name>__<leaf>` scalars, same as the lua path, so the exploded call
    // sites and the raw body's leaf names agree.
    let params = match skull_mir::lower::scalar_abi_lua_param_names(ws, scx, fr) {
        Some(names) => names.join(", "),
        None => f
            .param_decls
            .iter()
            .map(|p| p.name.to_string())
            .collect::<Vec<_>>()
            .join(", "),
    };
    let src = match f.inline_body {
        Some((bone_semantic::hir::FnBackend::Js, s)) => s.to_string(),
        _ => String::new(),
    };
    format!("function {js_name}({params}) {{\n{src}\n}}\n")
}

/// Drain the fn + ginit worklists: lower each queued fn (same per-body pipeline
/// the luajit path runs -- lower -> inline -> cleanup -> closures -> FORL ->
/// residual-bound-call validation; coroutines skip inline/FORL), discover its
/// callees + module-global reads (feeding both queues), and record the lowered
/// body. A ginit is a lowered `lower_global` returning the init value (never
/// inlined, mirroring the luajit global path).
/// Register a body's `static` locals as `__G` state: each lowers to a private
/// `__static_*` cell + done-flag global (`lower_static`). No initializer line
/// is needed -- an undefined done-flag is falsy, so the first-reach guard runs
/// the init exactly as Lua's nil does.
fn register_statics(mf: &MirFunc, gctx: &mut GlobalCtx) {
    for sc in &mf.statics {
        gctx.state.insert(sc.cell);
        gctx.state.insert(sc.done_flag);
    }
    for nf in &mf.nested_fns {
        register_statics(nf, gctx);
    }
}

/// A unit of JS reachability work. The search walks TWO node kinds -- a
/// plain-Rock fn to lower, and a module global whose initializer must be emitted
/// -- and either can discover the other, which is why the shared driver is
/// generic over its node type instead of walking bare `FuncRef`s.
///
/// A ginit is keyed by NAME only: `state_index` is the single map from a global's
/// name to its definition site, so the site is derivable and the name is what
/// `ginit_seen` already dedupes on.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum JsNode {
    Fn(FuncRef),
    Ginit(ustr::Ustr),
}

/// The JS half of [`skull_mir::reach`]: roots and successors, with cycles
/// allowed (mutual recursion is ordinary Rock) and no leaf class.
///
/// JS has no leaves at the node level because it filters them one step earlier,
/// at EDGE discovery: `discover_callees` must classify an intrinsic / host-mapped
/// / `inline(js)` callee anyway in order to REGISTER it (a bare name, a verbatim
/// co-emission), so a non-expandable callee never becomes a node in the first
/// place. GLSL, which registers nothing, filters at the node instead. The trait
/// serves both; `is_leaf` is the hook for the latter shape.
struct JsReach<'a> {
    ws: &'a Workspace,
    scx: &'a skull_mir::lower::ScalarCtx,
    st: &'a mut WorklistState,
}

impl JsReach<'_> {
    /// Everything `discover_*`/the root seeding has queued since the last drain,
    /// as nodes. Both queues are emptied: the driver owns the traversal from here.
    fn take_pending(&mut self) -> Vec<JsNode> {
        let mut out: Vec<JsNode> = std::mem::take(&mut self.st.queue).into_iter().map(JsNode::Fn).collect();
        out.extend(
            std::mem::take(&mut self.st.ginit_queue).into_iter().map(|(gname, _)| JsNode::Ginit(gname)),
        );
        out
    }

    /// Lower + transform one fn through the same per-body pipeline the luajit
    /// path runs (lower -> inline -> cleanup -> closures -> FORL -> residual-
    /// bound-call validation; coroutines skip inline/FORL), then record it.
    fn visit_fn(&mut self, fr: FuncRef) -> Result<(), CompilerError> {
        let f = &self.ws.module(fr.module).functions[fr.func];
        let mut mf = skull_mir::lower::lower_function(self.ws, self.scx, fr.module, fr.func)?;
        if mf.is_coroutine {
            skull_transform::cleanup_with_nobox(self.ws, &mut mf);
        } else {
            skull_transform::inline_functions(self.ws, self.scx, fr, &mut mf, &mut self.st.cache)?;
            skull_transform::cleanup_with_nobox(self.ws, &mut mf);
            skull_transform::convert_closures(&mut mf, &self.st.names[&fr]);
            skull_transform::recover_counted_loops(self.ws, &mut mf);
            for nf in &mut mf.nested_fns {
                skull_transform::recover_counted_loops(self.ws, nf);
            }
        }
        skull_transform::validate_no_residual_bound_calls(self.ws, f, &mf)?;
        nobox_errs(self.ws, &mf)?;
        validate_js_nobox_return(self.ws, f, &mf)?;
        for nf in &mf.nested_fns {
            nobox_errs(self.ws, nf)?;
        }
        self.discover(&mf)?;
        self.st.lowered.push((fr, mf));
        Ok(())
    }

    /// Lower one module global's initializer (never inlined, mirroring the luajit
    /// global path). A global with no initializer contributes no node body.
    fn visit_ginit(&mut self, gname: ustr::Ustr) -> Result<(), CompilerError> {
        let site = self.st.state_index[&gname];
        let init_name = global_init_js_name(gname);
        let lowered = skull_mir::lower::lower_global(
            self.ws, self.scx, site.module, site.gid, ustr::Ustr::from(init_name.as_str()),
        )?;
        let Some(mut mf) = lowered else { return Ok(()) };
        skull_transform::cleanup_with_nobox(self.ws, &mut mf);
        skull_transform::convert_closures(&mut mf, &init_name);
        skull_transform::recover_counted_loops(self.ws, &mut mf);
        for nf in &mut mf.nested_fns {
            skull_transform::recover_counted_loops(self.ws, nf);
        }
        nobox_errs(self.ws, &mf)?;
        for nf in &mf.nested_fns {
            nobox_errs(self.ws, nf)?;
        }
        self.discover(&mf)?;
        let is_const = self.ws.module(site.module).globals[site.gid].is_const;
        self.st.ginits.push(GlobalInit {
            name: gname,
            init_name,
            mf,
            is_const,
            order: (site.module.index(), u32::from(site.gid.into_raw())),
        });
        Ok(())
    }

    fn discover(&mut self, mf: &MirFunc) -> Result<(), CompilerError> {
        let st = &mut *self.st;
        register_statics(mf, &mut st.gctx);
        discover_callees(self.ws, mf, &mut st.names, &mut st.queue, &mut st.seen, &mut st.inline_js)?;
        discover_globals(
            self.ws, mf, &st.state_index, &st.deco_globals, &st.enum_defs, &mut st.gctx,
            &mut st.ginit_queue, &mut st.ginit_seen, &mut st.names, &mut st.queue, &mut st.seen,
            &mut st.inline_js,
        )
    }
}

impl skull_mir::reach::ReachPolicy for JsReach<'_> {
    type Node = JsNode;

    fn roots(&mut self) -> Result<Vec<JsNode>, CompilerError> {
        Ok(self.take_pending())
    }

    fn is_leaf(&mut self, _node: JsNode) -> Result<bool, CompilerError> {
        Ok(false)
    }

    fn on_cycle(&mut self, _node: JsNode) -> Result<(), CompilerError> {
        Ok(())
    }

    fn visit(&mut self, node: JsNode) -> Result<Vec<JsNode>, CompilerError> {
        match node {
            JsNode::Fn(fr) => self.visit_fn(fr)?,
            JsNode::Ginit(gname) => self.visit_ginit(gname)?,
        }
        Ok(self.take_pending())
    }
}

/// Expand the reachable program from whatever the root seeding left on the
/// worklists. Emission order is canonicalized afterwards, so the driver's
/// post-order does not reach the output.
fn drain_worklists(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    st: &mut WorklistState,
) -> Result<(), CompilerError> {
    skull_mir::reach::walk(&mut JsReach { ws, scx, st })?;
    Ok(())
}

/// Emit one lowered TOP body: a coroutine through the trampoline pipeline, else
/// the ordinary fn path plus its hoisted (`convert_closures`) sibling bodies.
fn emit_lowered_fn(
    ws: &Workspace,
    fr: FuncRef,
    mf: &MirFunc,
    js_name: &str,
    names: &HashMap<FuncRef, String>,
    hoisted: &HashSet<ustr::Ustr>,
    gctx: &GlobalCtx,
) -> Result<String, CompilerError> {
    if mf.is_coroutine {
        return emit_coroutine_js(ws, fr, mf, js_name, names, hoisted, gctx);
    }
    let mut out = emit_fn_with_callees(ws, mf, js_name, names, hoisted, gctx)?;
    for nf in &mf.nested_fns {
        out.push_str(&emit_fn_with_callees(ws, nf, nf.name.as_str(), names, hoisted, gctx)?);
    }
    Ok(out)
}

/// Scalar-ABI mismatch guard: a value-struct arg scalarizes to N leaf values at
/// the call, but a co-emitted callee that uses the param as a WHOLE value keeps
/// a single MIR param -- so `argc > arity` would silently drop leaves. Residual
/// co-emitted callees were NOT inlined (whole-value use blocks eligibility), so
/// reject rather than emit-and-hope. `argc < arity` down to the count of params
/// WITHOUT a default is the legitimate omitted-defaulted-arg shape.
fn check_scalar_abi(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    lowered: &[(FuncRef, MirFunc)],
    bodies: &[&MirFunc],
) -> Result<(), CompilerError> {
    let arities: HashMap<FuncRef, usize> =
        lowered.iter().map(|(fr, mf)| (*fr, mf.params.len())).collect();
    for mf in bodies {
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
                let (callee, argc) = match &mf.insts[iid] {
                    Inst::Call { func, args } | Inst::CallMulti { func, args, .. } => (func, args.len()),
                    _ => continue,
                };
                if let Some(&arity) = arities.get(callee) {
                    let required = required_arg_count(ws, scx, *callee, arity);
                    if argc < required || argc > arity {
                        let cf = &ws.module(callee.module).functions[callee.func];
                        return Err(err(format!(
                            "JS subset: value-struct scalar ABI mismatch calling `{}` \
                             ({argc} args vs {required}..={arity} params; callee not inlined)",
                            cf.name
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Program-wide hoisted closure-body names -- the only globals a capture-free
/// lambda creation site may reference. Flat: nesting is one level.
fn collect_hoisted(bodies: &[&MirFunc]) -> HashSet<ustr::Ustr> {
    bodies.iter().flat_map(|mf| mf.nested_fns.iter().map(|nf| nf.name)).collect()
}

/// Boxed-enum singleton / names-table defs the emitted bodies read, in name
/// order (deterministic; `var` redeclare is harmless on concat).
fn emit_enum_singletons(gctx: &GlobalCtx, enum_defs: &HashMap<ustr::Ustr, String>) -> String {
    let mut enum_names: Vec<&ustr::Ustr> = gctx.enum_globals.iter().collect();
    enum_names.sort();
    let mut out = String::new();
    for name in enum_names {
        out.push_str(&enum_defs[name]);
    }
    out
}

/// Per-type `__rock_vt_eq` registry for every GC-carrying value struct the
/// program constructs (closed transitively over GC-carrying value-struct fields).
fn emit_vt_eq(ws: &Workspace, bodies: &[&MirFunc]) -> Result<String, CompilerError> {
    let mut vt_names: BTreeSet<ustr::Ustr> = BTreeSet::new();
    for mf in bodies {
        collect_gc_value_structs(ws, mf, &mut vt_names);
    }
    close_gc_value_struct_fields(ws, &mut vt_names);
    build_vt_eq_defs(ws, &vt_names)
}

/// The `__deco_<name>` JS identity for a decorator (matches skull's Lua
/// `decorator_fn_name`; the two backends share the naming scheme).
fn decorator_js_name(deco: &str) -> String {
    format!("__deco_{deco}")
}

/// Every `deco`-definition name in the workspace -- the annotation names that are
/// decorator APPLICATIONS (vs plain `@test`/`@sim`/... annotations).
fn decorator_names(ws: &Workspace) -> HashSet<ustr::Ustr> {
    let mut set = HashSet::new();
    for module in ws.modules.iter() {
        for (_, d) in module.decorators.iter() {
            set.insert(d.name);
        }
    }
    set
}

/// The decorator names actually APPLIED (some fn carries an `@<deco>`
/// annotation) -- the reachable subset of [`decorator_names`]. An unapplied
/// `deco` def is never emitted (D1).
fn applied_decorator_names(ws: &Workspace, deco_names: &HashSet<ustr::Ustr>) -> HashSet<ustr::Ustr> {
    let mut set = HashSet::new();
    for module in ws.modules.iter() {
        if module.id == bone_semantic::prelude::ModuleId::builtin() {
            continue;
        }
        for (_, f) in module.functions.iter() {
            for ann in &f.annotations {
                if deco_names.contains(&ann.name.name) {
                    set.insert(ann.name.name);
                }
            }
        }
    }
    set
}

/// The entry module's nullary `main`, if it defines one -- the D3 program entry.
fn nullary_main(ws: &Workspace, user_mid: bone_semantic::prelude::ModuleId) -> Option<FuncRef> {
    ws.module(user_mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "main" && f.param_decls.is_empty())
        .map(|(fid, _)| FuncRef { module: user_mid, func: fid })
}

/// A `self_ref` for inlining a body that is NOT a function (a `deco` def): any
/// workspace `FuncRef` the body does not itself call, so [`inline_functions`]'s
/// self-call skip (`fref != self_ref`) never suppresses a callee the body wants
/// inlined. Deco bodies never call a decorated user fn, but computing the actual
/// call set makes the choice provably safe regardless.
fn safe_self_ref(ws: &Workspace, mf: &MirFunc) -> FuncRef {
    let targets: HashSet<FuncRef> = skull_mir::reach::call_targets(mf).into_iter().collect();
    ws.modules
        .iter()
        .filter(|m| m.id != bone_semantic::prelude::ModuleId::builtin())
        .flat_map(|m| m.functions.iter().map(move |(fid, _)| FuncRef { module: m.id, func: fid }))
        .find(|fr| !targets.contains(fr))
        .expect("workspace has a function the deco body does not call")
}

/// A JS literal for a `@deco` annotation arg value (the JS analog of skull's
/// `annotation_value_lua`): the same value classes, rendered in JS syntax. A
/// non-literal (`AnnoValue::Other`) yields `None` (the caller rejects).
pub fn annotation_value_js(v: &bone_semantic::hir::AnnoValue) -> Option<String> {
    use bone_semantic::hir::AnnoValue as A;
    Some(match v {
        A::Int(i) => i.to_string(),
        A::Float(f) => js_number(*f).ok()?,
        A::Bool(b) => b.to_string(),
        A::String(s) => js_string_lit(s.as_str()).ok()?,
        A::Ident(id) => id.name.as_str().to_string(),
        A::Other => return None,
    })
}

/// The JS argument list for one `@deco` application (the JS sibling of skull's
/// `decorator_call_args`): the SELECTION (named arg / defaulted `null` / missing-
/// required error) is the shared `skull_mir::decorator::select_decorator_args`;
/// only literal rendering (`annotation_value_js`) and the `null` default token
/// are JS-specific.
fn decorator_call_args_js(
    d: &bone_semantic::hir::DecoratorDef,
    ann: &bone_semantic::hir::Annotation,
) -> Result<Vec<String>, CompilerError> {
    use skull_mir::decorator::DecoArg;
    let mut args = Vec::new();
    for choice in skull_mir::decorator::select_decorator_args(d, ann)? {
        args.push(match choice {
            DecoArg::Value(v) => annotation_value_js(v).ok_or_else(|| {
                err(format!(
                    "JS subset: decorator `{}` arg is a non-literal expression (unsupported)",
                    d.name
                ))
            })?,
            DecoArg::Default => "null".to_string(),
        });
    }
    Ok(args)
}

/// Decorator APPLICATION lines for every decorated fn workspace-wide (the JS
/// sibling of skull's Lua `decorator_application_lines`): one
/// `__deco_<name>({name: "..", params: [], func: <js name>}, <args>);` per
/// `@<deco>` annotation, in module then fn source order. The descriptor mirrors
/// luajit's bare `{ name, params, func }` table (no `__ty`: reads render as plain
/// `.name`/`.func`, and no deco body eq-compares `target`); `params` is an inert
/// `[]` placeholder (never read by a deco body, same as luajit's `{}`).
fn decorator_application_lines_js(
    ws: &Workspace,
    deco_names: &HashSet<ustr::Ustr>,
    names: &HashMap<FuncRef, String>,
) -> Result<String, CompilerError> {
    let deco_map: HashMap<ustr::Ustr, (bone_semantic::prelude::ModuleId, Idx<bone_semantic::hir::DecoratorDef>)> = ws
        .modules
        .iter()
        .flat_map(|m| m.decorators.iter().map(move |(did, d)| (d.name, (m.id, did))))
        .collect();
    let mut out = String::new();
    for module in ws.modules.iter() {
        if module.id == bone_semantic::prelude::ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            for ann in &f.annotations {
                if !deco_names.contains(&ann.name.name) {
                    continue;
                }
                let &(dmid, did) = deco_map.get(&ann.name.name).expect("decorator name resolves");
                let d = &ws.module(dmid).decorators[did];
                let fr = FuncRef { module: module.id, func: fid };
                let js_fn = names.get(&fr).ok_or_else(|| {
                    err(format!(
                        "JS subset: decorated fn `{}` was not emitted (not a program root)",
                        f.name
                    ))
                })?;
                let descriptor = format!(
                    "{{name: {}, params: [], func: {js_fn}}}",
                    js_string_lit(f.name.as_str())?
                );
                let mut call_args = vec![descriptor];
                call_args.extend(decorator_call_args_js(d, ann)?);
                out.push_str(&format!("{}({});\n", decorator_js_name(ann.name.name.as_str()), call_args.join(", ")));
            }
        }
    }
    Ok(out)
}

/// The definition site of a module-level global (for lowering its initializer).
#[derive(Clone, Copy)]
struct GlobalSite {
    module: bone_semantic::prelude::ModuleId,
    gid: Idx<bone_semantic::hir::GlobalDef>,
}

/// A lowered global initializer: `__G.<name> = <init_name>()` runs the `mf` body.
struct GlobalInit {
    name: ustr::Ustr,
    init_name: String,
    mf: MirFunc,
    is_const: bool,
    order: (usize, u32),
}

struct GlobalInitLine {
    name: ustr::Ustr,
    js: String,
}

/// Deterministic init-fn name for a module global. Global names are unique
/// workspace-wide (`Workspace::globals_by_name` is a flat map), so a bare
/// `__ginit_<name>` cannot collide across independently-emitted test programs
/// (byte-identical definitions dedup under JS function-decl hoisting).
fn global_init_js_name(name: ustr::Ustr) -> String {
    format!("__ginit_{name}")
}

/// Global names referenced by any `deco` body. A decorator runs at MODULE LOAD
/// for each `@<deco>` application and typically mutates a registry global (e.g.
/// `@system` -> `UPDATE_SYSTEMS.push(...)`); the per-test JS emission does NOT
/// emit decorator applications, so a test reading such a global would see the
/// un-populated init value -- an emit-and-hope divergence. Reads of these reject
/// (SKIP) rather than silently diverge.
fn decorator_populated_globals(ws: &Workspace) -> HashSet<ustr::Ustr> {
    use bone_semantic::hir::{BodyOwner, Expr};
    let mut set = HashSet::new();
    for module in &ws.modules {
        for (did, _) in module.decorators.iter() {
            let Some(body) = module.bodies.get(&BodyOwner::Decorator(did)) else {
                continue;
            };
            for (_, expr) in body.expr_arena.iter() {
                if let Expr::Global(name) = expr {
                    set.insert(*name);
                }
            }
        }
    }
    set
}

/// Map every module-level `let`/`global` name to its definition site.
fn build_state_index(ws: &Workspace) -> HashMap<ustr::Ustr, GlobalSite> {
    let mut m = HashMap::new();
    for module in &ws.modules {
        for (&name, &gid) in &module.global_by_name {
            m.insert(name, GlobalSite { module: module.id, gid });
        }
    }
    m
}

/// Discover the plain-Rock callees `mf` calls directly (post-inline residual
/// calls), queueing each for co-emission under [`callee_js_name`]. Mirrors
/// `render_call`'s dispatch so both agree on what NOT to reject.
fn discover_callees(
    ws: &Workspace,
    mf: &MirFunc,
    names: &mut HashMap<FuncRef, String>,
    queue: &mut Vec<FuncRef>,
    seen: &mut HashSet<FuncRef>,
    inline_js: &mut Vec<FuncRef>,
) -> Result<(), CompilerError> {
    for callee in skull_mir::reach::call_targets(mf) {
        let cf = &ws.module(callee.module).functions[callee.func];
        // Only the BUILTIN Map renders inline (skip its co-emission); NativeBuffer
        // via the intrinsic table; Vec and user-struct `get` co-emit (fall through).
        if cf.name.as_str() == "get"
            && cf.param_decls.len() == 2
            && first_param_is_builtin_map(ws, cf)
        {
            continue;
        }
        if is_mapped_intrinsic(cf) || is_host_mapped(cf) || is_map_new(cf) {
            continue;
        }
        // `inline(js)`: co-emit its raw JS body verbatim (never MIR-lowered). The
        // FuncRef is already the JS variant of a same-name pair (`resolve_funcref`
        // swapped it at lowering).
        if matches!(cf.inline_body, Some((bone_semantic::hir::FnBackend::Js, _))) {
            if seen.insert(callee) {
                names.insert(callee, callee_js_name(ws, callee));
                inline_js.push(callee);
                scan_inline_js_body(ws, callee, names, queue, seen, inline_js);
            }
            continue;
        }
        // `extern(js)`: a JS runtime-/driver-provided global, called by its bare
        // name (the `__rock_env_var` glue pattern). No def is emitted here.
        if cf.is_extern && cf.backend == bone_semantic::hir::FnBackend::Js {
            names.entry(callee).or_insert_with(|| cf.name.to_string());
            continue;
        }
        if cf.inline_body.is_some() || cf.is_extern {
            return Err(err(format!(
                "JS subset: call to `{}` is not a mapped JS intrinsic",
                cf.name
            )));
        }
        if seen.insert(callee) {
            names.insert(callee, callee_js_name(ws, callee));
            queue.push(callee);
        }
    }
    Ok(())
}

/// Scan an emitted `inline(js)` fn's verbatim body for identifiers naming a
/// workspace `@nomangle` fn, and root each match under its BARE name -- the JS
/// half of the `@nomangle` discipline. A verbatim body is invisible to the MIR
/// callee walk, so without this the web boot seam (`stone_run_game` inline(js)
/// variant -> `@nomangle` Rock `web_run_game`/`web_run_frame`) never emits its
/// targets. Plain-Rock matches queue for normal lowering; inline(js) matches
/// co-emit verbatim (and are scanned recursively); extern(js) matches need no
/// def (prelude-provided); host-lua-only matches are skipped (a textual match
/// in a comment must not force an unemittable body -- the reject loop or the
/// missing-global runtime error catches a real call).
fn scan_inline_js_body(
    ws: &Workspace,
    fr: FuncRef,
    names: &mut HashMap<FuncRef, String>,
    queue: &mut Vec<FuncRef>,
    seen: &mut HashSet<FuncRef>,
    inline_js: &mut Vec<FuncRef>,
) {
    let f = &ws.module(fr.module).functions[fr.func];
    let Some((bone_semantic::hir::FnBackend::Js, body)) = f.inline_body else { return };
    let mut idents: HashSet<&str> = HashSet::new();
    let s = body.as_str();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < bytes.len() && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            idents.insert(&s[start..i]);
        } else {
            i += 1;
        }
    }
    for ident in idents {
        let name = ustr::Ustr::from(ident);
        let Some(cands) = ws.functions_by_name.get(&name) else { continue };
        for &(m, fid) in cands.iter() {
            let cf = &ws.module(m).functions[fid];
            if !is_nomangle(cf) {
                continue;
            }
            let target = FuncRef { module: m, func: fid };
            if matches!(cf.inline_body, Some((bone_semantic::hir::FnBackend::Js, _))) {
                if seen.insert(target) {
                    names.insert(target, cf.name.to_string());
                    inline_js.push(target);
                    scan_inline_js_body(ws, target, names, queue, seen, inline_js);
                }
            } else if cf.inline_body.is_none() && !cf.is_extern {
                if seen.insert(target) {
                    names.insert(target, cf.name.to_string());
                    queue.push(target);
                }
            }
        }
    }
}

/// Classify every `Inst::Global`/`StoreGlobal` name `mf` references: module-level
/// STATE (registered for `__G.<name>` + its initializer queued) or a top-level fn
/// used as a VALUE (registered under its co-emitted JS name + the fn queued).
/// A name that is neither (a hoisted lambda, `error`, an enum name table) is left
/// for the emitter to resolve or reject.
#[allow(clippy::too_many_arguments)]
fn discover_globals(
    ws: &Workspace,
    mf: &MirFunc,
    state_index: &HashMap<ustr::Ustr, GlobalSite>,
    deco_globals: &HashSet<ustr::Ustr>,
    enum_defs: &HashMap<ustr::Ustr, String>,
    gctx: &mut GlobalCtx,
    ginit_queue: &mut Vec<(ustr::Ustr, GlobalSite)>,
    ginit_seen: &mut HashSet<ustr::Ustr>,
    names: &mut HashMap<FuncRef, String>,
    queue: &mut Vec<FuncRef>,
    seen: &mut HashSet<FuncRef>,
    inline_js: &mut Vec<FuncRef>,
) -> Result<(), CompilerError> {
    for name in global_refs(mf) {
        if name.as_str() == "error" {
            continue;
        } else if enum_defs.contains_key(&name) {
            gctx.enum_globals.insert(name);
        } else if let Some(&site) = state_index.get(&name) {
            if deco_globals.contains(&name) {
                return Err(err(format!(
                    "JS subset: global `{name}` is populated by decorator application \
                     (module-load side effect not emitted)"
                )));
            }
            gctx.state.insert(name);
            if ginit_seen.insert(name) {
                ginit_queue.push((name, site));
            }
        } else if let Some(&[(module, func)]) =
            ws.functions_by_name.get(&name).map(|l| l.as_slice())
        {
            // A NON-overloaded top-level fn used as a value.
            let fr = FuncRef { module, func };
            let jsname = callee_js_name(ws, fr);
            gctx.fns.entry(name).or_insert_with(|| jsname.clone());
            if seen.insert(fr) {
                names.insert(fr, jsname);
                queue.push(fr);
            }
        } else if let Some(cands) = ws.functions_by_name.get(&name) {
            // A backend-variant sibling pair (same name, same signature,
            // different backend) used as a VALUE. bone accepts it (collapses to
            // one logical fn) exactly as the call path does; JS resolves to the
            // variant serving this target, mirroring `select_backend_variant`. A
            // real overload (distinct signatures) is rejected by bone before
            // emit, so `select_js_value_variant` only ever sees a sibling group
            // here; it returns `None` (leave rejecting) for a lua-only group.
            if let Some(fr) = select_js_value_variant(ws, &cands) {
                register_fn_value_variant(ws, name, fr, gctx, names, queue, seen, inline_js);
            }
        }
    }
    Ok(())
}

/// The variant of a same-name backend-sibling group that serves the JS target:
/// an exact `Js` decl (inline(js)/extern(js)) wins, else the portable `Any`
/// fallback. `None` means the group has neither -- a lua-only fn taken as a
/// value from JS-reachable code, which stays rejecting (no JS binding).
fn select_js_value_variant(
    ws: &Workspace,
    cands: &[(bone_semantic::prelude::ModuleId, Idx<bone_semantic::hir::Function>)],
) -> Option<FuncRef> {
    use bone_semantic::hir::FnBackend;
    let backend_of =
        |&(m, f): &(bone_semantic::prelude::ModuleId, Idx<bone_semantic::hir::Function>)| {
            ws.module(m).functions[f].backend
        };
    cands
        .iter()
        .find(|c| backend_of(c) == FnBackend::Js)
        .or_else(|| cands.iter().find(|c| backend_of(c) == FnBackend::Any))
        .map(|&(module, func)| FuncRef { module, func })
}

/// Register a fn-value variant `fr` (already selected for this target) the same
/// way `discover_callees` registers a callee by kind: an inline(js) body co-emits
/// verbatim, an extern(js) decl binds its bare driver name (no def), a plain-Rock
/// fn lowers normally. In every case `gctx.fns[name]` maps the surface name (what
/// the `Inst::Global` read carries) to the emitted JS identifier.
#[allow(clippy::too_many_arguments)]
fn register_fn_value_variant(
    ws: &Workspace,
    name: ustr::Ustr,
    fr: FuncRef,
    gctx: &mut GlobalCtx,
    names: &mut HashMap<FuncRef, String>,
    queue: &mut Vec<FuncRef>,
    seen: &mut HashSet<FuncRef>,
    inline_js: &mut Vec<FuncRef>,
) {
    use bone_semantic::hir::FnBackend;
    let cf = &ws.module(fr.module).functions[fr.func];
    if matches!(cf.inline_body, Some((FnBackend::Js, _))) {
        let jsname = callee_js_name(ws, fr);
        gctx.fns.entry(name).or_insert_with(|| jsname.clone());
        if seen.insert(fr) {
            names.insert(fr, jsname);
            inline_js.push(fr);
            scan_inline_js_body(ws, fr, names, queue, seen, inline_js);
        }
    } else if cf.is_extern && cf.backend == FnBackend::Js {
        gctx.fns.entry(name).or_insert_with(|| cf.name.to_string());
        names.entry(fr).or_insert_with(|| cf.name.to_string());
    } else {
        // A portable (`Any`) Rock body -- lower it to JS like any other callee.
        let jsname = callee_js_name(ws, fr);
        gctx.fns.entry(name).or_insert_with(|| jsname.clone());
        if seen.insert(fr) {
            names.insert(fr, jsname);
            queue.push(fr);
        }
    }
}

/// Static callee emit-names of `mf` (`Call`/`CallMulti`), resolved through
/// `names` (+ nested fns). Indirect (fn-value) and bound calls are not statically
/// resolvable and contribute no ordering edge -- parity with the luajit backend's
/// direct-call facts (`skull::mir_emit::direct_calls`).
fn direct_callees(mf: &MirFunc, names: &HashMap<FuncRef, String>) -> Vec<ustr::Ustr> {
    fn walk(mf: &MirFunc, names: &HashMap<FuncRef, String>, out: &mut Vec<ustr::Ustr>) {
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
                let fr = match &mf.insts[iid] {
                    Inst::Call { func, .. } | Inst::CallMulti { func, .. } => *func,
                    _ => continue,
                };
                if let Some(n) = names.get(&fr) {
                    out.push(ustr::Ustr::from(n.as_str()));
                }
            }
        }
        for nf in &mf.nested_fns {
            walk(nf, names, out);
        }
    }
    let mut out = Vec::new();
    walk(mf, names, &mut out);
    out
}

/// Order the global-init assignment lines so each global inits AFTER every global
/// it TRANSITIVELY reads through its call graph: `__G.<name> = <init_name>();`.
/// A shallow "reads in the init body" edge misses `ACT_UI_CONFIRM = action_new(..)`
/// where the `ACTIONS` read is inside `action_new`, not the init body -- so build
/// the transitive global-read closure over every emitted fn (ginits + lowered),
/// exactly the luajit backend's `transitive_global_reads`. A genuine cycle rejects.
fn order_global_inits(
    ginits: &[GlobalInit],
    lowered: &[(FuncRef, MirFunc)],
    names: &HashMap<FuncRef, String>,
) -> Result<Vec<GlobalInitLine>, CompilerError> {
    // Per-fn direct facts, keyed by emit name (ginit = init_name, lowered = js name).
    let mut direct_globals: HashMap<ustr::Ustr, Vec<ustr::Ustr>> = HashMap::new();
    let mut direct_calls: HashMap<ustr::Ustr, Vec<ustr::Ustr>> = HashMap::new();
    let mut record = |key: ustr::Ustr, mf: &MirFunc| {
        direct_globals.insert(key, global_refs(mf));
        direct_calls.insert(key, direct_callees(mf, names));
    };
    for g in ginits {
        record(ustr::Ustr::from(g.init_name.as_str()), &g.mf);
    }
    // Key by the EMIT name (`names[fr]`, mangled) -- that is what `direct_callees`
    // resolves a call to, NOT the unmangled `mf.name`.
    for (fr, mf) in lowered {
        if let Some(emit_name) = names.get(fr) {
            record(ustr::Ustr::from(emit_name.as_str()), mf);
        }
    }

    // Transitive closure: closure[f] |= closure[callee], to a fixpoint.
    let mut id_of: HashMap<ustr::Ustr, usize> = HashMap::new();
    let intern = |s: ustr::Ustr, id_of: &mut HashMap<ustr::Ustr, usize>| -> usize {
        let next = id_of.len();
        *id_of.entry(s).or_insert(next)
    };
    for (f, gs) in direct_globals.iter() {
        intern(*f, &mut id_of);
        for g in gs {
            intern(*g, &mut id_of);
        }
    }
    for (f, cs) in direct_calls.iter() {
        intern(*f, &mut id_of);
        for c in cs {
            intern(*c, &mut id_of);
        }
    }
    let n = id_of.len();
    let mut closure: Vec<HashSet<usize>> = vec![HashSet::new(); n];
    let mut callees: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (f, gs) in direct_globals.iter() {
        let fi = id_of[f];
        for g in gs {
            closure[fi].insert(id_of[g]);
        }
    }
    for (f, cs) in direct_calls.iter() {
        let fi = id_of[f];
        callees[fi].extend(cs.iter().map(|c| id_of[c]));
    }
    let mut add: Vec<usize> = Vec::new();
    loop {
        let mut changed = false;
        for fi in 0..n {
            if callees[fi].is_empty() {
                continue;
            }
            add.clear();
            for &c in &callees[fi] {
                for &g in &closure[c] {
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

    // Round-based topo sort over each init's transitive global reads (restricted to
    // globals that themselves have an init line); source order kept within a round.
    let mut names_by_id: Vec<ustr::Ustr> = vec![ustr::Ustr::from(""); n];
    for (&k, &v) in id_of.iter() {
        names_by_id[v] = k;
    }
    let known: HashSet<ustr::Ustr> = ginits.iter().map(|g| g.name).collect();
    let init_deps: HashMap<ustr::Ustr, HashSet<ustr::Ustr>> = ginits
        .iter()
        .map(|g| {
            let ds: HashSet<ustr::Ustr> = match id_of.get(&ustr::Ustr::from(g.init_name.as_str())) {
                Some(&fi) => closure[fi]
                    .iter()
                    .map(|&gid| names_by_id[gid])
                    .filter(|d| *d != g.name && known.contains(d))
                    .collect(),
                None => HashSet::new(),
            };
            (g.name, ds)
        })
        .collect();

    let mut emitted: HashSet<ustr::Ustr> = HashSet::new();
    let mut lines = Vec::with_capacity(ginits.len());
    let mut remaining: Vec<&GlobalInit> = ginits.iter().collect();
    while !remaining.is_empty() {
        let before = remaining.len();
        remaining.retain(|g| {
            if init_deps[&g.name].iter().all(|d| emitted.contains(d)) {
                lines.push(GlobalInitLine {
                    name: g.name,
                    js: format!("__G.{} = {}();\n", g.name, g.init_name),
                });
                emitted.insert(g.name);
                false
            } else {
                true
            }
        });
        if remaining.len() == before {
            let names: Vec<&str> = remaining.iter().map(|g| g.name.as_str()).collect();
            return Err(err(format!(
                "JS subset: module globals are mutually recursive: {}",
                names.join(", ")
            )));
        }
    }
    Ok(lines)
}

/// All valid `__rock_enum_*` global names -> their JS def line. A payload-free
/// (all-unit, non-cdata) enum yields `__rock_enum_<E>_names`, a `Map` keyed by
/// ordinal (matching the `kind: Map` index the `{x}` stringify lowers to); a mixed
/// enum yields a `__rock_enum_<E>_<V>` singleton per unit variant. cdata-eligible
/// enums have neither (they lower to value structs). Mirrors the Lua backend's
/// `enum_runtime_defs`; only referenced names are emitted, and the defs tolerate
/// duplication across concatenated per-test programs (`var` redeclare, same value).
fn build_enum_global_defs(ws: &Workspace) -> HashMap<ustr::Ustr, String> {
    use bone_semantic::hir::VariantPayload;
    let mut out = HashMap::new();
    for module in ws.modules.iter() {
        for (_, e) in module.enums.iter() {
            if skull_mir::lower::enum_value_shape(ws, e.name).is_some() {
                continue;
            }
            let all_unit = !e.variants.is_empty()
                && e.variants.iter().all(|v| matches!(v.payload, VariantPayload::Unit));
            if all_unit {
                let name = ustr::Ustr::from(format!("__rock_enum_{}_names", e.name).as_str());
                let items: Vec<String> = e
                    .variants
                    .iter()
                    .enumerate()
                    .map(|(ord, v)| format!("[{ord}, \"{e}::{v}\"]", e = e.name, v = v.name))
                    .collect();
                out.insert(name, format!("var {name} = new Map([{}]);\n", items.join(", ")));
            } else {
                for v in e.variants.iter() {
                    if matches!(v.payload, VariantPayload::Unit) {
                        let name =
                            ustr::Ustr::from(format!("__rock_enum_{}_{}", e.name, v.name).as_str());
                        out.insert(
                            name,
                            format!(
                                "var {name} = {{ __variant: \"{var}\", __ty: \"{ty}\" }};\n",
                                var = v.name,
                                ty = e.name,
                            ),
                        );
                    }
                }
            }
        }
    }
    out
}

/// Collect every GC-carrying value-struct name a body CONSTRUCTS (an
/// `Inst::MakeValueStruct` whose type is `value_struct_is_gc`), scanning the
/// full inst arena so a value struct built as a nested value operand is caught.
fn collect_gc_value_structs(ws: &Workspace, mf: &MirFunc, out: &mut BTreeSet<ustr::Ustr>) {
    for inst in mf.insts.values() {
        if let Inst::MakeValueStruct { name, .. } = inst {
            if skull_mir::lower::value_struct_is_gc(ws, *name) {
                out.insert(*name);
            }
        }
    }
    for nf in &mf.nested_fns {
        collect_gc_value_structs(ws, nf, out);
    }
}

/// Close the referenced set over GC-carrying value-struct FIELDS: a `Wrap`
/// registry entry recurses through `__op_eq` into its `VecBox` field, so that
/// field's entry must exist even if `VecBox` is never independently constructed.
fn close_gc_value_struct_fields(ws: &Workspace, names: &mut BTreeSet<ustr::Ustr>) {
    let mut queue: Vec<ustr::Ustr> = names.iter().copied().collect();
    while let Some(name) = queue.pop() {
        let Some(sid) = ws.struct_by_name.get(&name) else { continue };
        let s = &ws.module(sid.module).structs[sid.def];
        for f in &s.fields {
            if let Some(field_struct) = value_struct_field_name(ws, f.ty) {
                if skull_mir::lower::value_struct_is_gc(ws, field_struct) && names.insert(field_struct)
                {
                    queue.push(field_struct);
                }
            }
        }
    }
}

/// The nominal name of `ty` when it resolves to a `value` struct, else `None`
/// (used to walk value-struct field types for the registry closure).
fn value_struct_field_name(ws: &Workspace, ty: bone_semantic::prelude::TyId) -> Option<ustr::Ustr> {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    let te = match ty {
        bone_semantic::prelude::TyId::Static(mid, idx) => ws.modules.get(mid.index())?.ty_arena[idx],
        bone_semantic::prelude::TyId::Local(_) => return None,
    };
    match te {
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }
            if ws
                .struct_by_name
                .get(&name)
                .map(|sid| ws.module(sid.module).structs[sid.def].is_value)
                .unwrap_or(false) =>
        {
            Some(name)
        }
        _ => None,
    }
}

/// Emit the `__rock_vt_eq["<N>"] = function(a, b) {...}` entries mirroring the
/// Lua `_vt.__eq` (`skull::value_struct_gc_defs`) field-for-field: `__op_eq`
/// (structural) where `gc_eq_is_structural` says structural -- scalars, enums,
/// nested value structs; `===` (by reference, luajit's `rawequal`) for a Vec /
/// Map / Arena / reference-struct / generic-`T` / string field. `===` on a JS
/// string is by-value, matching luajit's interning.
fn build_vt_eq_defs(ws: &Workspace, names: &BTreeSet<ustr::Ustr>) -> Result<String, CompilerError> {
    let mut out = String::new();
    for name in names {
        let Some(sid) = ws.struct_by_name.get(name) else { continue };
        let s = &ws.module(sid.module).structs[sid.def];
        let cmps: Vec<String> = s
            .fields
            .iter()
            .map(|f| {
                let (a, b) = (format!("a.{}", f.name), format!("b.{}", f.name));
                if skull_mir::lower::gc_eq_is_structural(ws, f.ty) {
                    format!("__op_eq({a}, {b})")
                } else {
                    format!("{a} === {b}")
                }
            })
            .collect();
        let body = if cmps.is_empty() { "true".to_string() } else { cmps.join(" && ") };
        out.push_str(&format!(
            "__rock_vt_eq[{}] = function(a, b) {{ return {body}; }};\n",
            js_string_lit(name.as_str())?
        ));
    }
    Ok(out)
}

/// Emit a `co fn` for JS: the shared coroutine pipeline (`transform_coroutine`
/// -> `convert_closures_framed` -> `materialize_frame`) then the trampoline
/// step, hoisted-lambda siblings, hook/defer aux trampolines over the shared
/// arena, and the JS ctor (the exact Lua ctor contract: the object IS the
/// frame; `__resume`/`__advance` drive the step; `JS_CORO_RUNTIME` supplies
/// `__rock_coroutine_kill`/`__rock_coroutine_run_defers`/`error`).
fn emit_coroutine_js(
    ws: &Workspace,
    fr: FuncRef,
    mf: &MirFunc,
    js_name: &str,
    callees: &HashMap<FuncRef, String>,
    hoisted: &HashSet<ustr::Ustr>,
    globals: &GlobalCtx,
) -> Result<String, CompilerError> {
    let f = &ws.module(fr.module).functions[fr.func];
    let channel = f.return_ty.and_then(|t| skull_mir::lower::coroutine_yield_channel(ws, t));
    let mut lowered = skull_transform::transform_coroutine(mf, channel.as_ref());
    let resume_arg = mf.resume_arg.ok_or_else(|| err("co fn must carry a resume-arg cell"))?;
    let step_name = format!("{js_name}__step");
    // Aux slot map BEFORE the closure pass mutates the arena (mirrors the Lua
    // driver); hook/defer regions are untouched by the pass, so the map stays valid.
    let aux_slots = lowered.aux_all_cells_slots(resume_arg);
    let frame_info = skull_transform::FrameInfo {
        s_cell: lowered.s_cell,
        fields: lowered.frame_cells.iter().copied().collect(),
        slots: lowered.frame_slots.clone(),
    };
    skull_transform::convert_closures_framed(
        &mut lowered.step,
        &step_name,
        Some(&frame_info),
        Some((&lowered.step_tree, &lowered.step_own)),
    );
    // This coroutine's hoisted lambda names are referenced only from its own
    // bodies, so extend the program set locally.
    let mut hoisted = hoisted.clone();
    hoisted.extend(lowered.step.nested_fns.iter().map(|nf| nf.name));
    // Materialize the ROOT region's frame accesses, then each detached
    // hook/defer region with the all-cells aux map (disjoint block sets over
    // the shared arena).
    let step_entry = lowered.step.entry;
    skull_transform::materialize_frame(&mut lowered.step, step_entry, lowered.s_cell, &lowered.frame_slots);
    let mut hook_entries: Vec<(&'static str, Idx<Block>)> = Vec::new();
    for (kind, lambda) in &lowered.hooks {
        let Inst::Lambda { params, entry } = &lowered.step.insts[*lambda] else {
            return Err(err(format!("co-fn hook {} must be an Inst::Lambda", kind.slot())));
        };
        if !params.is_empty() {
            return Err(err("co-fn lifecycle hooks are nullary"));
        }
        hook_entries.push((kind.slot(), *entry));
    }
    let defer_entries: Vec<Idx<Block>> =
        lowered.step.coro_defers.iter().map(|(_, e)| *e).collect();
    for &e in defer_entries.iter().chain(hook_entries.iter().map(|(_, e)| e)) {
        skull_transform::materialize_frame(&mut lowered.step, e, lowered.s_cell, &aux_slots);
    }

    let mut out = emit_fn_trampoline(
        ws,
        &lowered.step,
        &step_name,
        callees,
        &hoisted,
        globals,
        lowered.step.entry,
        &[lowered.s_cell, resume_arg],
        false,
    )?;
    // Hoisted lambda siblings are frame-free ordinary fns (their frame accesses
    // are already `e.c<S>.c<slot>` MIR) -- the normal structured path.
    let mut nested = std::mem::take(&mut lowered.step.nested_fns);
    for nf in nested.iter_mut() {
        skull_transform::cleanup_with_nobox(ws, nf);
        skull_transform::recover_counted_loops(ws, nf);
        out.push_str(&emit_fn_with_callees(ws, nf, nf.name.as_str(), callees, &hoisted, globals)?);
    }
    lowered.step.nested_fns = nested;
    for (k, &entry) in defer_entries.iter().enumerate() {
        out.push_str(&emit_fn_trampoline(
            ws,
            &lowered.step,
            &format!("{js_name}__defer{k}"),
            callees,
            &hoisted,
            globals,
            entry,
            &[lowered.s_cell],
            false,
        )?);
    }
    for &(slot, entry) in &hook_entries {
        out.push_str(&emit_fn_trampoline(
            ws,
            &lowered.step,
            &format!("{js_name}{slot}"),
            callees,
            &hoisted,
            globals,
            entry,
            &[lowered.s_cell],
            false,
        )?);
    }
    out.push_str(&emit_js_constructor(js_name, &step_name, &lowered, channel.as_ref())?);
    Ok(out)
}

/// The JS Coroutine constructor + per-co-fn `__resume` (and `__advance` on the
/// frame-slot channel), mirroring the Lua ctor contract field for field. The
/// `try/catch` is the `pcall`; `undefined -> null` on the step's value so a
/// bare terminal return reads as nil.
fn emit_js_constructor(
    name: &str,
    step_name: &str,
    lowered: &skull_transform::CoroLowered,
    channel: Option<&skull_mir::lower::YieldChannel>,
) -> Result<String, CompilerError> {
    use std::fmt::Write as _;
    let param_fields: Vec<String> = lowered
        .params
        .iter()
        .map(|p| {
            lowered
                .frame_slots
                .get(p)
                .map(|s| format!("c{s}"))
                .ok_or_else(|| err("co-fn param must be a frame cell with a slot"))
        })
        .collect::<Result<_, _>>()?;
    let mut init = vec![
        "__ty: \"Coroutine\"".to_string(),
        "__state: 0".to_string(),
        format!("__layout: {}", lowered.layout_hash),
    ];
    for pf in &param_fields {
        init.push(format!("{pf}: {pf}"));
    }
    let n_defers = lowered.step.coro_defers.len();
    if n_defers > 0 {
        let list: Vec<String> = (0..n_defers).map(|k| format!("{name}__defer{k}")).collect();
        init.push(format!("__defers: [{}]", list.join(", ")));
    }
    for (kind, _) in &lowered.hooks {
        let field = kind.slot();
        init.push(format!("{field}: {name}{field}"));
    }
    // The step-raise recovery shared by both resume shapes: errored terminal,
    // kill the delegated inner, run defers LIFO, fire on_killed.
    let catch_body = "s.__state = -2; s.__error = (e && e.message !== undefined) ? e.message : e; \
                      if (s.__inner) { __rock_coroutine_kill(s.__inner); s.__inner = null; } \
                      __rock_coroutine_run_defers(s); \
                      if (s.__on_killed) { s.__on_killed(s); }";
    if let Some(ch) = channel {
        let mut box_fields = format!(
            "{{ __variant: s.{tag}, __ty: {ty}",
            tag = skull_mir::lower::yield_slot(0),
            ty = js_string_lit(ch.enum_name.as_str())?,
        );
        // 0-based positional keys: JS `TupleIndex` reads `base[i]` (no +1 like Lua's
        // array keys), so a boxed-enum consumer of a yielded channel value projects
        // the payload correctly. Matches `MakeEnum`'s native 0-based keys.
        for i in 0..ch.tuple_width {
            let _ = write!(box_fields, ", \"{}\": s.{}", i, skull_mir::lower::yield_slot(1 + i));
        }
        for (j, rf) in ch.record_fields.iter().enumerate() {
            let _ = write!(
                box_fields,
                ", {rf}: s.{}",
                skull_mir::lower::yield_slot(1 + ch.tuple_width + j)
            );
        }
        box_fields.push_str(" }");
        return Ok(format!(
            "function {name}__advance(s, arg) {{\n\
             \x20 if (s.__state < 0) return true;\n\
             \x20 if (s.__on_resume) s.__on_resume(s);\n\
             \x20 try {{ {step_name}(s, arg); }} catch (e) {{ {catch_body} return true; }}\n\
             \x20 return s.__state < 0;\n\
             }}\n\
             function {name}__resume(s, arg) {{\n\
             \x20 if ({name}__advance(s, arg)) return null;\n\
             \x20 return {box_fields};\n\
             }}\n\
             function {name}({paramlist}) {{\n\
             \x20 return {{ {fields}, __advance: {name}__advance, __resume: {name}__resume }};\n\
             }}\n",
            paramlist = param_fields.join(", "),
            fields = init.join(", "),
        ));
    }
    Ok(format!(
        "function {name}__resume(s, arg) {{\n\
         \x20 if (s.__state < 0) return null;\n\
         \x20 if (s.__on_resume) s.__on_resume(s);\n\
         \x20 var val;\n\
         \x20 try {{ val = {step_name}(s, arg); }} catch (e) {{ {catch_body} return null; }}\n\
         \x20 return val === undefined ? null : val;\n\
         }}\n\
         function {name}({paramlist}) {{\n\
         \x20 return {{ {fields}, __resume: {name}__resume }};\n\
         }}\n",
        paramlist = param_fields.join(", "),
        fields = init.join(", "),
    ))
}

/// Every `Inst::Global` read + `Inst::StoreGlobal` write name in `mf` (main body
/// and hoisted nested fns) -- the module-global surface the emitter must resolve
/// (state, fn-value, or reject). Names are deduped in source order.
fn global_refs(mf: &MirFunc) -> Vec<ustr::Ustr> {
    fn walk(mf: &MirFunc, out: &mut Vec<ustr::Ustr>, seen: &mut HashSet<ustr::Ustr>) {
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
                let name = match &mf.insts[iid] {
                    Inst::Global(n) => *n,
                    Inst::StoreGlobal { name, .. } => *name,
                    _ => continue,
                };
                if seen.insert(name) {
                    out.push(name);
                }
            }
        }
        for nf in &mf.nested_fns {
            walk(nf, out, seen);
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk(mf, &mut out, &mut seen);
    out
}

/// The minimum argument count a co-emitted callee accepts: its MIR param count
/// minus the params carrying a default (each omitted trailing default arrives as
/// JS `undefined` and is filled callee-side). Defaults live on the fn's HIR body;
/// for the scalar defaults this gate targets (`step: int = 1`) each maps 1:1 to a
/// MIR param slot. A GC value-struct default would break the 1:1 assumption, but
/// value-struct construction rejects at emit independently, so no such call is
/// admitted here.
fn required_arg_count(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    fr: FuncRef,
    arity: usize,
) -> usize {
    use bone_semantic::hir::BodyOwner;
    let defaults: Vec<bool> = ws
        .module(fr.module)
        .bodies
        .get(&BodyOwner::Function(fr.func))
        .map(|b| b.param_defaults.iter().map(|d| d.is_some()).collect())
        .unwrap_or_default();
    // Scalar ABI: a defaulted value-struct param explodes to N leaf slots and
    // all N are omittable together -- subtract per-param LEAF widths, not 1.
    if let Some(widths) = skull_mir::lower::scalar_abi_param_leaf_widths(ws, scx, fr) {
        let omittable: usize = widths
            .iter()
            .zip(defaults.iter().chain(std::iter::repeat(&false)))
            .filter(|(_, d)| **d)
            .map(|(w, _)| w)
            .sum();
        return arity.saturating_sub(omittable);
    }
    let n_defaults = defaults.iter().filter(|d| **d).count();
    arity.saturating_sub(n_defaults)
}

/// Whether `cf` renders via the JS template table: it must both HAVE a table
/// entry and be a stdlib `@intrinsic` declaration -- a user fn that merely
/// shares an entry's (name, arity) must not silently become `Math.min`.
fn is_mapped_intrinsic(cf: &bone_semantic::hir::Function) -> bool {
    cf.annotations.iter().any(|a| a.name.name.as_str() == "intrinsic")
        && JsIntrinsics.lookup(cf).is_some()
}

/// The variadic `print` extern: rendered as the harness-provided `print`
/// global (Lua-parity stdout capture), not co-emitted.
fn is_host_print(cf: &bone_semantic::hir::Function) -> bool {
    cf.is_extern && cf.name.as_str() == "print"
}

/// The test-local `put` idiom: `@inline inline(lua) fn put(s: string) =
/// "io.write(s)"` -- a raw stdout write with no trailing newline. Rendered as the
/// harness-provided `put` global (which appends to the same per-test buffer as
/// `print`, raw), not co-emitted. Gated on the `io.write` body so a plain-Rock
/// user `put` co-emits normally.
fn is_host_put(cf: &bone_semantic::hir::Function) -> bool {
    cf.name.as_str() == "put"
        && cf.param_decls.len() == 1
        && cf.inline_body.is_some_and(|(_, b)| b.as_str().contains("io.write"))
}

/// The logging dedup/throttle impls (`@nomangle inline(lua)` in
/// `stdlib/logging.rock`): rendered by their original name to the JS runtime port
/// (`crate::JS_LOG_RUNTIME`), not co-emitted. The (name, arity) set matches the
/// stdlib decls; the host-body gate keeps a plain-Rock same-name user fn on the
/// co-emit path. `log_error`/`log_warn`/... and `log_get_level` are pure Rock and
/// co-emit over `__stone_log`/`__stone_log_init` normally.
fn is_host_log_fn(cf: &bone_semantic::hir::Function) -> bool {
    const LOG_FNS: &[(&str, usize)] = &[
        ("__stone_log_init", 0),
        ("__stone_emit", 3),
        ("__stone_log", 3),
        ("log_set_level", 1),
        ("_log_once_impl", 3),
        ("_log_throttled_with_now_impl", 5),
        ("stone_log_set_now", 1),
        ("stone_log_clear_now", 0),
        ("__stone_log_now", 0),
        ("_log_throttled_impl", 4),
        ("log_reset_dedup", 0),
        ("log_dedup_count", 0),
    ];
    (cf.is_extern || cf.inline_body.is_some())
        && LOG_FNS.iter().any(|(n, a)| *n == cf.name.as_str() && *a == cf.param_decls.len())
}

/// A fixed-arity, host-bodied stdlib fn (extern or inline(lua)) with a
/// hand-written JS binding the emitter substitutes for the Lua body. The gate
/// is the host-body flag PLUS the (name, arity) match, so a plain-Rock user fn
/// of the same name co-emits normally and is never captured here.
fn host_fn_template(cf: &bone_semantic::hir::Function) -> Option<&'static str> {
    if !(cf.is_extern || cf.inline_body.is_some()) {
        return None;
    }
    match (cf.name.as_str(), cf.param_decls.len()) {
        ("tostring", 1) => Some("lua_tostring($1)"),
        ("__rock_format_float", 1) => Some("__rock_format_float($1)"),
        // `int_to_float`/`to_float` are pure `$1` identities (JS numbers are all
        // doubles): a call-site host template inlines with zero cost and neither
        // is ever taken as a value, so a co-emitted identity fn would be strictly
        // worse. `float_to_int` (taken as a value in tests/function_ref_cross_module)
        // is an inline(js) sibling in the stdlib instead -- the fn-value path now
        // collapses backend siblings, so it needs a real bound fn, not a template.
        ("int_to_float", 1) => Some("$1"),
        ("to_float", 1) => Some("$1"),
        // `phantom<T>()` is a nil placeholder (iterator `done`, post-`error`
        // unreachable). `__debug_traceback` is error-message-only (never on
        // stdout, so an empty string suffices). `is_null` mirrors `== nil`.
        ("phantom", 0) => Some("null"),
        ("__debug_traceback", 0) => Some("\"\""),
        ("is_null", 1) => Some("($1 == null)"),
        // `cast`/`transmute` are runtime identities (type recovery only).
        ("cast", 1) | ("transmute", 1) => Some("$1"),
        // Untyped table primitives, runtime-polymorphic: in Lua a Map IS a raw
        // table, so `table_get(m, k)` and `m[k]` must agree -- but on JS a Map is
        // a real `Map` (`.get`/`.set`) while an array/object indexes directly.
        // `__table_get`/`__table_set` dispatch on `instanceof Map`.
        ("table_get", 2) => Some("__table_get($1, $2)"),
        ("table_set", 3) => Some("__table_set($1, $2, $3)"),
        // `t[k] = nil`: a Map key delete or an array/object hole, dispatched on
        // `instanceof Map`. Returns undefined -> Rock nil if the result is stored.
        ("table_unset", 2) => Some("__table_unset($1, $2)"),
        // Base-agnostic Vec `_data` accessors (`__vec_data_*`): a logical 0-based
        // element index over the 0-based hole-free `_data` array (never a Map, so
        // index directly). `unset` writes `undefined` (the drain callers only
        // touch the tail; the len-aware `__op_eq` Vec branch and len-bounded
        // readers ignore the trailing slot, matching luajit's pairs-walk).
        ("__vec_data_get", 2) => Some("$1._data[$2]"),
        ("__vec_data_set", 3) => Some("$1._data[$2] = $3"),
        ("__vec_data_unset", 2) => Some("$1._data[$2] = undefined"),
        // The COLD half of the same boundary (allocate / measure / shift / copy
        // / reorder the backing as a whole). JS has no column storage -- columns
        // go to the SKIP histogram -- so each is just the boxed form, but they
        // must be mapped or the ops routed through them stop emitting.
        // `lua_index` arrives 1-based from the shared Rock body; JS is 0-based.
        ("__vec_data_alloc", 1) => Some("({_data: new Array($1).fill(false), len: 0, __ty: \"Vec\"})"),
        ("__vec_data_capacity", 1) => Some("$1._data.length"),
        ("__vec_data_insert", 3) => Some("$1._data.splice($2 - 1, 0, $3)"),
        ("__vec_data_clone", 1) => Some("({_data: $1._data.slice(), len: $1.len, __ty: \"Vec\"})"),
        ("__vec_data_sort", 2) => Some("__vec_sort($1, $2)"),
        ("table_new", 0) => Some("({})"),
        // Vec: `{_data, len, __ty}` 0-based. `vec_new` builds it; `vec_push`
        // writes `_data[len]` (the new tail slot) THEN increments `len` -- the
        // reverse of the 1-based Lua order (comma operator so it stays an
        // expression; `$1` is a cell load, safe to re-evaluate).
        ("vec_new", 0) => Some("({_data: [], len: 0, __ty: \"Vec\"})"),
        ("vec_push", 2) => Some("($1._data[$1.len] = $2, $1.len = $1.len + 1)"),
        // Structural equality (deep, matching Rock's `__op_eq`: structs, enums,
        // newtypes, tuples by contents; primitives by value). Runtime helper in
        // JS_STRUCTURAL_EQ.
        ("__op_eq", 2) => Some("__op_eq($1, $2)"),
        ("__op_ne", 2) => Some("(!__op_eq($1, $2))"),
        // Lua `//` floored integer division.
        ("__op_idiv", 2) => Some("Math.floor($1 / $2)"),
        // `string.len` counts bytes; in the ASCII-only string subset that equals
        // JS `.length` (UTF-16 units). `float(int)` is an identity (all doubles).
        ("len", 1) => Some("$1.length"),
        ("float", 1) => Some("$1"),
        // Host-bodied string ops (all lua_body, so a pure-Rock user fn of the same
        // name -- e.g. the iterator `find(it, pred)` -- co-emits instead). ASCII
        // subset: `unit_at` is a JS UTF-16 unit, a luajit byte, equal in [0,127].
        // `find` -> 0-based indexOf (-1 miss); its `from` default is padded to
        // `undefined` above (indexOf reads that as 0). `substring(start, end)` is
        // a 0-based end-exclusive slice (Lua `sub(s, start+1, end)`); callers pass
        // in-range non-negative bounds.
        ("find", 3) => Some("$1.indexOf($2, $3)"),
        ("substring", 3) => Some("$1.slice($2, $3)"),
        ("unit_at", 2) => Some("($1.charCodeAt($2) || 0)"),
        ("upper", 1) => Some("$1.toUpperCase()"),
        ("lower", 1) => Some("$1.toLowerCase()"),
        ("string_char", 1) => Some("String.fromCharCode($1)"),
        // `from_codepoint(cp)` -> the byte-faithful UTF-8 encoder in
        // JS_STRING_RUNTIME: each output JS char code is one UTF-8 byte, matching
        // luajit's byte-string, so `chars()` (which UTF-8-decodes over `unit_at`)
        // round-trips a code point identically on both backends.
        ("from_codepoint", 1) => Some("__from_codepoint($1)"),
        // `tonumber(s)` -> a strict numeric parse (null on non-numeric / empty).
        ("parse_float", 1) => Some("__parse_float($1)"),
        // RNG -> the `JS_RNG_RUNTIME` mulberry32 port (stream differs from Lua's
        // splitmix by doctrine; the seedable/deterministic/serializable semantics
        // hold). `env_var` -> the driver-provided node glue (`process.env`).
        ("random", 0) => Some("__rock_random()"),
        ("random_int", 2) => Some("__rock_random_int($1, $2)"),
        ("random_seed", 1) => Some("__rock_rng_seed($1)"),
        ("env_var", 1) => Some("__rock_env_var($1)"),
        // `deepcopy(obj)` -> the recursive clone runtime (`JS_DEEPCOPY_RUNTIME`).
        ("deepcopy", 1) => Some("deepcopy($1)"),
        ("__rock_deepcopy_value", 2) => Some("__rock_deepcopy_value($1, $2)"),
        // `inspect`/`inspect_formatted` -> the pretty-printer runtime
        // (`JS_INSPECT_RUNTIME`). The 3-arg `inspect` (obj, cycle_detect,
        // precision) is the mangled form; the defaulted trailing args are padded
        // to `undefined` above and defaulted inside the runtime.
        ("inspect", 3) => Some("inspect($1, $2, $3)"),
        ("inspect_formatted", 2) => Some("inspect_formatted($1, $2)"),
        // The `{x:spec}` interpolation format helpers -> their JS runtime ports.
        ("__rock_fmt_pad", 4) => Some("__rock_fmt_pad($1, $2, $3, $4)"),
        ("__rock_fmt_grouping", 3) => Some("__rock_fmt_grouping($1, $2, $3)"),
        ("__rock_fmt_binary", 1) => Some("__rock_fmt_binary($1)"),
        ("__rock_fmt_percent", 2) => Some("__rock_fmt_percent($1, $2)"),
        // `format_float(v, width, decimals)` reconstructs the `%W.Df` printf spec.
        ("format_float", 3) => Some("__rock_sprintf(\"%\" + $2 + \".\" + $3 + \"f\", $1)"),
        // A tuple is a 0-based JS array; Lua `tuple[1]`/`[2]` are `[0]`/`[1]`.
        ("tuple_first", 1) => Some("$1[0]"),
        ("tuple_second", 1) => Some("$1[1]"),
        // Materialized `a..b` range -> a Vec-shaped object (runtime helper).
        ("__range", 2) => Some("__range($1, $2)"),
        // The JS harness has no release mode (`_G.__ROCK_RELEASE` is never set).
        ("is_release_mode", 0) => Some("false"),
        // `pcall(f); return not ok` -- did `f` error? A JS throw is the failure.
        ("try_call", 1) => Some("__try_call($1)"),
        // Coroutine consumers (stdlib inline(lua) readers of the frame object).
        // IIFEs where an arg is read twice; `undefined -> null` normalization so
        // an unset frame field reads as Rock null (Lua nil parity). `resume` has
        // two arities (overload-by-arity: `c.resume()` vs `c.resume(arg)`).
        ("resume", 1) => Some("(function(c){var v=c.__resume(c);return v===undefined?null:v;})($1)"),
        ("resume", 2) => Some("(function(c,a){var v=c.__resume(c,a);return v===undefined?null:v;})($1, $2)"),
        ("result", 1) => Some("(function(c){var v=c.__return;return v===undefined?null:v;})($1)"),
        ("status", 1) => Some("(function(c){return c.__state === -2 ? \"errored\" : (c.__state < 0 ? \"dead\" : \"suspended\");})($1)"),
        ("completed", 1) => Some("($1.__state === -3)"),
        ("last_error", 1) => Some("(function(c){var v=c.__error;return v===undefined?null:v;})($1)"),
        ("kill", 1) => Some("__rock_coroutine_kill($1)"),
        ("fire_hot_reload", 1) => Some("(function(c){ if (c.__state >= 0 && c.__on_hot_reload) c.__on_hot_reload(c); })($1)"),
        // `error(msg)` diverges; the message is already a string. Wrapped in a
        // throwing IIFE so it is a valid EXPRESSION -- the lowering stores a
        // diverging call's (dead) result into an if-result cell, and a bare
        // `throw` is a statement, not an expression. The node harness catch turns
        // the throw into a FAIL carrying the message -- Lua's behavior for an
        // errored test. luajit prepends a `file:line:` position; no in-scope test
        // compares that (it is on the error object, not stdout), so JS reproduces
        // only the message. `assert` is pure Rock over `error`.
        ("error", 1) => Some("(() => { throw new Error($1); })()"),
        _ => None,
    }
}

/// A host-bodied fn the emitter renders directly (bypassing co-emission): the
/// `print` global or one of the fixed-arity templates. Shared by `render_call`
/// and the `emit_with_closure` callee walk so both agree on what NOT to reject.
fn is_host_mapped(cf: &bone_semantic::hir::Function) -> bool {
    is_host_print(cf) || is_host_put(cf) || is_host_log_fn(cf) || host_fn_template(cf).is_some()
}

/// `map_new()`: pure Rock (`transmute(table_new())`), but intercepted to a real
/// JS `Map` rather than co-emitted to a plain object (which coerces keys).
fn is_map_new(cf: &bone_semantic::hir::Function) -> bool {
    cf.name.as_str() == "map_new" && cf.param_decls.is_empty()
}

/// The cells an inst writes: a `Store`'s target, or a multi-value call's dests.
/// These are the cells that need a hoisted `let` (the multi-assign / store binds
/// them, and a later block may read them across a branch merge).
fn inst_writes(inst: &Inst) -> Vec<Idx<Cell>> {
    match inst {
        Inst::Store { cell, .. } => vec![*cell],
        Inst::CallMulti { dests, .. }
        | Inst::CallValueMulti { dests, .. }
        | Inst::CallBoundMulti { dests, .. } => dests.clone(),
        // MapNext writes both the cursor key cell (in place) and the value cell.
        Inst::MapNext { key_cell, val_cell, .. } => vec![*key_cell, *val_cell],
        _ => Vec::new(),
    }
}

/// The `NativeBuffer*` nominal names (T-typed + phantom-size variants); the
/// same set skull-mir's lowering recognizes as `ContainerKind::NativeBuffer`.
fn is_native_buffer(name: &str) -> bool {
    matches!(
        name,
        "NativeBuffer"
            | "NativeBufferU8"
            | "NativeBufferI8"
            | "NativeBufferU16"
            | "NativeBufferI16"
            | "NativeBufferU32"
            | "NativeBufferI32"
    )
}


/// Whether `f`'s first declared param is the BUILTIN `Map<K,V>` (`TypeCtor::Map`),
/// distinguishing it from a user struct named `Map` (a `Nominal`). The `get`/2
/// receiver dispatch renders inline `.get` ONLY for the builtin.
fn first_param_is_builtin_map(ws: &Workspace, f: &bone_semantic::hir::Function) -> bool {
    use bone_semantic::hir::{TyId, TypeCtor, TypeExpr};
    let Some(pd) = f.param_decls.first() else {
        return false;
    };
    let TyId::Static(mid, idx) = pd.ty else {
        return false;
    };
    matches!(
        &ws.module(mid).ty_arena[idx],
        TypeExpr::App { ctor: TypeCtor::Map, .. }
    )
}

/// Invoke `f` for each inst used as an operand by `inst`. The match is
/// EXHAUSTIVE on purpose: a new Inst variant must decide its operands here or
/// the compiler errors, so use-counting can never silently rot.
fn for_each_operand(inst: &Inst, f: &mut impl FnMut(Idx<Inst>)) {
    let mut mark = |ids: &[Idx<Inst>]| ids.iter().for_each(|&i| f(i));
    match inst {
        Inst::Const(_) | Inst::Global(_) | Inst::Load(_) | Inst::Lambda { .. } => {}
        Inst::FfiViewCast { ptr, .. } => mark(&[*ptr]),
        Inst::IntAdd(a, b)
        | Inst::IntSub(a, b)
        | Inst::IntMul(a, b)
        | Inst::IntDiv(a, b)
        | Inst::IntMod(a, b)
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
        | Inst::Shr(a, b) => mark(&[*a, *b]),
        Inst::ToString(x)
        | Inst::FloatToInt(x)
        | Inst::IntToFloat(x)
        | Inst::Neg(x)
        | Inst::Not(x)
        | Inst::BitNot(x)
        | Inst::CoroSetInner(x) => mark(&[*x]),
        Inst::Format { value, .. } => mark(&[*value]),
        Inst::FormatDyn { value, fmt } => mark(&[*value, *fmt]),
        Inst::Call { args, .. } | Inst::CallBoundMulti { args, .. } => mark(args),
        Inst::CallMulti { args, .. } => mark(args),
        Inst::CallValue { callee, args } | Inst::CallValueMulti { callee, args, .. } => {
            mark(&[*callee]);
            mark(args);
        }
        Inst::MakeTuple(elems) | Inst::MakeVec(elems) => mark(elems),
        Inst::MakeVecColumns {
            columns, capacity, ..
        } => {
            mark(&[*capacity]);
            columns.iter().for_each(|(_, vs)| mark(vs));
        }
        Inst::MakeStruct { fields, .. }
        | Inst::MakeValueStruct { fields, .. }
        | Inst::MaterializeValueStruct { fields, .. }
        | Inst::MakeEnv { fields, .. } => {
            for (_, v) in fields {
                f(*v);
            }
        }
        Inst::MakeEnum { positional, named, .. } => {
            mark(positional);
            for (_, v) in named {
                f(*v);
            }
        }
        Inst::MakeEnumDyn { tag, positional, named, .. } => {
            mark(&[*tag]);
            mark(positional);
            for (_, v) in named {
                f(*v);
            }
        }
        Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => mark(&[*base]),
        Inst::Index { base, index, .. } => mark(&[*base, *index]),
        Inst::VecCheckedIndex { vec, index } => mark(&[*vec, *index]),
        Inst::Len { base, .. } => mark(&[*base]),
        Inst::FfiNewArray { count, .. } => mark(&[*count]),
        Inst::Store { value, .. } => mark(&[*value]),
        Inst::StoreField { base, value, .. } => mark(&[*base, *value]),
        Inst::StoreIndex { base, index, value, .. } => mark(&[*base, *index, *value]),
        Inst::StoreGlobal { value, .. } => mark(&[*value]),
        Inst::ArrayPush { arr, value, .. } => mark(&[*arr, *value]),
        Inst::CoroHook { lambda, .. } => mark(&[*lambda]),
        Inst::MapNext { table, .. } => mark(&[*table]),
    }
}

fn for_each_term_operand(term: &Term, f: &mut impl FnMut(Idx<Inst>)) {
    match term {
        Term::Return(vs) => vs.iter().for_each(|&i| f(i)),
        Term::Branch { cond, .. } => f(*cond),
        Term::Switch { discr, .. } => f(*discr),
        Term::Yield { values, .. } => values.iter().for_each(|&i| f(i)),
        Term::Unreachable | Term::Goto(_) => {}
    }
}

/// Value-element NativeBuffer refs to suppress: an `Index{NativeBufferValue}`
/// (or a nested-struct `Field` chained off one) whose every use is a `Field`/
/// `StoreField` BASE. Those consumers render flat-slot access from the chain's
/// own operands, so the ref emits nothing; any other use (a `Store` into a cell,
/// a call arg, a `StoreIndex` value) forces the whole-element COPY materialize.
fn compute_nbv_bypass(mf: &MirFunc, blocks: &[Idx<Block>]) -> HashSet<u32> {
    let mut cand: HashSet<u32> = HashSet::new();
    for &b in blocks {
        for &id in &mf.blocks[b].insts {
            match &mf.insts[id] {
                Inst::Index { kind: ContainerKind::NativeBufferValue(_), .. } => {
                    cand.insert(raw(id));
                }
                // A Field chained off a candidate (nested value-struct descent).
                // Blocks are in traversal order and a Field always follows its
                // base textually, so a single pass suffices for chains.
                Inst::Field { base, .. } if cand.contains(&raw(*base)) => {
                    cand.insert(raw(id));
                }
                _ => {}
            }
        }
    }
    if cand.is_empty() {
        return cand;
    }
    let mut other_use: HashSet<u32> = HashSet::new();
    let mut used: HashSet<u32> = HashSet::new();
    for &b in blocks {
        for &id in &mf.blocks[b].insts {
            match &mf.insts[id] {
                Inst::Field { base, .. } => {
                    used.insert(raw(*base));
                }
                Inst::StoreField { base, value, .. } => {
                    used.insert(raw(*base));
                    used.insert(raw(*value));
                    if cand.contains(&raw(*value)) {
                        other_use.insert(raw(*value));
                    }
                }
                other => for_each_operand(other, &mut |u| {
                    used.insert(raw(u));
                    if cand.contains(&raw(u)) {
                        other_use.insert(raw(u));
                    }
                }),
            }
        }
        for_each_term_operand(&mf.blocks[b].term, &mut |u| {
            used.insert(raw(u));
            if cand.contains(&raw(u)) {
                other_use.insert(raw(u));
            }
        });
    }
    cand.retain(|id| used.contains(id) && !other_use.contains(id));
    cand
}

/// A pure, position-independent value inst that folds into its single use site.
/// EXCLUDES `Load`/`Call`/`Index`/`Field`/`Len` (impure or cell-order-sensitive)
/// -- those materialize at their program position. Mirrors codegen-lua's
/// `always_inlinable`.
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
            | Inst::Neg(_) | Inst::Not(_) | Inst::BitNot(_) | Inst::ToString(_) | Inst::FloatToInt(_) | Inst::IntToFloat(_)
            | Inst::Format { .. } | Inst::FormatDyn { .. }
            | Inst::MakeTuple(_) | Inst::MakeVec(_)
            | Inst::MakeStruct { .. } | Inst::MakeValueStruct { .. } | Inst::MakeEnum { .. } | Inst::MakeEnumDyn { .. }
            | Inst::MakeEnv { .. }
    )
}

/// Use counts (dense by raw inst id, over reachable blocks) + the inlinable
/// mask: an `always_inlinable` value used EXACTLY ONCE with def and use in the
/// SAME block folds into that use; everything else materializes at its program
/// position. Same-block + single-use makes the fold order-safe.
fn compute_inlinable(mf: &MirFunc) -> (Vec<u32>, Vec<bool>) {
    compute_inlinable_over(mf, &reachable_blocks(mf))
}

/// [`compute_inlinable`] over an explicit block region -- the trampoline mode
/// analyzes a detached hook/defer region, not the fn-entry region.
fn compute_inlinable_over(mf: &MirFunc, blocks: &[Idx<Block>]) -> (Vec<u32>, Vec<bool>) {
    const NONE: u32 = u32::MAX;
    let ni = mf.insts.len();
    let mut use_count = vec![0u32; ni];
    let mut use_block = vec![NONE; ni];
    let mut def_block = vec![NONE; ni];
    for &b in blocks {
        let br = raw(b);
        for &id in &mf.blocks[b].insts {
            if mf.insts[id].produces_value() {
                def_block[raw(id) as usize] = br;
            }
            for_each_operand(&mf.insts[id], &mut |u| {
                let k = raw(u) as usize;
                use_count[k] += 1;
                use_block[k] = br;
            });
        }
        for_each_term_operand(&mf.blocks[b].term, &mut |u| {
            let k = raw(u) as usize;
            use_count[k] += 1;
            use_block[k] = br;
        });
    }
    let mut inl = vec![false; ni];
    for &b in blocks {
        for &id in &mf.blocks[b].insts {
            let k = raw(id) as usize;
            if use_count[k] == 1
                && def_block[k] != NONE
                && def_block[k] == use_block[k]
                && always_inlinable(&mf.insts[id])
            {
                inl[k] = true;
            }
        }
    }
    (use_count, inl)
}

/// Materialized value temps (raw inst ids) whose use crosses their defining
/// block. The structured emitter nests blocks so such a temp stays in lexical
/// scope; the trampoline puts each block in a SIBLING switch case, so a
/// cross-case `let v<N>` would be undefined at the use -- these get hoist-
/// declared at the fn top and assigned in-place ([`Emitter::hoisted_temps`]). A
/// same-block temp is not returned (keeps its per-case `let`).
fn cross_block_temps(
    mf: &MirFunc,
    region: &[Idx<Block>],
    use_count: &[u32],
    inlinable: &[bool],
) -> HashSet<u32> {
    let mut def_block: HashMap<u32, Idx<Block>> = HashMap::new();
    for &b in region {
        for &iid in &mf.blocks[b].insts {
            let k = raw(iid);
            if mf.insts[iid].produces_value()
                && !inlinable[k as usize]
                && use_count[k as usize] > 0
            {
                def_block.insert(k, b);
            }
        }
    }
    let mut cross: HashSet<u32> = HashSet::new();
    for &b in region {
        let mut check = |op: Idx<Inst>| {
            let k = raw(op);
            if let Some(&db) = def_block.get(&k) {
                if db != b {
                    cross.insert(k);
                }
            }
        };
        for &iid in &mf.blocks[b].insts {
            for_each_operand(&mf.insts[iid], &mut check);
        }
        for_each_term_operand(&mf.blocks[b].term, &mut check);
    }
    cross
}

/// Successor blocks of `b` (`None` == the virtual exit).
fn succ(mf: &MirFunc, b: Idx<Block>) -> Vec<Option<Idx<Block>>> {
    match &mf.blocks[b].term {
        Term::Return(_) | Term::Unreachable => vec![None],
        Term::Goto(t) => vec![Some(*t)],
        Term::Branch { then_bb, else_bb, .. } => vec![Some(*then_bb), Some(*else_bb)],
        Term::Switch { cases, default, .. } => {
            let mut v: Vec<_> = cases.iter().map(|(_, b)| Some(*b)).collect();
            v.push(Some(*default));
            v
        }
        Term::Yield { resume, .. } => vec![Some(*resume)],
    }
}

/// Blocks reachable from the entry, in discovery order.
fn reachable_blocks(mf: &MirFunc) -> Vec<Idx<Block>> {
    reachable_from(mf, mf.entry)
}

/// Blocks reachable over terminators from `root` (a fn entry OR a lambda entry).
fn reachable_from(mf: &MirFunc, root: Idx<Block>) -> Vec<Idx<Block>> {
    let mut reachable = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(b) = stack.pop() {
        if !seen.insert(b) {
            continue;
        }
        reachable.push(b);
        for s in succ(mf, b).into_iter().flatten() {
            stack.push(s);
        }
    }
    reachable
}
