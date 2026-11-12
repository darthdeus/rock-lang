//! Bounds-check elision for `[T]` element accesses: a forward dataflow over
//! in-bounds facts marks every `xs[i]` read or write whose index is provably in
//! `[0, xs.len)` as [`ContainerKind::VecInBounds`], which the backends index
//! raw. Results and errors are unchanged: an access is only marked where the
//! check could not have raised.
//!
//! Three facts are tracked about a *key*, which is either the value an
//! instruction produced or a place (a local cell, a global, a field or an
//! element of another key):
//!
//! - `nonneg(k)`: the integer at `k` is `>= 0`.
//! - `le_len(k, v)`: the integer at `k` is at most the live length of the
//!   vector at `v`. A `len` read yields one; a push keeps it (the length only
//!   grew); anything that may shrink a vector drops every one.
//! - `lt(k, v)`: the integer at `k` is below the live length of `v`. Taking
//!   the true edge of `k < b` with `le_len(b, v)` yields one, as does a checked
//!   access `v[k]` (it would have raised otherwise).
//!
//! A value and a place are tied by `same(value, place)`: the value is what the
//! place holds right now. A load ties its value to its cell, a field read to
//! the field of its base's key, and a store ties the stored value to the cell;
//! the tie breaks when the place is written. Facts flow across a tie in both
//! directions, so a bound read into a cell in the loop's preheader reaches the
//! compare that loads the cell in the header, and a guard on one load of `i`
//! covers the access that loads `i` again. A value that was read before its
//! place changed keeps only its own facts, which is what makes a stale snapshot
//! harmless.
//!
//! An access `v[k]` is in bounds when `nonneg(k)` and `lt(k, v)` both hold
//! before it. This covers a counted `for i in lo..xs.len()` (the bound cell is
//! `le_len`, the counter starts nonnegative and only increments), a
//! `while i < xs.len` (the header re-reads the length), a dominating guard
//! `if i >= 0 && i < xs.len`, and a dominating checked access `xs[i]` before
//! `xs[i] = ..`.
//!
//! Facts die at the writes that could falsify them. A store to a cell drops
//! facts about that cell; a field store drops facts through that field name,
//! and a `len` store drops every length fact; a global store drops facts
//! through that global; an element store drops facts through any element. A
//! call drops every length fact and every fact through memory (a global, a
//! field or an element): the callee may shrink any vector it can reach or
//! rebind any field. Only the stdlib push keeps them, because it only grows
//! the vector and only writes a slot that was outside it; `@pure` is not
//! trusted, since the checker admits `@debug_impure` callees and never
//! verifies an `inline(lua)` or `extern` body. A yield drops everything. An instruction that executes
//! again (a loop) drops the facts about its previous value.
//!
//! Merging at a join intersects the incoming facts, so a fact that reaches the
//! loop header around the back edge must have survived the whole body: a body
//! that pops, clears or calls into unknown code keeps its checks. Early exits
//! need no care: a `break` or `return` only removes later accesses.
//!
//! Runs after closure conversion, so every cell is a plain local of this body:
//! a captured mutable variable is a box reached through a field by then. A
//! body that still holds a lambda is left alone.
//!
//! `ROCK_SKULL_BOUNDS_OFF=1` keeps every check, so the output with and without
//! the pass can be compared; a measurement switch, never a shipped mode.

use rustc_hash::FxHashSet as HashSet;

use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::cfg::{predecessors, reverse_postorder};
use skull_mir::mir::*;
use ustr::Ustr;

fn raw<T>(i: Idx<T>) -> usize {
    i.into_raw().into_u32() as usize
}

/// Mark every provably in-bounds `[T]` element access in `func`. Returns how
/// many accesses were marked.
pub fn elide_bounds_checks(ws: Option<&Workspace>, func: &mut MirFunc) -> usize {
    if disabled_by_env() {
        return 0;
    }
    let rpo = reverse_postorder(func, func.entry);
    let mut has_checked = false;
    for &b in &rpo {
        for &i in &func.blocks[b].insts {
            match func.insts[i] {
                Inst::Lambda { .. } => return 0,
                Inst::Index {
                    kind: ContainerKind::Vec,
                    ..
                }
                | Inst::StoreIndex {
                    kind: ContainerKind::Vec,
                    ..
                } => has_checked = true,
                _ => {}
            }
        }
    }
    if !has_checked {
        return 0;
    }
    let (preds, _) = predecessors(func, &rpo);
    let ctx = Ctx {
        ws,
        relevant: relevant_values(func, &rpo),
    };
    let n = func.blocks.len();
    let mut ins: Vec<Option<Facts>> = vec![None; n];
    let mut outs: Vec<Option<Facts>> = vec![None; n];
    ins[raw(func.entry)] = Some(Facts::default());
    loop {
        let mut changed = false;
        for &b in &rpo {
            if b != func.entry {
                let mut merged: Option<Facts> = None;
                if let Some(ps) = preds.get(&b) {
                    for &p in ps {
                        let Some(out) = &outs[raw(p)] else {
                            continue;
                        };
                        let edge = edge_facts(func, out, p, b);
                        merged = Some(match merged {
                            None => edge,
                            Some(mut m) => {
                                m.meet(&edge);
                                m
                            }
                        });
                    }
                }
                if merged == ins[raw(b)] && outs[raw(b)].is_some() {
                    continue;
                }
                if merged != ins[raw(b)] {
                    changed = true;
                    ins[raw(b)] = merged;
                }
            }
            let Some(input) = &ins[raw(b)] else {
                continue;
            };
            let mut facts = input.clone();
            for &id in &func.blocks[b].insts {
                step(&mut facts, func, &ctx, id);
            }
            if outs[raw(b)].as_ref() != Some(&facts) {
                changed = true;
                outs[raw(b)] = Some(facts);
            }
        }
        if !changed {
            break;
        }
    }
    let mut proven = Vec::new();
    for &b in &rpo {
        let Some(input) = &ins[raw(b)] else {
            continue;
        };
        let mut facts = input.clone();
        for &id in &func.blocks[b].insts {
            match func.insts[id] {
                Inst::Index {
                    base,
                    index,
                    kind: ContainerKind::Vec,
                }
                | Inst::StoreIndex {
                    base,
                    index,
                    kind: ContainerKind::Vec,
                    ..
                } if facts.in_bounds(func, index, base) => proven.push(id),
                _ => {}
            }
            step(&mut facts, func, &ctx, id);
        }
    }
    for &id in &proven {
        match &mut func.insts[id] {
            Inst::Index { kind, .. } | Inst::StoreIndex { kind, .. } => {
                *kind = ContainerKind::VecInBounds;
            }
            _ => unreachable!(),
        }
    }
    proven.len()
}

fn disabled_by_env() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        matches!(
            std::env::var("ROCK_SKULL_BOUNDS_OFF").ok().as_deref(),
            Some("1") | Some("true")
        )
    })
}

struct Ctx<'a> {
    ws: Option<&'a Workspace>,
    /// Values whose place tie is worth keeping: operands of the instructions
    /// that consume or produce facts, and of the arithmetic feeding them.
    relevant: Vec<bool>,
}

fn relevant_values(func: &MirFunc, rpo: &[Idx<Block>]) -> Vec<bool> {
    let mut relevant = vec![false; func.insts.len()];
    let mark = |ids: &[Idx<Inst>], relevant: &mut Vec<bool>| {
        for &id in ids {
            relevant[raw(id)] = true;
        }
    };
    for &b in rpo {
        for &id in &func.blocks[b].insts {
            match &func.insts[id] {
                Inst::Index { base, index, .. }
                | Inst::StoreIndex { base, index, .. }
                | Inst::VecCheckedIndex { vec: base, index } => {
                    mark(&[*base, *index], &mut relevant);
                }
                Inst::Len { base, .. } => mark(&[*base], &mut relevant),
                Inst::Field { base, field } if field.as_str() == LEN => {
                    mark(&[*base], &mut relevant);
                }
                Inst::Store { value, .. } => mark(&[*value], &mut relevant),
                _ => {}
            }
        }
        if let Term::Branch { cond, .. } = func.blocks[b].term {
            relevant[raw(cond)] = true;
        }
    }
    // Arithmetic and compares pass relevance to their operands; in reverse
    // postorder a use follows its definition, so one pass over the blocks in
    // reverse reaches every operand.
    for &b in rpo.iter().rev() {
        for &id in func.blocks[b].insts.iter().rev() {
            if !relevant[raw(id)] {
                continue;
            }
            match &func.insts[id] {
                Inst::IntAdd(a, b)
                | Inst::IntSub(a, b)
                | Inst::IntMul(a, b)
                | Inst::IntLt(a, b)
                | Inst::IntLeq(a, b)
                | Inst::IntGt(a, b)
                | Inst::IntGeq(a, b) => mark(&[*a, *b], &mut relevant),
                Inst::Not(a) => mark(&[*a], &mut relevant),
                _ => {}
            }
        }
    }
    relevant
}

/// The facts flowing along the edge `from -> to`: the block's out-state
/// sharpened by its branch condition.
fn edge_facts(func: &MirFunc, out: &Facts, from: Idx<Block>, to: Idx<Block>) -> Facts {
    match func.blocks[from].term {
        Term::Yield { .. } => Facts::default(),
        Term::Branch {
            cond,
            then_bb,
            else_bb,
        } if then_bb != else_bb => {
            let mut facts = out.clone();
            cond_facts(&mut facts, func, cond, to == then_bb);
            facts
        }
        _ => out.clone(),
    }
}

/// What a fact is about: the value an instruction produced, or a place.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Key {
    Value(Idx<Inst>),
    Cell(Idx<Cell>),
    Global(Ustr),
    Field(Box<Key>, Ustr),
    /// The element `base[index]` of a container.
    Elem(Box<Key>, Box<Key>),
}

impl Key {
    fn mentions(&self, pred: &impl Fn(&Key) -> bool) -> bool {
        pred(self)
            || match self {
                Key::Field(base, _) => base.mentions(pred),
                Key::Elem(base, index) => base.mentions(pred) || index.mentions(pred),
                _ => false,
            }
    }

    /// Reached through memory another body can write: a global, a field or
    /// an element.
    fn through_memory(&self) -> bool {
        self.mentions(&|k| matches!(k, Key::Global(_) | Key::Field(..) | Key::Elem(..)))
    }
}

/// The place `id` reads, when it reads one.
fn place_of(func: &MirFunc, id: Idx<Inst>) -> Option<Key> {
    let via = |x: Idx<Inst>| place_of(func, x).unwrap_or(Key::Value(x));
    match &func.insts[id] {
        Inst::Load(c) => Some(Key::Cell(*c)),
        Inst::Global(g) => Some(Key::Global(*g)),
        Inst::Field { base, field } => Some(Key::Field(Box::new(via(*base)), *field)),
        Inst::Index { base, index, .. } => {
            Some(Key::Elem(Box::new(via(*base)), Box::new(via(*index))))
        }
        _ => None,
    }
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Facts {
    /// `(value, place)`: the value is what the place holds now.
    same: HashSet<(Idx<Inst>, Key)>,
    nonneg: HashSet<Key>,
    /// `(k, v)`: the integer at `k` is at most the live length of `v`.
    le_len: HashSet<(Key, Key)>,
    /// `(k, v)`: the integer at `k` is below the live length of `v`.
    lt: HashSet<(Key, Key)>,
}

impl Facts {
    fn meet(&mut self, other: &Facts) {
        self.same.retain(|k| other.same.contains(k));
        self.nonneg.retain(|k| other.nonneg.contains(k));
        self.le_len.retain(|k| other.le_len.contains(k));
        self.lt.retain(|k| other.lt.contains(k));
    }

    fn is_empty(&self) -> bool {
        self.same.is_empty()
            && self.nonneg.is_empty()
            && self.le_len.is_empty()
            && self.lt.is_empty()
    }

    /// Drop every fact and tie whose keys mention a key `pred` accepts.
    fn forget(&mut self, pred: impl Fn(&Key) -> bool) {
        if self.is_empty() {
            return;
        }
        self.same
            .retain(|(v, p)| !pred(&Key::Value(*v)) && !p.mentions(&pred));
        self.nonneg.retain(|k| !k.mentions(&pred));
        self.le_len
            .retain(|(k, v)| !k.mentions(&pred) && !v.mentions(&pred));
        self.lt
            .retain(|(k, v)| !k.mentions(&pred) && !v.mentions(&pred));
    }

    fn forget_cell(&mut self, cell: Idx<Cell>) {
        self.forget(|k| *k == Key::Cell(cell));
    }

    fn forget_value(&mut self, id: Idx<Inst>) {
        self.forget(|k| *k == Key::Value(id));
    }

    fn forget_field(&mut self, field: Ustr) {
        self.forget(|k| matches!(k, Key::Field(_, f) if *f == field));
    }

    /// A vector may have shrunk.
    fn forget_lengths(&mut self) {
        self.le_len.clear();
        self.lt.clear();
    }

    /// Unknown code ran: any vector may have shrunk, any global, field or
    /// element may hold something else.
    fn after_unknown_call(&mut self) {
        self.forget_lengths();
        self.forget(|k| k.through_memory());
    }

    /// The value `id` and every place known to hold it.
    fn keys_of(&self, id: Idx<Inst>) -> Vec<Key> {
        let mut keys = vec![Key::Value(id)];
        keys.extend(
            self.same
                .iter()
                .filter(|(v, _)| *v == id)
                .map(|(_, p)| p.clone()),
        );
        keys
    }

    fn nonneg(&self, func: &MirFunc, id: Idx<Inst>) -> bool {
        // Only integer arithmetic over proven operands: a `len` field can be
        // written by source code, and the unsigned representation is not
        // vouched for here.
        match &func.insts[id] {
            Inst::Const(Lit::Int(n)) => *n >= 0,
            Inst::IntAdd(a, b) | Inst::IntMul(a, b) => {
                self.nonneg(func, *a) && self.nonneg(func, *b)
            }
            _ => self.keys_of(id).iter().any(|k| self.nonneg.contains(k)),
        }
    }

    /// The vectors whose live length bounds the integer `id` from above.
    fn le_len_of(&self, func: &MirFunc, id: Idx<Inst>) -> Vec<Key> {
        // `n - c` for a constant `c >= 0` is no larger than `n`.
        if let Inst::IntSub(a, b) = func.insts[id]
            && matches!(func.insts[b], Inst::Const(Lit::Int(c)) if c >= 0)
        {
            return self.le_len_of(func, a);
        }
        let keys = self.keys_of(id);
        self.le_len
            .iter()
            .filter(|(k, _)| keys.contains(k))
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn lt_of(&self, id: Idx<Inst>) -> Vec<Key> {
        let keys = self.keys_of(id);
        self.lt
            .iter()
            .filter(|(k, _)| keys.contains(k))
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn in_bounds(&self, func: &MirFunc, index: Idx<Inst>, vec: Idx<Inst>) -> bool {
        if !self.nonneg(func, index) {
            return false;
        }
        let below = self.lt_of(index);
        self.keys_of(vec).iter().any(|v| below.contains(v))
    }

    /// `v[k]` ran without raising, or was proven not to.
    fn checked_access(&mut self, vec: Idx<Inst>, index: Idx<Inst>) {
        let vecs = self.keys_of(vec);
        for k in self.keys_of(index) {
            self.nonneg.insert(k.clone());
            for v in &vecs {
                self.lt.insert((k.clone(), v.clone()));
            }
        }
    }
}

/// Sharpen `facts` with `cond == truth`.
fn cond_facts(facts: &mut Facts, func: &MirFunc, cond: Idx<Inst>, truth: bool) {
    match func.insts[cond] {
        Inst::Not(x) => cond_facts(facts, func, x, !truth),
        Inst::IntLt(a, b) | Inst::IntGt(b, a) => {
            if truth {
                lt_fact(facts, func, a, b);
            } else {
                leq_fact(facts, func, b, a);
            }
        }
        Inst::IntLeq(a, b) | Inst::IntGeq(b, a) => {
            if truth {
                leq_fact(facts, func, a, b);
            } else {
                lt_fact(facts, func, b, a);
            }
        }
        _ => {}
    }
}

/// `a < b` holds.
fn lt_fact(facts: &mut Facts, func: &MirFunc, a: Idx<Inst>, b: Idx<Inst>) {
    let bounds = facts.le_len_of(func, b);
    for k in facts.keys_of(a) {
        for v in &bounds {
            facts.lt.insert((k.clone(), v.clone()));
        }
    }
    // `-1 < b` or `0 <= a < b` makes `b` positive.
    let a_floor = match func.insts[a] {
        Inst::Const(Lit::Int(n)) => n >= -1,
        _ => facts.nonneg(func, a),
    };
    if a_floor {
        for k in facts.keys_of(b) {
            facts.nonneg.insert(k);
        }
    }
}

/// `a <= b` holds.
fn leq_fact(facts: &mut Facts, func: &MirFunc, a: Idx<Inst>, b: Idx<Inst>) {
    if facts.nonneg(func, a) {
        for k in facts.keys_of(b) {
            facts.nonneg.insert(k);
        }
    }
}

enum CallKind {
    /// The stdlib push: a vector only grew.
    Push,
    Unknown,
}

/// The stdlib push bodies, line by line: the only call a loop body may make
/// that provably leaves every proven index in bounds. The text is matched,
/// not the name, so a future push that reallocates or shrinks keeps the
/// checks until this list says otherwise.
const PUSH_BODY: [&str; 2] = ["vec.len = vec.len + 1", "vec._data[vec.len] = item"];

/// `@pure` is deliberately not trusted here: the checker admits calls to
/// `@debug_impure` callees, and `inline(lua)` and `extern` bodies are never
/// verified, so the annotation does not prove a callee leaves vectors alone.
fn classify_call(ctx: &Ctx<'_>, fref: FuncRef) -> CallKind {
    let Some(ws) = ctx.ws else {
        return CallKind::Unknown;
    };
    let module = ws.module(fref.module);
    let f = &module.functions[fref.func];
    if matches!(f.name.as_str(), "push" | "vec_push")
        && bone_core::modules::is_stdlib_module_file(module.file.as_str())
        && f.inline_body.as_ref().is_some_and(|(_, body)| {
            body.as_str()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .eq(PUSH_BODY)
        })
    {
        return CallKind::Push;
    }
    CallKind::Unknown
}

const LEN: &str = "len";

fn after_call(facts: &mut Facts, kind: CallKind) {
    match kind {
        CallKind::Push => facts.forget_field(Ustr::from(LEN)),
        CallKind::Unknown => facts.after_unknown_call(),
    }
}

/// Advance `facts` across `id`.
fn step(facts: &mut Facts, func: &MirFunc, ctx: &Ctx<'_>, id: Idx<Inst>) {
    let inst = &func.insts[id];
    if inst.produces_value() {
        facts.forget_value(id);
        if ctx.relevant[raw(id)]
            && let Some(place) = place_of(func, id)
        {
            facts.same.insert((id, place));
        }
    }
    match inst {
        Inst::Len { base, .. } => {
            for v in facts.keys_of(*base) {
                facts.le_len.insert((Key::Value(id), v));
            }
        }
        Inst::Field { base, field } if field.as_str() == LEN => {
            for v in facts.keys_of(*base) {
                facts.le_len.insert((Key::Value(id), v));
            }
        }
        Inst::Index {
            base,
            index,
            kind: ContainerKind::Vec | ContainerKind::VecInBounds,
        }
        | Inst::VecCheckedIndex { vec: base, index } => facts.checked_access(*base, *index),
        Inst::StoreIndex {
            base, index, kind, ..
        } => {
            facts.forget(|k| matches!(k, Key::Elem(..)));
            if matches!(kind, ContainerKind::Vec | ContainerKind::VecInBounds) {
                facts.checked_access(*base, *index);
            }
        }
        Inst::Store { cell, value } => {
            let nonneg = facts.nonneg(func, *value);
            let le_len = facts.le_len_of(func, *value);
            let lt = facts.lt_of(*value);
            facts.forget_cell(*cell);
            let k = Key::Cell(*cell);
            if nonneg {
                facts.nonneg.insert(k.clone());
            }
            for v in le_len {
                facts.le_len.insert((k.clone(), v));
            }
            for v in lt {
                facts.lt.insert((k.clone(), v));
            }
            if ctx.relevant[raw(*value)] {
                facts.same.insert((*value, k));
            }
        }
        Inst::StoreGlobal { name, .. } => facts.forget(|k| *k == Key::Global(*name)),
        Inst::StoreField { field, .. } => {
            if field.as_str() == LEN {
                facts.forget_lengths();
            }
            facts.forget_field(*field);
        }
        Inst::ArrayPush { .. } => facts.forget_field(Ustr::from(LEN)),
        Inst::Call { func: fref, .. } => after_call(facts, classify_call(ctx, *fref)),
        Inst::CallMulti {
            dests, func: fref, ..
        } => {
            after_call(facts, classify_call(ctx, *fref));
            for d in dests {
                facts.forget_cell(*d);
            }
        }
        Inst::CallValue { .. } => facts.after_unknown_call(),
        Inst::CallValueMulti { dests, .. } | Inst::CallBoundMulti { dests, .. } => {
            facts.after_unknown_call();
            for d in dests {
                facts.forget_cell(*d);
            }
        }
        // String conversion can reach a user `to_string`.
        Inst::Format { .. } | Inst::FormatDyn { .. } | Inst::ToString(_) => {
            facts.after_unknown_call()
        }
        Inst::MapNext {
            key_cell, val_cell, ..
        } => {
            facts.forget_cell(*key_cell);
            facts.forget_cell(*val_cell);
        }
        _ => {}
    }
}
