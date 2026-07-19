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
    emit_fn_with_callees(ws, mf, js_name, &HashMap::new())
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
        use_count,
        inlinable,
        structure: st,
        counted,
        general,
        active: HashSet::new(),
        visited: HashSet::new(),
        lambdas: HashMap::new(),
        lambda_owned: compute_lambda_owned(mf),
        out: String::new(),
        indent: 1,
    };

    // Pre-render every lambda body to a JS arrow, inner-first: a nested lambda's
    // `Inst::Lambda` is lowered before its enclosing one, so ascending inst order
    // renders inner arrows before an outer arrow that references them.
    let mut lambda_insts: Vec<Idx<Inst>> = mf
        .insts
        .iter()
        .filter(|(_, i)| matches!(i, Inst::Lambda { .. }))
        .map(|(id, _)| id)
        .collect();
    lambda_insts.sort_by_key(|&i| raw(i));
    let hoistable = hoistable_lambdas(mf, &em.lambda_owned);
    let mut hoisted: Vec<String> = Vec::new();
    for iid in lambda_insts {
        let arrow = em.render_lambda(iid)?;
        if hoistable.contains(&iid) {
            // A shared module-level value: same-site references are `==` (matches
            // luajit P3). The name is unique (`js_name` is program-unique, inst
            // ids are fn-unique).
            let name = format!("{js_name}__lam{}", raw(iid));
            hoisted.push(format!("const {name} = {arrow};\n"));
            em.lambdas.insert(iid, name);
        } else {
            em.lambdas.insert(iid, arrow);
        }
    }

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
            if let Inst::Store { cell, .. } = mf.insts[iid] {
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
    Ok(format!("{}{header}{}}}\n", hoisted.concat(), em.out))
}

struct Emitter<'a> {
    ws: &'a Workspace,
    mf: &'a MirFunc,
    /// Known co-emitted plain-Rock callees (`FuncRef` -> emitted JS name).
    callees: &'a HashMap<FuncRef, String>,
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
    /// Each `Lambda` inst -> its pre-rendered JS arrow expression. Populated
    /// inner-first (ascending inst id) before the main walk, so a `render_val`
    /// of a `Lambda` operand is a map lookup even for nested lambdas.
    lambdas: HashMap<Idx<Inst>, String>,
    /// Each `Lambda` inst -> the cells its arrow body must `let`-declare.
    lambda_owned: HashMap<Idx<Inst>, Vec<Idx<Cell>>>,
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
            // A natural-loop header is dispatched structurally (before the
            // visited guard): `emit_general` re-enters `emit_seq` on the header
            // for its condition region, so the header must not be pre-marked.
            if !self.active.contains(&cur) {
                if let Some(li) = self.counted.get(&cur).copied() {
                    self.emit_counted(li)?;
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
                // reducible-but-unstructured shape: a branch whose arms share a
                // tail (a control-flow short-circuit, e.g. a `match` range/tuple
                // pattern) that this emitter does not yet recover. Reject cleanly.
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
        // Empty else -> then-only `if`; continue at the else target.
        if let Some(j) = self.empty_forward(else_bb) {
            self.push(&format!("if ({cond_s}) {{"));
            self.indent += 1;
            self.emit_seq(then_bb, Some(j), ctx)?;
            self.indent -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        // Empty then -> invert and emit the else arm only.
        if let Some(j) = self.empty_forward(then_bb) {
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
        let merge = self
            .structure
            .ipdom(blk)
            .filter(|&m| Some(m) != ctx.cont && Some(m) != ctx.brk);
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
    fn emit_counted(&mut self, li: LoopInfo) -> Result<(), CompilerError> {
        let slot = cell_name(li.slot);
        // The bound is a `Load` of a hoisted preheader cell (or a const),
        // rendered inline so it never depends on a header-block temp we skip.
        let bound = self.render_inst_expr(li.bound)?;
        let cmp = if li.inclusive { "<=" } else { "<" };
        self.push(&format!("for (; {slot} {cmp} {bound}; {slot} = {slot} + 1) {{"));
        self.indent += 1;
        let ctx = LoopCtx { cont: Some(li.latch), brk: Some(li.exit) };
        let body_len = self.out.len();
        self.emit_seq(li.body_entry, None, ctx)?;
        self.trim_trailing_continue(body_len);
        self.indent -= 1;
        self.push("}");
        Ok(())
    }

    /// A general loop: `while (true) { <cond region>; if (!(cond)) break; <body> }`.
    /// The condition region may be a multi-block diamond (`while a && b`), so it
    /// goes through the structurizer recursively. A range-`for` is just this shape
    /// whose test IS the header `Branch` (its else is the exit) and whose latch is
    /// the increment block -- `continue` runs the increment before re-testing.
    fn emit_general(&mut self, region: &LoopRegion, outer: LoopCtx) -> Result<(), CompilerError> {
        let h = region.header;
        let exit = region.exit;
        let (test, cond, body_start, break_when_true) = self.find_test(region)?;

        self.active.insert(h);
        self.push("while (true) {");
        self.indent += 1;

        self.emit_seq(h, Some(test), outer)?;
        self.emit_block_effects(test)?;
        self.visited.insert(test);

        let cond_s = self.render_val(cond)?;
        if break_when_true {
            self.push(&format!("if ({cond_s}) {{ break; }}"));
        } else {
            self.push(&format!("if (!({cond_s})) {{ break; }}"));
        }

        let ctx = LoopCtx { cont: Some(h), brk: Some(exit) };
        let body_len = self.out.len();
        self.emit_seq(body_start, None, ctx)?;
        self.trim_trailing_continue(body_len);

        self.indent -= 1;
        self.push("}");
        self.active.remove(&h);
        Ok(())
    }

    /// The loop's condition test: the unique body `Branch` with a direct successor
    /// to the exit. Returns `(test block, cond, body entry, break-when-cond-true)`.
    fn find_test(
        &self,
        region: &LoopRegion,
    ) -> Result<(Idx<Block>, Idx<Inst>, Idx<Block>, bool), CompilerError> {
        for &b in &region.body {
            if let Term::Branch { cond, then_bb, else_bb } = self.mf.blocks[b].term {
                if then_bb == region.exit {
                    return Ok((b, cond, else_bb, true));
                }
                if else_bb == region.exit {
                    return Ok((b, cond, then_bb, false));
                }
            }
        }
        Err(err("JS subset: unstructured loop (no single condition test)"))
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
            _ => return Err(err("JS subset: multi-value return not supported")),
        }
        Ok(())
    }

    /// Render a `Lambda` inst as a JS arrow `(<params>) => { <body> }`. The body
    /// is the lambda's sub-CFG (rooted at `entry`, in the same arenas): captured
    /// cells render to the same `c<N>` name they carry in the enclosing scope, so
    /// JS lexical capture stands in for the Lua upvalue. Emits into a scratch
    /// buffer so `render_inst_expr` (immutable) can look the string up later.
    fn render_lambda(&mut self, iid: Idx<Inst>) -> Result<String, CompilerError> {
        let Inst::Lambda { params, entry } = &self.mf.insts[iid] else {
            unreachable!("render_lambda on a non-Lambda inst");
        };
        let param_names: Vec<String> = params.iter().map(|&c| cell_name(c)).collect();
        let entry = *entry;
        let owned = self.lambda_owned.get(&iid).cloned().unwrap_or_default();

        let saved_out = std::mem::take(&mut self.out);
        let saved_indent = self.indent;
        self.indent = saved_indent + 1;
        if !owned.is_empty() {
            let names: Vec<String> = owned.iter().map(|&c| cell_name(c)).collect();
            self.push(&format!("let {};", names.join(", ")));
        }
        self.emit_seq(entry, None, LoopCtx::default())?;
        let body = std::mem::replace(&mut self.out, saved_out);
        self.indent = saved_indent;

        let close = "  ".repeat(saved_indent);
        Ok(format!("({}) => {{\n{body}{close}}}", param_names.join(", ")))
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
            // A closure value: the arrow was pre-rendered inner-first before the
            // main walk. Parenthesized so a `CallValue` of it (`(<arrow>)(args)`)
            // is a valid IIFE.
            Inst::Lambda { .. } => match self.lambdas.get(&iid) {
                Some(arrow) => format!("({arrow})"),
                None => return Err(err("JS subset: lambda not pre-rendered")),
            },
            // An indirect call `callee(args)`: the callee is parenthesized so an
            // inline arrow callee still parses as a call target.
            Inst::CallValue { callee, args } => {
                let c = self.render_val(*callee)?;
                let rendered: Result<Vec<String>, CompilerError> =
                    args.iter().map(|a| self.render_val(*a)).collect();
                format!("({c})({})", rendered?.join(", "))
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
        if cf.name.as_str() == "get" && cf.param_decls.len() == 2 {
            match first_param_nominal(self.ws, cf).as_deref() {
                Some(n) if is_native_buffer(n) => {} // intrinsic template path below
                Some("Map") => return Ok(format!("{}.get({})", rargs[0], rargs[1])),
                Some("Vec") => {} // co-emit path below
                _ => return Err(err("JS subset: `get` receiver not supported")),
            }
        }
        // Host-bodied stdlib fns (extern / inline(lua)) with a hand-written JS
        // binding: `print` (variadic -> the harness global), the fixed-arity
        // templates (`tostring`, `__rock_format_float`).
        if is_host_print(cf) {
            return Ok(format!("print({})", rargs.join(", ")));
        }
        if let Some(tmpl) = host_fn_template(cf) {
            let refs: Vec<&str> = rargs.iter().map(|s| s.as_str()).collect();
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
    let mut names: HashMap<FuncRef, String> = roots.iter().cloned().collect();
    let mut queue: Vec<FuncRef> = roots.iter().map(|(fr, _)| *fr).collect();
    let mut seen: HashSet<FuncRef> = queue.iter().copied().collect();
    let mut lowered: Vec<(FuncRef, MirFunc)> = Vec::new();
    while let Some(fr) = queue.pop() {
        let mf = skull_mir::lower::lower_function(ws, scx, fr.module, fr.func)?;
        for callee in call_targets(&mf) {
            let cf = &ws.module(callee.module).functions[callee.func];
            // Mirror `render_call`'s decision exactly: the `get`/2 receiver
            // gate, then the intrinsic table (stdlib @intrinsic decls only),
            // then co-emit for plain-Rock bodies.
            if cf.name.as_str() == "get" && cf.param_decls.len() == 2 {
                match first_param_nominal(ws, cf).as_deref() {
                    // NativeBuffer via the intrinsic table (handled below); Map
                    // rendered inline in `render_call` (skip co-emission); Vec
                    // co-emits its pure-Rock body (falls through).
                    Some(n) if is_native_buffer(n) => {}
                    Some("Map") => continue,
                    Some("Vec") => {}
                    _ => return Err(err("JS subset: `get` receiver not supported")),
                }
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
    let mut out = String::new();
    for (fr, mf) in &lowered {
        out.push_str(&emit_fn_with_callees(ws, mf, &names[fr], &names)?);
    }
    Ok(out)
}

/// Every statically-resolved call target in `mf` (incl. lambda sub-CFGs --
/// the inst arena is shared).
fn call_targets(mf: &MirFunc) -> Vec<FuncRef> {
    let mut out = Vec::new();
    for (_, inst) in mf.insts.iter() {
        match inst {
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } => out.push(*func),
            _ => {}
        }
    }
    out
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


/// The nominal type name of `f`'s first declared param (`Vec`, `Map`,
/// `NativeBuffer*`, ...), for receiver-based dispatch of the overloaded
/// `get`/2. `None` if there is no first param or it is not a nominal App.
fn first_param_nominal(ws: &Workspace, f: &bone_semantic::hir::Function) -> Option<String> {
    use bone_semantic::hir::{TyId, TypeCtor, TypeExpr};
    let pd = f.param_decls.first()?;
    let TyId::Static(mid, idx) = pd.ty else {
        return None;
    };
    // `[T]` and `Map<K,V>` are distinct ctors (`Array`/`Map`), not `Nominal` --
    // canonicalize them to the receiver-dispatch names the emitter keys on.
    match &ws.module(mid).ty_arena[idx] {
        TypeExpr::App { ctor: TypeCtor::Array, .. } => Some("Vec".to_string()),
        TypeExpr::App { ctor: TypeCtor::Map, .. } => Some("Map".to_string()),
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } => Some(name.as_str().to_string()),
        _ => None,
    }
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
        Inst::MakeStruct { fields, .. } | Inst::MakeValueStruct { fields, .. } => {
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
    )
}

/// Use counts (dense by raw inst id, over reachable blocks) + the inlinable
/// mask: an `always_inlinable` value used EXACTLY ONCE with def and use in the
/// SAME block folds into that use; everything else materializes at its program
/// position. Same-block + single-use makes the fold order-safe.
fn compute_inlinable(mf: &MirFunc) -> (Vec<u32>, Vec<bool>) {
    const NONE: u32 = u32::MAX;
    let ni = mf.insts.len();
    let mut use_count = vec![0u32; ni];
    let mut use_block = vec![NONE; ni];
    let mut def_block = vec![NONE; ni];
    let blocks = emitted_blocks(mf);
    for &b in &blocks {
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
    for &b in &blocks {
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

/// Every block the emitter walks: the fn's entry component plus each `Lambda`'s
/// sub-CFG. A lambda body is unreachable from `mf.entry` (only its `Inst::Lambda`
/// operand references it), so `reachable_blocks` alone misses it -- and the value
/// model would then leave a lambda-body temp unmaterialized. Order is
/// deterministic (entry component first, then lambdas in inst order).
fn emitted_blocks(mf: &MirFunc) -> Vec<Idx<Block>> {
    let mut out = reachable_blocks(mf);
    let mut seen: HashSet<Idx<Block>> = out.iter().copied().collect();
    for (_, inst) in mf.insts.iter() {
        if let Inst::Lambda { entry, .. } = inst {
            for b in reachable_from(mf, *entry) {
                if seen.insert(b) {
                    out.push(b);
                }
            }
        }
    }
    out
}

/// Cells that a lambda arrow must `let`-declare itself, keyed by the `Lambda`
/// inst. A stored (non-param) cell belongs to the INNERMOST scope whose body
/// contains all its store sites: a cell stored only inside one lambda's body is
/// arrow-local; a captured-then-mutated cell (stored in the enclosing scope too)
/// stays owned by the enclosing scope and is declared there, so a single JS
/// binding is shared -- closure capture by reference, exactly like a Lua upvalue.
fn compute_lambda_owned(mf: &MirFunc) -> HashMap<Idx<Inst>, Vec<Idx<Cell>>> {
    let mut params: HashSet<u32> = mf.params.iter().map(|&c| raw(c)).collect();
    let mut lambdas: Vec<(Idx<Inst>, HashSet<Idx<Block>>)> = Vec::new();
    for (id, inst) in mf.insts.iter() {
        if let Inst::Lambda { params: ps, entry } = inst {
            params.extend(ps.iter().map(|&c| raw(c)));
            lambdas.push((id, reachable_from(mf, *entry).into_iter().collect()));
        }
    }
    let mut store_blocks: HashMap<u32, (Idx<Cell>, HashSet<Idx<Block>>)> = HashMap::new();
    for (b, blk) in mf.blocks.iter() {
        for &iid in &blk.insts {
            if let Inst::Store { cell, .. } = mf.insts[iid] {
                store_blocks.entry(raw(cell)).or_insert_with(|| (cell, HashSet::new())).1.insert(b);
            }
        }
    }
    let mut owned: HashMap<Idx<Inst>, Vec<Idx<Cell>>> = HashMap::new();
    for (craw, (cell, blocks)) in &store_blocks {
        if params.contains(craw) {
            continue;
        }
        // Innermost (smallest-body) lambda whose body contains every store site.
        // A cell whose stores span the enclosing scope is owned there (declared
        // at the fn top by the reachable-from-entry pass), so it lands in no
        // lambda here.
        let best = lambdas
            .iter()
            .filter(|(_, body)| blocks.iter().all(|b| body.contains(b)))
            .min_by_key(|(_, body)| body.len());
        if let Some((lid, _)) = best {
            owned.entry(*lid).or_default().push(*cell);
        }
    }
    for v in owned.values_mut() {
        v.sort_by_key(|&c| raw(c));
    }
    owned
}

/// Lambdas that carry NO capture and can be hoisted to a single module-level
/// value. luajit shares one hoisted fn per capture-free site (P3), so two
/// references to the same-site lambda are `==` (identity leaf); an inline arrow
/// would mint a fresh value per call and diverge. A lambda is hoistable iff every
/// cell it references is its own param or arrow-local AND it nests no lambda (a
/// nested capture would reach an enclosing cell not in module scope). `owned` is
/// [`compute_lambda_owned`]'s result.
fn hoistable_lambdas(
    mf: &MirFunc,
    owned: &HashMap<Idx<Inst>, Vec<Idx<Cell>>>,
) -> HashSet<Idx<Inst>> {
    let mut out = HashSet::new();
    for (id, inst) in mf.insts.iter() {
        let Inst::Lambda { params, entry } = inst else { continue };
        let body: HashSet<Idx<Block>> = reachable_from(mf, *entry).into_iter().collect();
        let mut local: HashSet<u32> = params.iter().map(|&c| raw(c)).collect();
        local.extend(owned.get(&id).into_iter().flatten().map(|&c| raw(c)));
        let mut hoistable = true;
        for &b in &body {
            for &iid in &mf.blocks[b].insts {
                match &mf.insts[iid] {
                    Inst::Lambda { .. } => hoistable = false,
                    Inst::Load(c) | Inst::Store { cell: c, .. } => {
                        if !local.contains(&raw(*c)) {
                            hoistable = false;
                        }
                    }
                    _ => {}
                }
            }
        }
        if hoistable {
            out.insert(id);
        }
    }
    out
}
