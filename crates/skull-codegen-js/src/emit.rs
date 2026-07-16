//! Minimal MIR->JS emitter (docs/js-backend-plan.md S2) for the scalar
//! conformance subset: the fallback bodies of the eight differential
//! intrinsics. Structured (goto-free) output modeled on the GLSL backend's
//! `emit_seq`/`emit_branch`, NOT the goto-linear Lua backend -- JS, like GLSL,
//! has no goto, so a `Branch` recovers into `if/else` and its arms rejoin at the
//! block's immediate post-dominator. Any MIR op outside the subset is a spanned
//! `CompilerError` (reject, don't emit-and-hope); the subset grows with the
//! backend.

use std::collections::{HashMap, HashSet};

use bone_semantic::inference::Workspace;
use comfy_la_arena::Idx;
use skull_mir::intrinsics::{Emission, IntrinsicTable};
use skull_mir::mir::{Block, Inst, Lit, MirFunc, Term};
use skull_parser::{CompilerError, Span};

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

/// Emit `mf` as `function <js_name>(...) { ... }`. `js_name` is the caller's
/// chosen JS identity (the S3/S4 harness names each fallback fn).
pub fn emit_fn(ws: &Workspace, mf: &MirFunc, js_name: &str) -> Result<String, CompilerError> {
    if mf.is_coroutine {
        return Err(err("JS subset: coroutine fn not supported"));
    }
    let mut em = Emitter {
        ws,
        mf,
        ipdom: post_doms(mf),
        visited: HashSet::new(),
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

    em.emit_seq(mf.entry, None)?;
    Ok(format!("{header}{}}}\n", em.out))
}

struct Emitter<'a> {
    ws: &'a Workspace,
    mf: &'a MirFunc,
    /// Block -> its immediate post-dominator (`None` == the virtual exit: the
    /// block's arms all diverge, so the `if` has no fall-through).
    ipdom: HashMap<Idx<Block>, Option<Idx<Block>>>,
    visited: HashSet<Idx<Block>>,
    out: String,
    indent: usize,
}

impl Emitter<'_> {
    fn push(&mut self, line: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(line);
        self.out.push('\n');
    }

    /// A block that produces no statement and just forwards (`Some(target)`): an
    /// empty `if`/`else` arm.
    fn empty_forward(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        if let Term::Goto(t) = self.mf.blocks[b].term {
            let has_store = self.mf.blocks[b]
                .insts
                .iter()
                .any(|&i| matches!(self.mf.insts[i], Inst::Store { .. }));
            if !has_store {
                return Some(t);
            }
        }
        None
    }

    fn emit_seq(&mut self, start: Idx<Block>, stop: Option<Idx<Block>>) -> Result<(), CompilerError> {
        let mut cur = start;
        loop {
            if Some(cur) == stop {
                return Ok(());
            }
            if !self.visited.insert(cur) {
                return Err(err("JS subset: unstructured control flow (revisited block -- loop?)"));
            }
            self.emit_block_effects(cur)?;
            match self.mf.blocks[cur].term.clone() {
                Term::Return(vs) => {
                    self.emit_return(&vs)?;
                    return Ok(());
                }
                Term::Unreachable => return Ok(()),
                Term::Goto(t) => {
                    if Some(t) == stop {
                        return Ok(());
                    }
                    cur = t;
                }
                Term::Branch { cond, then_bb, else_bb } => {
                    match self.emit_branch(cur, cond, then_bb, else_bb, stop)? {
                        Some(m) => cur = m,
                        None => return Ok(()),
                    }
                }
                Term::Switch { .. } => return Err(err("JS subset: `match`/switch not supported")),
                Term::Yield { .. } => return Err(err("JS subset: `yield` not supported")),
            }
        }
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
    ) -> Result<Option<Idx<Block>>, CompilerError> {
        let cond_s = self.render_val(cond)?;
        // Empty else -> then-only `if`; continue at the else target.
        if let Some(j) = self.empty_forward(else_bb) {
            self.push(&format!("if ({cond_s}) {{"));
            self.indent += 1;
            self.emit_seq(then_bb, Some(j))?;
            self.indent -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        // Empty then -> invert and emit the else arm only.
        if let Some(j) = self.empty_forward(then_bb) {
            self.push(&format!("if (!({cond_s})) {{"));
            self.indent += 1;
            self.emit_seq(else_bb, Some(j))?;
            self.indent -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        let merge = self.ipdom.get(&blk).copied().flatten();
        let arm_stop = merge.map(Some).unwrap_or(stop);
        self.push(&format!("if ({cond_s}) {{"));
        self.indent += 1;
        self.emit_seq(then_bb, arm_stop)?;
        self.indent -= 1;
        self.push("} else {");
        self.indent += 1;
        self.emit_seq(else_bb, arm_stop)?;
        self.indent -= 1;
        self.push("}");
        Ok(merge)
    }

    fn emit_block_effects(&mut self, b: Idx<Block>) -> Result<(), CompilerError> {
        for iid in self.mf.blocks[b].insts.clone() {
            if let Inst::Store { cell, value } = self.mf.insts[iid] {
                let rhs = self.render_val(value)?;
                let line = format!("{} = {};", cell_name(cell), rhs);
                self.push(&line);
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

    /// Render a value instruction as a JS expression (inlined; MIR values are
    /// single-use, so no CSE duplication). JS is untyped, so the int/float split
    /// collapses to one operator and `IntToFloat` is identity.
    fn render_val(&self, iid: Idx<Inst>) -> Result<String, CompilerError> {
        let bin = |a: &Idx<Inst>, op: &str, b: &Idx<Inst>, s: &Self| -> Result<String, CompilerError> {
            Ok(format!("({} {} {})", s.render_val(*a)?, op, s.render_val(*b)?))
        };
        Ok(match &self.mf.insts[iid] {
            Inst::Const(Lit::Int(i)) => i.to_string(),
            Inst::Const(Lit::Float(f)) => js_number(*f)?,
            Inst::Const(Lit::Bool(b)) => b.to_string(),
            Inst::Const(Lit::Nil) => return Err(err("JS subset: null literal not supported")),
            Inst::Const(Lit::Str(_)) => return Err(err("JS subset: string literal not supported")),
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
            Inst::Eq(a, b) => bin(a, "===", b, self)?,
            Inst::NotEq(a, b) => bin(a, "!==", b, self)?,
            Inst::Neg(x) => format!("(-{})", self.render_val(*x)?),
            Inst::Not(x) => format!("(!{})", self.render_val(*x)?),
            // Identity on JS (all doubles), like codegen-lua on luajit.
            Inst::IntToFloat(x) => self.render_val(*x)?,
            // Rock float->int truncates toward -inf == floor.
            Inst::FloatToInt(x) => format!("Math.floor({})", self.render_val(*x)?),
            Inst::Call { func, args } => self.render_call(*func, args)?,
            other => {
                return Err(err(format!(
                    "JS subset: cannot render {other:?} as a JS expression"
                )))
            }
        })
    }

    fn render_call(
        &self,
        func: skull_mir::mir::FuncRef,
        args: &[Idx<Inst>],
    ) -> Result<String, CompilerError> {
        let cf = &self.ws.module(func.module).functions[func.func];
        match JsIntrinsics.lookup(cf) {
            Some(Emission::Template(tmpl)) => {
                let rargs: Result<Vec<String>, CompilerError> =
                    args.iter().map(|a| self.render_val(*a)).collect();
                let rargs = rargs?;
                let refs: Vec<&str> = rargs.iter().map(|s| s.as_str()).collect();
                Ok(fill_template(tmpl, &refs))
            }
            _ => Err(err(format!(
                "JS subset: call to `{}` is not a mapped JS intrinsic",
                cf.name
            ))),
        }
    }
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

fn dfs_post(u: usize, preds: &[Vec<usize>], visited: &mut [bool], order: &mut Vec<usize>) {
    visited[u] = true;
    for &v in &preds[u] {
        if !visited[v] {
            dfs_post(v, preds, visited, order);
        }
    }
    order.push(u);
}

/// Blocks reachable from the entry, in discovery order.
fn reachable_blocks(mf: &MirFunc) -> Vec<Idx<Block>> {
    let mut reachable = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![mf.entry];
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

/// Immediate post-dominators (Cooper-Harvey-Kennedy on the reversed CFG rooted
/// at a virtual exit). Value `None` == the block's ipdom is the virtual exit.
/// The conformance subset is acyclic; the algorithm is general.
fn post_doms(mf: &MirFunc) -> HashMap<Idx<Block>, Option<Idx<Block>>> {
    let reachable = reachable_blocks(mf);
    let n = reachable.len();
    let exit = n; // virtual exit node
    let idx_of: HashMap<Idx<Block>, usize> =
        reachable.iter().enumerate().map(|(i, &b)| (b, i)).collect();

    // Forward successors in dense indices (exit for return/unreachable).
    let mut fsucc: Vec<Vec<usize>> = vec![Vec::new(); n + 1];
    for (i, &b) in reachable.iter().enumerate() {
        for s in succ(mf, b) {
            fsucc[i].push(s.map_or(exit, |t| idx_of[&t]));
        }
    }
    // Forward predecessors (== successors in the reversed graph G').
    let mut fpred: Vec<Vec<usize>> = vec![Vec::new(); n + 1];
    for i in 0..n {
        for &d in &fsucc[i] {
            fpred[d].push(i);
        }
    }

    // Postorder of G' from exit; G' successors of x are fpred[x].
    let mut po_order = Vec::new();
    let mut visited = vec![false; n + 1];
    dfs_post(exit, &fpred, &mut visited, &mut po_order);
    let mut po_num = vec![0usize; n + 1];
    for (num, &node) in po_order.iter().enumerate() {
        po_num[node] = num;
    }

    let intersect = |mut a: usize, mut b: usize, idom: &[Option<usize>]| -> usize {
        while a != b {
            while po_num[a] < po_num[b] {
                a = idom[a].expect("post-idom finger");
            }
            while po_num[b] < po_num[a] {
                b = idom[b].expect("post-idom finger");
            }
        }
        a
    };

    let mut idom: Vec<Option<usize>> = vec![None; n + 1];
    idom[exit] = Some(exit);
    let rpo: Vec<usize> = po_order.iter().rev().copied().collect();
    let mut changed = true;
    while changed {
        changed = false;
        for &b in &rpo {
            if b == exit {
                continue;
            }
            // Preds of b in G' are its forward successors.
            let mut new_idom: Option<usize> = None;
            for &p in &fsucc[b] {
                if idom[p].is_some() {
                    new_idom = Some(match new_idom {
                        None => p,
                        Some(cur) => intersect(p, cur, &idom),
                    });
                }
            }
            if idom[b] != new_idom {
                idom[b] = new_idom;
                changed = true;
            }
        }
    }

    let mut out = HashMap::new();
    for (i, &b) in reachable.iter().enumerate() {
        let ip = match idom[i] {
            Some(d) if d != exit => Some(reachable[d]),
            _ => None,
        };
        out.insert(b, ip);
    }
    out
}
