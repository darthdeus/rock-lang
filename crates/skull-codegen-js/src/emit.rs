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

use std::collections::{HashMap, HashSet};

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

fn err(msg: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(Span::default(), msg.into())
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
/// needs. Non-ASCII-printable bytes reject rather than guessing an encoding
/// (the Lua backend passes UTF-8 through verbatim; JS source is UTF-16, so a
/// raw copy would diverge -- out of the T2 subset).
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
            _ => {
                return Err(err(format!(
                    "JS subset: non-ASCII-printable char in string literal (U+{:04X})",
                    c as u32
                )))
            }
        }
    }
    o.push('"');
    Ok(o)
}

/// Emit `mf` as `function <js_name>(...) { ... }`. `js_name` is the caller's
/// chosen JS identity (the S3/S4 harness names each fallback fn).
pub fn emit_fn(ws: &Workspace, mf: &MirFunc, js_name: &str) -> Result<String, CompilerError> {
    emit_fn_with_callees(ws, mf, js_name, &HashMap::new(), &HashSet::new())
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
            for &cell in inst_writes(&mf.insts[iid]) {
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

    em.emit_seq(mf.entry, None, LoopCtx::default())?;
    let body = format!("{header}{}}}\n", em.out);
    Ok(finalize_labels(body, &em.loop_markers, &em.labels_used))
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
    entry: Idx<Block>,
    params: &[Idx<Cell>],
) -> Result<String, CompilerError> {
    let region = reachable_from(mf, entry);
    let (use_count, inlinable) = compute_inlinable_over(mf, &region);
    let mut em = Emitter {
        ws,
        mf,
        callees,
        hoisted,
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
            for &cell in inst_writes(&mf.insts[iid]) {
                if !param_set.contains(&raw(cell)) && seen.insert(raw(cell)) {
                    declared.push(raw(cell));
                }
            }
        }
    }
    if !declared.is_empty() {
        declared.sort_unstable();
        let names: Vec<String> = declared.iter().map(|id| format!("c{id}")).collect();
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
            structure::successors(&self.mf.blocks[b].term, |t| {
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
                        ContainerKind::Vec => format!("__vec_checked_set({b}, {i}, {v});"),
                        ContainerKind::Map => format!("{b}.set({i}, {v});"),
                    };
                    self.push(&line);
                }
                // `base.field = value` (struct field / cursor mutation).
                Inst::StoreField { base, field, value } => {
                    let line = format!(
                        "{}.{field} = {};",
                        self.render_val(*base)?,
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
                // A used, non-inlinable value inst: materialize it at its
                // program position as `let v<raw> = <expr>` so cell reads capture
                // their value here and effectful calls keep their order. Its use
                // sites read the temp via `render_val`.
                other if other.produces_value()
                    && !self.inlinable[raw(iid) as usize]
                    && self.use_count[raw(iid) as usize] > 0 =>
                {
                    let expr = self.render_inst_expr(iid)?;
                    self.push(&format!("let v{} = {};", raw(iid), expr));
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
            // `yield*`'s re-raise read it as a value (`JS_CORO_RUNTIME` defines it).
            Inst::Global(name) if self.hoisted.contains(name) || name.as_str() == "error" => {
                name.to_string()
            }
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
                    parts.push(format!("{f}: {}", self.render_val(*v)?));
                }
                format!("({{{}}})", parts.join(", "))
            }
            // A `value` struct is byte-value cdata on luajit; on JS an all-scalar
            // (GC-free) one is a plain `{__ty, ...}` object, same shape as a
            // reference struct -- structural deep-eq (`JS_STRUCTURAL_EQ`) matches
            // luajit's byte-value eq for all-scalar leaves. The scalar ABI elides
            // most value structs at call boundaries (they scalarize through); this
            // covers the whole-value re-box (`==`, a Vec element, a boxed return).
            // A GC-carrying value struct compares its pointer fields by REFERENCE
            // on luajit, which the generic contents-eq cannot reproduce without
            // type-aware codegen -- rejected (T8c+).
            Inst::MakeValueStruct { name, fields } => {
                if !skull_mir::lower::value_struct_is_gc_free(self.ws, *name) {
                    return Err(err(format!(
                        "JS subset: GC-carrying value struct `{name}` (reference-field \
                         eq needs type-aware codegen)"
                    )));
                }
                let mut parts = vec![format!("__ty: {}", js_string_lit(&name.to_string())?)];
                for (f, v) in fields {
                    parts.push(format!("{f}: {}", self.render_val(*v)?));
                }
                format!("({{{}}})", parts.join(", "))
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
            // A Vec literal: `{_data, len, __ty}` with `_data` used 1-indexed
            // (index 0 is an unused placeholder, matching the Lua 1-indexed
            // backing table -- the stdlib helpers read `_data[i + 1]`).
            Inst::MakeVec(elems) => {
                let rendered: Result<Vec<String>, CompilerError> =
                    elems.iter().map(|e| self.render_val(*e)).collect();
                let rendered = rendered?;
                let data = if rendered.is_empty() {
                    "[]".to_string()
                } else {
                    format!("[null, {}]", rendered.join(", "))
                };
                format!("({{_data: {data}, len: {}, __ty: \"Vec\"}})", elems.len())
            }
            Inst::Index { base, index, kind } => {
                let (b, i) = (self.render_val(*base)?, self.render_val(*index)?);
                match kind {
                    ContainerKind::NativeBuffer => format!("__nb_checked_get({b}, {i})"),
                    ContainerKind::Vec => format!("__vec_checked_get({b}, {i})"),
                    // Map miss returns nil (raw VM op in Lua); JS Map.get returns
                    // undefined on miss, which `== null` treats as nil.
                    ContainerKind::Map => format!("{b}.get({i})"),
                }
            }
            Inst::Len { base, kind } => match kind {
                ContainerKind::NativeBuffer | ContainerKind::Vec => {
                    format!("{}.len", self.render_val(*base)?)
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
                other => {
                    return Err(err(format!(
                        "JS subset: native_buffer_alloc element ctype `{other}` has no TypedArray"
                    )))
                }
            },
            // A general field read is JS property access. NativeBuffer `.ptr`/
            // `.len` and Vec `._data`/`.len` are the same shape.
            Inst::Field { base, field } => {
                format!("{}.{field}", self.render_val(*base)?)
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

    fn render_call(&self, func: FuncRef, args: &[Idx<Inst>]) -> Result<String, CompilerError> {
        let cf = &self.ws.module(func.module).functions[func.func];
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

/// Deterministic JS name for a co-emitted callee, unique per `FuncRef`
/// (`<name>__m<module>_f<fn>`): concatenating independently emitted closure
/// programs can only ever collide on byte-identical definitions.
pub fn callee_js_name(ws: &Workspace, fr: FuncRef) -> String {
    let f = &ws.module(fr.module).functions[fr.func];
    format!("{}__m{}_f{}", f.name, fr.module.index(), raw(fr.func))
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
    // Same per-body pipeline the luajit path runs (`skull::mir_emit::emit_body_chunk`):
    // lower -> inline_functions -> cleanup -> recover_counted_loops -> residual-
    // bound-call validation. Inlining splices `@inline`/`@pure`/`Iterator`-bounded
    // callees, so a combinator chain monomorphizes and the post-inline residual
    // calls (below) drive co-emission of only the plain fns that survive. The
    // cache is per-call (step-3-simplest scope); measured against the ~2x wall-time
    // gate before considering a hoist.
    let mut cache = skull_transform::InlineCache::default();
    let mut names: HashMap<FuncRef, String> = roots.iter().cloned().collect();
    let mut queue: Vec<FuncRef> = roots.iter().map(|(fr, _)| *fr).collect();
    let mut seen: HashSet<FuncRef> = queue.iter().copied().collect();
    let mut lowered: Vec<(FuncRef, MirFunc)> = Vec::new();
    while let Some(fr) = queue.pop() {
        let f = &ws.module(fr.module).functions[fr.func];
        let mut mf = skull_mir::lower::lower_function(ws, scx, fr.module, fr.func)?;
        // Coroutines never inline, mirroring the luajit path's coroutine branch
        // (cleanup only, no inline / forl rewrite); the transform + trampoline
        // emission run in `emit_coroutine_js` at emission time.
        if mf.is_coroutine {
            skull_transform::cleanup(&mut mf);
        } else {
            skull_transform::inline_functions(ws, scx, fr, &mut mf, &mut cache)?;
            skull_transform::cleanup(&mut mf);
            // Shared closure conversion: every `Inst::Lambda` becomes an env
            // record / bare global + a hoisted sibling fn in `nested_fns`. Runs
            // BEFORE FORL recovery so a loop lifted into a lambda body is a plain
            // fn loop the emitter structures normally. `names[&fr]` is the unique
            // JS name -> collision-free `<name>__lambda<k>` hoisted names.
            skull_transform::convert_closures(&mut mf, &names[&fr]);
            skull_transform::recover_counted_loops(ws, &mut mf);
            for nf in &mut mf.nested_fns {
                skull_transform::recover_counted_loops(ws, nf);
            }
        }
        skull_transform::validate_no_residual_bound_calls(ws, f, &mf)?;
        // Discover co-emit callees from the POST-inline residual calls only, so a
        // spliced callee (its call inst dropped from its block) stops co-emitting.
        for callee in call_targets(&mf) {
            let cf = &ws.module(callee.module).functions[callee.func];
            // Mirror `render_call`'s decision exactly: the `get`/2 receiver
            // gate, then the intrinsic table (stdlib @intrinsic decls only),
            // then co-emit for plain-Rock bodies.
            // Mirror `render_call`: only the BUILTIN Map renders inline (skip its
            // co-emission); NativeBuffer via the intrinsic table (below); Vec and
            // user-struct `get` co-emit their pure-Rock bodies (fall through).
            if cf.name.as_str() == "get"
                && cf.param_decls.len() == 2
                && first_param_is_builtin_map(ws, cf)
            {
                continue;
            }
            if is_mapped_intrinsic(cf) || is_host_mapped(cf) || is_map_new(cf) {
                continue;
            }
            if cf.lua_body.is_some() || cf.is_extern {
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
        lowered.push((fr, mf));
    }
    // Scalar-ABI mismatch guard: a value-struct arg scalarizes to N leaf values
    // at the call, but a co-emitted callee that uses the param as a WHOLE value
    // keeps a single (un-expanded) MIR param -- so `argc > arity` and leaves would
    // be silently dropped. The residual co-emitted callees here were NOT inlined
    // (value-struct-whole-value use blocks inlining eligibility), so reject rather
    // than emit-and-hope. The legitimate `argc < arity` shape is an omitted
    // defaulted param (callee-side `== nil` fill; the omitted arg arrives as JS
    // `undefined`): accept down to the count of params WITHOUT a default.
    let arities: HashMap<FuncRef, usize> =
        lowered.iter().map(|(fr, mf)| (*fr, mf.params.len())).collect();
    for (_, mf) in &lowered {
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
                let (callee, argc) = match &mf.insts[iid] {
                    Inst::Call { func, args } | Inst::CallMulti { func, args, .. } => {
                        (func, args.len())
                    }
                    _ => continue,
                };
                if let Some(&arity) = arities.get(callee) {
                    let required = required_arg_count(ws, *callee, arity);
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
    // Program-wide hoisted closure-body names -- the only globals a capture-free
    // lambda creation site may reference (every other `Inst::Global` read stays
    // out of subset). Flat: nesting is one level (all lambdas are siblings).
    let hoisted: HashSet<ustr::Ustr> =
        lowered.iter().flat_map(|(_, mf)| mf.nested_fns.iter().map(|nf| nf.name)).collect();
    let mut out = String::new();
    for (fr, mf) in &lowered {
        if mf.is_coroutine {
            // The shared coroutine pipeline + trampoline step + JS ctor. Runs
            // its own closure conversion, so its hoisted names extend `hoisted`
            // locally inside.
            out.push_str(&emit_coroutine_js(ws, *fr, mf, &names[fr], &names, &hoisted)?);
            continue;
        }
        out.push_str(&emit_fn_with_callees(ws, mf, &names[fr], &names, &hoisted)?);
        // Hoisted closure bodies (`convert_closures`): each is a normal fn named
        // by its `MakeEnv`/`Global` `__fn` reference, emitted as a sibling.
        for nf in &mf.nested_fns {
            out.push_str(&emit_fn_with_callees(ws, nf, nf.name.as_str(), &names, &hoisted)?);
        }
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
        lowered.step.entry,
        &[lowered.s_cell, resume_arg],
    )?;
    // Hoisted lambda siblings are frame-free ordinary fns (their frame accesses
    // are already `e.c<S>.c<slot>` MIR) -- the normal structured path.
    let mut nested = std::mem::take(&mut lowered.step.nested_fns);
    for nf in nested.iter_mut() {
        skull_transform::cleanup(nf);
        skull_transform::recover_counted_loops(ws, nf);
        out.push_str(&emit_fn_with_callees(ws, nf, nf.name.as_str(), callees, &hoisted)?);
    }
    lowered.step.nested_fns = nested;
    for (k, &entry) in defer_entries.iter().enumerate() {
        out.push_str(&emit_fn_trampoline(
            ws,
            &lowered.step,
            &format!("{js_name}__defer{k}"),
            callees,
            &hoisted,
            entry,
            &[lowered.s_cell],
        )?);
    }
    for &(slot, entry) in &hook_entries {
        out.push_str(&emit_fn_trampoline(
            ws,
            &lowered.step,
            &format!("{js_name}{slot}"),
            callees,
            &hoisted,
            entry,
            &[lowered.s_cell],
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
        for i in 0..ch.tuple_width {
            let _ = write!(box_fields, ", \"{}\": s.{}", i + 1, skull_mir::lower::yield_slot(1 + i));
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

/// Every statically-resolved call target in `mf`, walking BLOCK inst-lists (not
/// the raw arena) so a spliced-away call's stale arena inst is not re-discovered
/// -- only the calls that survive inlining co-emit. Hoisted closure bodies are
/// separate `nested_fns` after `convert_closures`, so their calls are walked too.
fn call_targets(mf: &MirFunc) -> Vec<FuncRef> {
    fn walk(mf: &MirFunc, out: &mut Vec<FuncRef>) {
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
                match &mf.insts[iid] {
                    Inst::Call { func, .. } | Inst::CallMulti { func, .. } => out.push(*func),
                    _ => {}
                }
            }
        }
        for nf in &mf.nested_fns {
            walk(nf, out);
        }
    }
    let mut out = Vec::new();
    walk(mf, &mut out);
    out
}

/// The minimum argument count a co-emitted callee accepts: its MIR param count
/// minus the params carrying a default (each omitted trailing default arrives as
/// JS `undefined` and is filled callee-side). Defaults live on the fn's HIR body;
/// for the scalar defaults this gate targets (`step: int = 1`) each maps 1:1 to a
/// MIR param slot. A GC value-struct default would break the 1:1 assumption, but
/// value-struct construction rejects at emit independently, so no such call is
/// admitted here.
fn required_arg_count(ws: &Workspace, fr: FuncRef, arity: usize) -> usize {
    use bone_semantic::hir::BodyOwner;
    let n_defaults = ws
        .module(fr.module)
        .bodies
        .get(&BodyOwner::Function(fr.func))
        .map(|b| b.param_defaults.iter().filter(|d| d.is_some()).count())
        .unwrap_or(0);
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

/// A fixed-arity, host-bodied stdlib fn (extern or inline(lua)) with a
/// hand-written JS binding the emitter substitutes for the Lua body. The gate
/// is the host-body flag PLUS the (name, arity) match, so a plain-Rock user fn
/// of the same name co-emits normally and is never captured here.
fn host_fn_template(cf: &bone_semantic::hir::Function) -> Option<&'static str> {
    if !(cf.is_extern || cf.lua_body.is_some()) {
        return None;
    }
    match (cf.name.as_str(), cf.param_decls.len()) {
        ("tostring", 1) => Some("lua_tostring($1)"),
        ("__rock_format_float", 1) => Some("__rock_format_float($1)"),
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
        ("table_new", 0) => Some("({})"),
        // Vec: `{_data, len, __ty}` 1-indexed. `vec_new` builds it; `vec_push`
        // increments `len` then writes `_data[len]` (comma operator so it stays
        // an expression; `$1` is a cell load, safe to re-evaluate).
        ("vec_new", 0) => Some("({_data: [], len: 0, __ty: \"Vec\"})"),
        ("vec_push", 2) => Some("($1.len = $1.len + 1, $1._data[$1.len] = $2)"),
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
        // `tonumber(s)` -> a strict numeric parse (null on non-numeric / empty).
        ("parse_float", 1) => Some("__parse_float($1)"),
        // `Vec.sort(key_fn)`: sort the 1-indexed `_data` slice in place through
        // the `__vec_sort` runtime (the pure-Rock FlatVec.sort co-emits instead).
        ("sort", 2) => Some("__vec_sort($1, $2)"),
        // The `{x:spec}` interpolation format helpers -> their JS runtime ports.
        ("__rock_fmt_pad", 4) => Some("__rock_fmt_pad($1, $2, $3, $4)"),
        ("__rock_fmt_grouping", 3) => Some("__rock_fmt_grouping($1, $2, $3)"),
        ("__rock_fmt_binary", 1) => Some("__rock_fmt_binary($1)"),
        ("__rock_fmt_percent", 2) => Some("__rock_fmt_percent($1, $2)"),
        // `format_float(v, width, decimals)` reconstructs the `%W.Df` printf spec.
        ("format_float", 3) => Some("__rock_sprintf(\"%\" + $2 + \".\" + $3 + \"f\", $1)"),
        // Coroutine consumers (stdlib inline(lua) readers of the frame object).
        // IIFEs where an arg is read twice; `undefined -> null` normalization so
        // an unset frame field reads as Rock null (Lua nil parity). `resume` has
        // two arities (overload-by-arity: `c.resume()` vs `c.resume(arg)`).
        ("resume", 1) => Some("(function(c){var v=c.__resume(c);return v===undefined?null:v;})($1)"),
        ("resume", 2) => Some("(function(c,a){var v=c.__resume(c,a);return v===undefined?null:v;})($1, $2)"),
        ("result", 1) => Some("(function(c){var v=c.__return;return v===undefined?null:v;})($1)"),
        ("status", 1) => Some("(function(c){return c.__state === -2 ? \"errored\" : (c.__state < 0 ? \"dead\" : \"suspended\");})($1)"),
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
    is_host_print(cf) || host_fn_template(cf).is_some()
}

/// `map_new()`: pure Rock (`transmute(table_new())`), but intercepted to a real
/// JS `Map` rather than co-emitted to a plain object (which coerces keys).
fn is_map_new(cf: &bone_semantic::hir::Function) -> bool {
    cf.name.as_str() == "map_new" && cf.param_decls.is_empty()
}

/// The cells an inst writes: a `Store`'s target, or a multi-value call's dests.
/// These are the cells that need a hoisted `let` (the multi-assign / store binds
/// them, and a later block may read them across a branch merge).
fn inst_writes(inst: &Inst) -> &[Idx<Cell>] {
    match inst {
        Inst::Store { cell, .. } => std::slice::from_ref(cell),
        Inst::CallMulti { dests, .. }
        | Inst::CallValueMulti { dests, .. }
        | Inst::CallBoundMulti { dests, .. } => dests,
        _ => &[],
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
        Inst::MakeStruct { fields, .. } | Inst::MakeValueStruct { fields, .. } | Inst::MakeEnv { fields, .. } => {
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

