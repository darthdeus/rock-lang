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
use skull_mir::mir::{Block, ContainerKind, FuncRef, Inst, Lit, MirFunc, Term};
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
    let mut em = Emitter {
        ws,
        mf,
        callees,
        used: used_values(mf),
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
    /// Known co-emitted plain-Rock callees (`FuncRef` -> emitted JS name).
    callees: &'a HashMap<FuncRef, String>,
    /// Insts referenced as an operand somewhere (another inst or a terminator).
    /// An `Inst::Call` NOT in here is a statement-position call (`nb_set(..)`)
    /// and emits as an expression statement.
    used: HashSet<Idx<Inst>>,
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
            match &self.mf.insts[iid] {
                Inst::Store { cell, value } => {
                    let rhs = self.render_val(*value)?;
                    let line = format!("{} = {};", cell_name(*cell), rhs);
                    self.push(&line);
                }
                Inst::StoreIndex { base, index, value, kind } => match kind {
                    ContainerKind::NativeBuffer => {
                        let line = format!(
                            "__nb_checked_set({}, {}, {});",
                            self.render_val(*base)?,
                            self.render_val(*index)?,
                            self.render_val(*value)?
                        );
                        self.push(&line);
                    }
                    other => {
                        return Err(err(format!(
                            "JS subset: {other:?} index store not supported"
                        )))
                    }
                },
                // A call in statement position (value unused): `nb_set(b, i, v)`.
                Inst::Call { .. } if !self.used.contains(&iid) => {
                    let Inst::Call { func, args } = self.mf.insts[iid].clone() else {
                        unreachable!()
                    };
                    let line = format!("{};", self.render_call(func, &args)?);
                    self.push(&line);
                }
                // An unused indirect call would be a silently-dropped effect.
                Inst::CallValue { .. } if !self.used.contains(&iid) => {
                    return Err(err("JS subset: indirect call not supported"));
                }
                // Any other unused value inst is pure and dead -- skip. Used
                // value insts render inline at their use site. Remaining
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
            Inst::Const(Lit::Nil) => "null".to_string(),
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
            // NativeBuffer surface: `{ptr, len}` object literal; `buf[i]` via
            // the bounds-checked shim (JS_NB_CHECK_SHIMS preamble, matching
            // the stdlib `__nb_checked_get` luajit emits); `.len`/`.ptr` are
            // plain property reads.
            Inst::MakeStruct { ty, fields } => {
                let name = ty.map(|t| t.to_string()).unwrap_or_default();
                if !is_native_buffer(&name) {
                    return Err(err(format!(
                        "JS subset: struct construction only supported for NativeBuffer types, got `{name}`"
                    )));
                }
                let rendered: Result<Vec<String>, CompilerError> = fields
                    .iter()
                    .map(|(f, v)| Ok(format!("{f}: {}", self.render_val(*v)?)))
                    .collect();
                format!("({{{}}})", rendered?.join(", "))
            }
            Inst::Index { base, index, kind } => match kind {
                ContainerKind::NativeBuffer => format!(
                    "__nb_checked_get({}, {})",
                    self.render_val(*base)?,
                    self.render_val(*index)?
                ),
                other => return Err(err(format!("JS subset: {other:?} indexing not supported"))),
            },
            Inst::Len { base, kind } => match kind {
                ContainerKind::NativeBuffer => format!("{}.len", self.render_val(*base)?),
                other => return Err(err(format!("JS subset: {other:?} length not supported"))),
            },
            Inst::Field { base, field } => match field.as_str() {
                "ptr" | "len" => format!("{}.{field}", self.render_val(*base)?),
                _ => {
                    return Err(err(format!(
                        "JS subset: field read `.{field}` not supported (only NativeBuffer `.ptr`/`.len`)"
                    )))
                }
            },
            other => {
                return Err(err(format!(
                    "JS subset: cannot render {other:?} as a JS expression"
                )))
            }
        })
    }

    fn render_call(&self, func: FuncRef, args: &[Idx<Inst>]) -> Result<String, CompilerError> {
        let cf = &self.ws.module(func.module).functions[func.func];
        // The table keys on (name, arity); `get`/2 is also Vec/Map/Arena
        // surface, so the NativeBuffer template only applies to a NativeBuffer
        // receiver -- anything else rejects rather than kinda-working.
        if cf.name.as_str() == "get" && !first_param_is_native_buffer(self.ws, cf) {
            return Err(err(
                "JS subset: `get` is mapped only for NativeBuffer receivers",
            ));
        }
        let rargs: Result<Vec<String>, CompilerError> =
            args.iter().map(|a| self.render_val(*a)).collect();
        let rargs = rargs?;
        match JsIntrinsics.lookup(cf) {
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

/// Whether `f`'s first declared param is a `NativeBuffer*` nominal -- the
/// receiver gate for the overloaded `get`/2 table entry. Top-level fn param
/// types are always `TyId::Static`.
fn first_param_is_native_buffer(ws: &Workspace, f: &bone_semantic::hir::Function) -> bool {
    use bone_semantic::hir::{TyId, TypeCtor, TypeExpr};
    let Some(pd) = f.param_decls.first() else {
        return false;
    };
    let TyId::Static(mid, idx) = pd.ty else {
        return false;
    };
    matches!(
        ws.module(mid).ty_arena[idx],
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } if is_native_buffer(name.as_str())
    )
}

/// Every inst referenced as an operand by another inst or a terminator. The
/// match is EXHAUSTIVE on purpose: a new Inst variant must decide its operands
/// here or the compiler errors, so the statement-position-call detection in
/// `emit_block_effects` can never silently rot.
fn used_values(mf: &MirFunc) -> HashSet<Idx<Inst>> {
    let mut used = HashSet::new();
    let mark = |ids: &[Idx<Inst>], used: &mut HashSet<Idx<Inst>>| {
        for &i in ids {
            used.insert(i);
        }
    };
    for (_, inst) in mf.insts.iter() {
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
            | Inst::Shr(a, b) => mark(&[*a, *b], &mut used),
            Inst::ToString(x)
            | Inst::FloatToInt(x)
            | Inst::IntToFloat(x)
            | Inst::Neg(x)
            | Inst::Not(x)
            | Inst::BitNot(x)
            | Inst::CoroSetInner(x) => mark(&[*x], &mut used),
            Inst::Format { value, .. } => mark(&[*value], &mut used),
            Inst::FormatDyn { value, fmt } => mark(&[*value, *fmt], &mut used),
            Inst::Call { args, .. } | Inst::CallBoundMulti { args, .. } => mark(args, &mut used),
            Inst::CallMulti { args, .. } => mark(args, &mut used),
            Inst::CallValue { callee, args } | Inst::CallValueMulti { callee, args, .. } => {
                mark(&[*callee], &mut used);
                mark(args, &mut used);
            }
            Inst::MakeTuple(elems) | Inst::MakeVec(elems) => mark(elems, &mut used),
            Inst::MakeStruct { fields, .. } | Inst::MakeValueStruct { fields, .. } => {
                for (_, v) in fields {
                    used.insert(*v);
                }
            }
            Inst::MakeEnum { positional, named, .. } => {
                mark(positional, &mut used);
                for (_, v) in named {
                    used.insert(*v);
                }
            }
            Inst::MakeEnumDyn { tag, positional, named, .. } => {
                mark(&[*tag], &mut used);
                mark(positional, &mut used);
                for (_, v) in named {
                    used.insert(*v);
                }
            }
            Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => mark(&[*base], &mut used),
            Inst::Index { base, index, .. } => mark(&[*base, *index], &mut used),
            Inst::Len { base, .. } => mark(&[*base], &mut used),
            Inst::Store { value, .. } => mark(&[*value], &mut used),
            Inst::StoreField { base, value, .. } => mark(&[*base, *value], &mut used),
            Inst::StoreIndex { base, index, value, .. } => {
                mark(&[*base, *index, *value], &mut used)
            }
            Inst::StoreGlobal { value, .. } => mark(&[*value], &mut used),
            Inst::ArrayPush { arr, value, .. } => mark(&[*arr, *value], &mut used),
            Inst::CoroHook { lambda, .. } => mark(&[*lambda], &mut used),
            Inst::MapNext { table, .. } => mark(&[*table], &mut used),
        }
    }
    for (_, block) in mf.blocks.iter() {
        match &block.term {
            Term::Return(vs) => mark(vs, &mut used),
            Term::Unreachable => {}
            Term::Goto(_) => {}
            Term::Branch { cond, .. } => mark(&[*cond], &mut used),
            Term::Switch { discr, .. } => mark(&[*discr], &mut used),
            Term::Yield { values, .. } => mark(values, &mut used),
        }
    }
    used
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
