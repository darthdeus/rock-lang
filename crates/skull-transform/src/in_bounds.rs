//! Bounds-check elision for `[T]` element accesses: a forward dataflow over
//! in-bounds facts marks every `xs[i]` read or write whose index is provably in
//! `[0, xs.len)` as [`ContainerKind::VecInBounds`], which the backends index
//! raw, and replaces a proven column check ([`Inst::VecCheckedIndex`], the
//! one check shared by the leaf reads of a column-stored or strided element)
//! with the bare slot ([`Inst::VecIndexSlot`]). A `NativeBuffer` has the same
//! `len` and the same proof: a proven element access becomes
//! [`ContainerKind::NativeBufferInBounds`] and a proven `__nb_checked_index`
//! call becomes [`Inst::NbIndexSlot`]. A call whose callee declares
//! `@returns_index_below("<param>.<field>..")` or `@returns_nonneg` seeds the
//! matching fact about its result. Results and errors are unchanged: an access is only marked where the
//! check could not have raised.
//!
//! Three core facts are tracked about a *key*, which is either the value an
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
//! Constant-stride `row * stride + lane` also uses true-edge ordered
//! comparisons and whole-number certificates. A bounded query proves both
//! products and the sum exact in binary64, `row < rows`, `lane < stride`,
//! and `rows * stride <= len`. False comparison edges do not establish these
//! facts because they admit NaN. Arithmetic snapshot ties prevent expanding an
//! old result using operands whose instructions have since executed again.
//!
//! Facts die at the writes that could falsify them. A store to a cell drops
//! facts about that cell; a field store drops facts through that field name,
//! and a `len` store drops every length fact; a global store drops facts
//! through that global; an element store drops facts through any element. A
//! boxed Vec, Map, or heap-record lookup/store can enter a Lua metamethod, so it drops all
//! memory facts. A call drops every length fact and every fact through memory
//! (a global, a field or an element): the callee may shrink any vector it can
//! reach or rebind any field. `@pure` is not trusted, since the checker admits
//! `@debug_impure` callees and never verifies an `inline(lua)` or `extern`
//! body. A yield drops everything. An instruction that executes
//! again (a loop) drops the facts about its previous value.
//!
//! A fresh Lua boxed Vec also carries a local, must-private allocation identity.
//! Local copies preserve it; publishing any alias or exposing its backing
//! revokes the whole family. A private ArrayPush or recognized boxed stdlib
//! push cannot call a setter, so it invalidates current length/element places
//! without discarding captured bounds. Opaque calls still discard memory facts
//! even when they receive no private vector. No container contents are tracked.
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

use std::borrow::Cow;

use rustc_hash::FxHashSet as HashSet;

use bone_core::hir::FnBackend;
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
    elide_bounds_checks_for_target(ws, func, FnBackend::Any)
}

/// Add only allocation guarantees of the actual emission target. In particular,
/// fresh JS arrays can inherit setters; fresh Lua tables have no metatable.
pub fn elide_bounds_checks_for_target(
    ws: Option<&Workspace>,
    func: &mut MirFunc,
    target: FnBackend,
) -> usize {
    if disabled_by_env() {
        return 0;
    }
    let rpo = reverse_postorder(func, func.entry);
    let mut has_checked = false;
    let mut has_make_vec = false;
    let mut has_array_push = false;
    for &b in &rpo {
        for &i in &func.blocks[b].insts {
            match func.insts[i] {
                Inst::Lambda { .. } => return 0,
                Inst::MakeVec(_) => has_make_vec = true,
                Inst::ArrayPush { .. } => has_array_push = true,
                ref inst if needs_bounds_proof(ws, inst) => has_checked = true,
                _ => {}
            }
        }
    }
    if !has_checked {
        return 0;
    }
    let (preds, _) = predecessors(func, &rpo);
    let pushes: HashSet<_> = if target == FnBackend::Lua && has_make_vec {
        rpo.iter()
            .flat_map(|&b| func.blocks[b].insts.iter().copied())
            .filter(|&id| match &func.insts[id] {
                Inst::Call { func: fref, args } => args.len() == 2 && is_boxed_vec_push(ws, *fref),
                _ => false,
            })
            .collect()
    } else {
        HashSet::default()
    };
    let ctx = Ctx {
        ws,
        track_private: target == FnBackend::Lua
            && has_make_vec
            && (has_array_push || !pushes.is_empty()),
        relevant: relevant_values(func, &rpo),
        pushes,
        affine: rpo.iter().any(|&b| {
            func.blocks[b].insts.iter().any(|&id| {
                let index = match &func.insts[id] {
                    Inst::Index { index, .. }
                    | Inst::StoreIndex { index, .. }
                    | Inst::VecCheckedIndex { index, .. } => Some(*index),
                    Inst::Call { func: fref, args }
                        if args.len() == 2 && is_stdlib_fn(ws, *fref, "__nb_checked_index") =>
                    {
                        Some(args[1])
                    }
                    _ => None,
                };
                index.is_some_and(|i| affine_shape(func, i).is_some())
            })
        }),
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
                        let edge = edge_facts(func, out, p, b, ctx.affine);
                        merged = Some(match merged {
                            None => edge.into_owned(),
                            Some(mut m) => {
                                m.meet(edge.as_ref());
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
        let Some(last) = func.blocks[b]
            .insts
            .iter()
            .rposition(|&id| needs_bounds_proof(ws, &func.insts[id]))
        else {
            continue;
        };
        let Some(mut facts) = ins[raw(b)].take() else {
            continue;
        };
        // Solved inputs are independent: replay only as far as this block's
        // last proof query, then discard its final state.
        let last = func.blocks[b].insts[last];
        for &id in &func.blocks[b].insts {
            match &func.insts[id] {
                Inst::Index {
                    base,
                    index,
                    kind:
                        ContainerKind::Vec | ContainerKind::VecNonNull | ContainerKind::NativeBuffer,
                }
                | Inst::StoreIndex {
                    base,
                    index,
                    kind:
                        ContainerKind::Vec | ContainerKind::VecNonNull | ContainerKind::NativeBuffer,
                    ..
                }
                | Inst::VecCheckedIndex { vec: base, index }
                    if facts.in_bounds(func, *index, *base) =>
                {
                    proven.push(id)
                }
                Inst::Call { func: fref, args }
                    if is_stdlib_fn(ws, *fref, "__nb_checked_index")
                        && args.len() == 2
                        && facts.in_bounds(func, args[1], args[0]) =>
                {
                    proven.push(id)
                }
                _ => {}
            }
            if id == last {
                break;
            }
            step(&mut facts, func, &ctx, id);
        }
    }
    for &id in &proven {
        match &mut func.insts[id] {
            Inst::Index { kind, .. } | Inst::StoreIndex { kind, .. } => {
                *kind = match kind {
                    ContainerKind::Vec => ContainerKind::VecInBounds,
                    ContainerKind::VecNonNull => ContainerKind::VecNonNullInBounds,
                    _ => ContainerKind::NativeBufferInBounds,
                };
            }
            Inst::VecCheckedIndex { index, .. } => {
                let index = *index;
                func.insts[id] = Inst::VecIndexSlot(index);
            }
            Inst::Call { args, .. } => {
                let index = args[1];
                func.insts[id] = Inst::NbIndexSlot(index);
            }
            _ => unreachable!(),
        }
    }
    proven.len()
}

fn needs_bounds_proof(ws: Option<&Workspace>, inst: &Inst) -> bool {
    match inst {
        Inst::Index {
            kind: ContainerKind::Vec | ContainerKind::VecNonNull | ContainerKind::NativeBuffer,
            ..
        }
        | Inst::StoreIndex {
            kind: ContainerKind::Vec | ContainerKind::VecNonNull | ContainerKind::NativeBuffer,
            ..
        }
        | Inst::VecCheckedIndex { .. } => true,
        Inst::Call { func, .. } => is_stdlib_fn(ws, *func, "__nb_checked_index"),
        _ => false,
    }
}

/// Whether `fref` is the standard library's function `name`.
fn is_stdlib_fn(ws: Option<&Workspace>, fref: FuncRef, name: &str) -> bool {
    ws.is_some_and(|ws| {
        let module = ws.module(fref.module);
        module.functions[fref.func].name.as_str() == name
            && bone_core::modules::is_stdlib_module_file(module.file.as_str())
    })
}

// These two stdlib bodies only grow `len` and store into `_data`. The
// receiver's representation and privacy must still be proved at each call.
fn is_boxed_vec_push(ws: Option<&Workspace>, fref: FuncRef) -> bool {
    use bone_core::hir::{FnBackend, TyId, TypeCtor, TypeExpr};
    let Some(ws) = ws else {
        return false;
    };
    let module = ws.module(fref.module);
    let f = &module.functions[fref.func];
    if !bone_core::modules::is_stdlib_module_file(module.file.as_str())
        || !matches!(f.name.as_str(), "push" | "vec_push")
    {
        return false;
    }
    let Some((FnBackend::Lua, body)) = f.inline_body else {
        return false;
    };
    // Fail closed if the intrinsic gains callbacks or other effects later.
    if !body
        .as_str()
        .split_whitespace()
        .eq("vec.len = vec.len + 1 vec._data[vec.len] = item".split_whitespace())
    {
        return false;
    }
    let [receiver, item] = f.param_decls.as_slice() else {
        return false;
    };
    if receiver.name.as_str() != "vec"
        || item.name.as_str() != "item"
        || f.is_coroutine
        || f.is_variadic
    {
        return false;
    }
    let TyId::Static(mid, ty) = receiver.ty else {
        return false;
    };
    matches!(
        ws.module(mid).ty_arena[ty],
        TypeExpr::App {
            ctor: TypeCtor::Array,
            ..
        }
    )
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
    affine: bool,
    pushes: HashSet<Idx<Inst>>,
    track_private: bool,
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
                // A call's arguments and result may carry the facts its
                // callee declares, and the NativeBuffer index check is one.
                Inst::Call { args, .. } => {
                    mark(&[id], &mut relevant);
                    mark(args, &mut relevant);
                }
                // A length read is tied to its place so a later use of the
                // value (a modulo, a `len - 1`) can name the vector.
                Inst::Len { base, .. } => mark(&[id, *base], &mut relevant),
                Inst::Field { base, field } if field.as_str() == LEN => {
                    mark(&[id, *base], &mut relevant);
                }
                Inst::Store { value, .. } => mark(&[*value], &mut relevant),
                Inst::MakeStruct { fields, .. } => {
                    for (field, value) in fields {
                        if field.as_str() == LEN {
                            mark(&[*value], &mut relevant);
                        }
                    }
                }
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
                | Inst::IntMod(a, b)
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
fn edge_facts<'a>(
    func: &MirFunc,
    out: &'a Facts,
    from: Idx<Block>,
    to: Idx<Block>,
    affine: bool,
) -> Cow<'a, Facts> {
    match func.blocks[from].term {
        Term::Yield { .. } => Cow::Owned(Facts::default()),
        Term::Branch {
            cond,
            then_bb,
            else_bb,
        } if then_bb != else_bb => {
            let mut facts = out.clone();
            cond_facts(&mut facts, func, cond, to == then_bb, affine);
            Cow::Owned(facts)
        }
        _ => Cow::Borrowed(out),
    }
}

/// What a fact is about: the value an instruction produced, or a place.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Key {
    Value(Idx<Inst>),
    /// An integer literal, so two `Const` instructions of one value agree.
    Const(i64),
    Cell(Idx<Cell>),
    Scale(Box<Key>, i64),
    Sum(Box<Key>, Box<Key>),
    Product(Box<Key>, Box<Key>),
    Global(Ustr),
    Field(Box<Key>, Ustr),
    /// The element `base[index]` of a container.
    Elem(Box<Key>, Box<Key>),
}

impl Key {
    fn mentions(&self, pred: &impl Fn(&Key) -> bool) -> bool {
        pred(self)
            || match self {
                Key::Field(base, _) | Key::Scale(base, _) => base.mentions(pred),
                Key::Elem(base, index) | Key::Sum(base, index) | Key::Product(base, index) => {
                    base.mentions(pred) || index.mentions(pred)
                }
                _ => false,
            }
    }

    /// Reached through memory another body can write: a global, a field or
    /// an element.
    fn through_memory(&self) -> bool {
        self.mentions(&|k| matches!(k, Key::Global(_) | Key::Field(..) | Key::Elem(..)))
    }
}

/// The key of `id` as a value: a literal by its value, anything else by
/// its instruction.
fn value_key(func: &MirFunc, id: Idx<Inst>) -> Key {
    match func.insts[id] {
        Inst::Const(Lit::Int(n)) => Key::Const(n),
        _ => Key::Value(id),
    }
}

/// The place `id` reads, when it reads one.
fn place_of(func: &MirFunc, id: Idx<Inst>) -> Option<Key> {
    let via = |x: Idx<Inst>| place_of(func, x).unwrap_or_else(|| value_key(func, x));
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
    /// A value equals a current place or an arithmetic operand snapshot.
    same: HashSet<(Idx<Inst>, Key)>,
    nonneg: HashSet<Key>,
    /// `(k, v)`: the integer at `k` is at most the live length of `v`.
    le_len: HashSet<(Key, Key)>,
    /// `(k, v)`: the integer at `k` is below the live length of `v`.
    lt: HashSet<(Key, Key)>,
    // Whole-valued when finite; arithmetic may still produce NaN/infinity.
    whole: HashSet<Key>,
    // Ordered comparisons, seeded only by true edges (false admits NaN).
    ordered: HashSet<(Key, Key, bool)>,
    // A local reference to a fresh boxed Vec whose header/backing never escaped.
    // Allocation sites identify alias families; reexecution may conservatively
    // join several private instances of one site into the same family.
    private_vecs: HashSet<(Key, Idx<Inst>)>,
}

impl Facts {
    fn meet(&mut self, other: &Facts) {
        self.private_vecs.retain(|k| other.private_vecs.contains(k));
        self.whole.retain(|k| other.whole.contains(k));
        self.ordered.retain(|k| other.ordered.contains(k));
        self.same.retain(|k| other.same.contains(k));
        self.nonneg.retain(|k| other.nonneg.contains(k));
        self.le_len.retain(|k| other.le_len.contains(k));
        self.lt.retain(|k| other.lt.contains(k));
    }

    fn is_empty(&self) -> bool {
        self.private_vecs.is_empty()
            && self.whole.is_empty()
            && self.ordered.is_empty()
            && self.same.is_empty()
            && self.nonneg.is_empty()
            && self.le_len.is_empty()
            && self.lt.is_empty()
    }

    /// Drop every fact and tie whose keys mention a key `pred` accepts.
    fn forget(&mut self, pred: impl Fn(&Key) -> bool) {
        if self.is_empty() {
            return;
        }
        self.private_vecs.retain(|(k, _)| !k.mentions(&pred));
        self.whole.retain(|k| !k.mentions(&pred));
        self.ordered
            .retain(|(a, b, _)| !a.mentions(&pred) && !b.mentions(&pred));
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
        self.ordered.clear();
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
    fn keys_of(&self, func: &MirFunc, id: Idx<Inst>) -> Vec<Key> {
        let mut keys = vec![value_key(func, id)];
        keys.extend(
            self.same
                .iter()
                .filter(|(v, _)| *v == id)
                .map(|(_, p)| p.clone()),
        );
        keys
    }

    fn product_operand(&self, func: &MirFunc, id: Idx<Inst>) -> Key {
        if let Inst::Load(cell) = func.insts[id]
            && !func.cells[cell].borrowed
            && self.same.contains(&(id, Key::Cell(cell)))
        {
            return Key::Cell(cell);
        }
        value_key(func, id)
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
            // Floor modulo by a positive divisor lands in `[0, b)`; the
            // divisor is positive when it is a length proven above zero.
            Inst::IntMod(_, b) => !self.positive_lengths(func, *b).is_empty(),
            _ => self
                .keys_of(func, id)
                .iter()
                .any(|k| self.nonneg.contains(k)),
        }
    }

    fn private_root(&self, key: &Key) -> Option<Idx<Inst>> {
        self.private_vecs
            .iter()
            .find_map(|(k, root)| (k == key).then_some(*root))
    }

    fn track_private_vecs(&mut self, func: &MirFunc, ctx: &Ctx<'_>, id: Idx<Inst>) -> bool {
        if !ctx.track_private {
            return false;
        }
        let inst = &func.insts[id];
        if self.private_vecs.is_empty() {
            if matches!(inst, Inst::MakeVec(_)) {
                self.private_vecs.insert((Key::Value(id), id));
            }
            return false;
        }
        let push = match inst {
            Inst::ArrayPush {
                arr,
                kind: ContainerKind::Vec | ContainerKind::VecNonNull,
                ..
            } => Some(*arr),
            Inst::Call { args, .. } if ctx.pushes.contains(&id) => Some(args[0]),
            _ => None,
        };
        let safe_push = push.is_some_and(|arr| self.private_root(&value_key(func, arr)).is_some());
        let allowed = match inst {
            Inst::Store { cell, value } if !func.cells[*cell].borrowed => Some(*value),
            Inst::Field { base, field } if field.as_str() == LEN => Some(*base),
            Inst::Len { base, .. } | Inst::VecCheckedIndex { vec: base, .. } => Some(*base),
            Inst::Index {
                base,
                kind:
                    ContainerKind::Vec
                    | ContainerKind::VecNonNull
                    | ContainerKind::VecInBounds
                    | ContainerKind::VecNonNullInBounds,
                ..
            }
            | Inst::StoreIndex {
                base,
                kind:
                    ContainerKind::Vec
                    | ContainerKind::VecNonNull
                    | ContainerKind::VecInBounds
                    | ContainerKind::VecNonNullInBounds,
                ..
            } => Some(*base),
            _ => push,
        };
        // A receiver also used as the stored item escapes conservatively.
        // Inspect operand occurrences, not just membership in an allow-list.
        let mut allowed_uses = 0;
        inst.uses_each(&mut |op| {
            if Some(op) == allowed {
                allowed_uses += 1;
            }
        });
        let mut escaped = HashSet::default();
        inst.uses_each(&mut |op| {
            if (Some(op) != allowed || allowed_uses != 1)
                && let Some(root) = self.private_root(&value_key(func, op))
            {
                escaped.insert(root);
            }
        });
        self.private_vecs
            .retain(|(_, root)| !escaped.contains(root));
        match inst {
            Inst::MakeVec(_) => {
                self.private_vecs.insert((Key::Value(id), id));
            }
            Inst::Load(cell) => {
                if let Some(root) = self.private_root(&Key::Cell(*cell)) {
                    self.private_vecs.insert((Key::Value(id), root));
                }
            }
            _ => {}
        }
        safe_push
    }

    fn after_private_push(&mut self) {
        // The numeric length only grows, so captured upper bounds remain true.
        // Current length reads and element-derived facts no longer name memory.
        self.forget_field(Ustr::from(LEN));
        self.forget(|k| matches!(k, Key::Elem(..)));
    }

    fn whole_key(&self, key: &Key) -> bool {
        matches!(key, Key::Const(_)) || self.whole.contains(key)
    }

    fn whole_value(&self, func: &MirFunc, id: Idx<Inst>) -> bool {
        self.keys_of(func, id).iter().any(|k| self.whole_key(k))
    }

    fn ordered_fact(&mut self, func: &MirFunc, a: Idx<Inst>, b: Idx<Inst>, strict: bool) {
        let lengths = self.le_len_of(func, b);
        for ka in self.keys_of(func, a) {
            for kb in self.keys_of(func, b) {
                self.ordered.insert((ka.clone(), kb, strict));
            }
            if matches!(ka, Key::Scale(..)) {
                for v in &lengths {
                    self.le_len.insert((ka.clone(), v.clone()));
                }
            }
        }
    }

    fn aliases(&self, key: &Key) -> Vec<Key> {
        let mut keys = vec![key.clone()];
        if let Key::Value(id) = key {
            keys.extend(
                self.same
                    .iter()
                    .filter(|(v, _)| v == id)
                    .map(|(_, p)| p.clone()),
            );
        }
        keys
    }

    // A bounded search through explicit comparisons, not an iteratively growing
    // interval domain: loops cannot require one solver iteration per integer.
    fn upper(&self, key: &Key, depth: usize) -> Option<i64> {
        self.upper_with_budget(key, depth, &mut 64)
    }

    fn upper_with_budget(&self, key: &Key, depth: usize, budget: &mut usize) -> Option<i64> {
        if depth == 0 || *budget == 0 {
            return None;
        }
        *budget -= 1;
        if let Key::Const(n) = key {
            return ((-EXACT_INT..=EXACT_INT).contains(n)).then_some(*n);
        }
        let aliases = self.aliases(key);
        self.ordered
            .iter()
            .filter_map(|(a, b, strict)| {
                if !aliases.contains(a) {
                    return None;
                }
                let bound = self.upper_with_budget(b, depth - 1, budget)?;
                Some(bound - i64::from(*strict && self.whole_key(a) && self.whole_key(b)))
            })
            .min()
    }

    fn upper_value(&self, func: &MirFunc, id: Idx<Inst>) -> Option<i64> {
        self.keys_of(func, id)
            .iter()
            .filter_map(|k| self.upper(k, 8))
            .min()
    }

    fn known_nonneg(&self, func: &MirFunc, id: Idx<Inst>) -> bool {
        self.keys_of(func, id)
            .iter()
            .any(|k| matches!(k, Key::Const(n) if *n >= 0) || self.nonneg.contains(k))
    }

    fn affine_in_bounds(&self, func: &MirFunc, index: Idx<Inst>, vec: Idx<Inst>) -> bool {
        let Some((product, row, stride, lane)) = affine_shape(func, index) else {
            return false;
        };
        // MIR instruction ids can execute again. Only expand arithmetic while
        // its operand snapshots still describe the value that was computed.
        if !self.same.contains(&(
            index,
            Key::Sum(
                Box::new(value_key(func, product)),
                Box::new(value_key(func, lane)),
            ),
        )) || !self
            .same
            .contains(&(product, Key::Scale(Box::new(value_key(func, row)), stride)))
        {
            return false;
        }
        if !self.whole_value(func, row)
            || !self.whole_value(func, lane)
            || !self.known_nonneg(func, row)
            || !self.known_nonneg(func, lane)
        {
            return false;
        }
        let Some(lane_max) = self.upper_value(func, lane) else {
            return false;
        };
        if lane_max < 0 || lane_max >= stride {
            return false;
        }
        let Some(row_max) = self.upper_value(func, row) else {
            return false;
        };
        if row_max < 0
            || row_max
                .checked_mul(stride)
                .and_then(|n| n.checked_add(lane_max))
                .is_none_or(|n| n > EXACT_INT)
        {
            return false;
        }
        let rows = self.keys_of(func, row);
        let vectors = self.keys_of(func, vec);
        self.le_len.iter().any(|(limit, vector)| {
            let Key::Scale(count, scale) = limit else {
                return false;
            };
            if *scale != stride || !vectors.contains(vector) || !self.whole_key(count) {
                return false;
            }
            let Some(max) = self.upper(count, 8) else {
                return false;
            };
            let counts = self.aliases(count);
            max >= 0
                && max.checked_mul(stride).is_some_and(|n| n <= EXACT_INT)
                && self
                    .ordered
                    .iter()
                    .any(|(a, b, strict)| *strict && rows.contains(a) && counts.contains(b))
        })
    }

    /// The vectors whose live length bounds the integer `id` from above.
    fn le_len_of(&self, func: &MirFunc, id: Idx<Inst>) -> Vec<Key> {
        // `n - c` for a constant `c >= 0` is no larger than `n`.
        if let Inst::IntSub(a, b) = func.insts[id]
            && matches!(func.insts[b], Inst::Const(Lit::Int(c)) if c >= 0)
        {
            return self.le_len_of(func, a);
        }
        let keys = self.keys_of(func, id);
        self.le_len
            .iter()
            .filter(|(k, _)| keys.contains(k))
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// The vectors whose live length `id` is below, by a recorded fact or
    /// floor modulo with a positive divisor. Subtracting a positive integer
    /// from a length does not prove a strict bound: infinity and large
    /// floating-point integers can remain unchanged by the subtraction.
    fn lt_of(&self, func: &MirFunc, id: Idx<Inst>) -> Vec<Key> {
        let keys = self.keys_of(func, id);
        let mut out: Vec<Key> = self
            .lt
            .iter()
            .filter(|(k, _)| keys.contains(k))
            .map(|(_, v)| v.clone())
            .collect();
        match func.insts[id] {
            Inst::IntMod(_, b) => out.extend(self.positive_lengths(func, b)),
            _ => {}
        }
        out
    }

    /// The vectors whose live length the value `id` is: a `len` read of
    /// `v`, by the tie the read established.
    fn lengths_of(&self, func: &MirFunc, id: Idx<Inst>) -> Vec<Key> {
        self.keys_of(func, id)
            .into_iter()
            .filter_map(|k| match k {
                Key::Field(base, f) if f.as_str() == LEN => Some(*base),
                _ => None,
            })
            .collect()
    }

    /// [`Facts::lengths_of`], kept to the vectors proven non-empty
    /// (`0 < len` holds).
    fn positive_lengths(&self, func: &MirFunc, id: Idx<Inst>) -> Vec<Key> {
        self.lengths_of(func, id)
            .into_iter()
            .filter(|v| self.lt.contains(&(Key::Const(0), v.clone())))
            .collect()
    }

    fn in_bounds(&self, func: &MirFunc, index: Idx<Inst>, vec: Idx<Inst>) -> bool {
        if self.affine_in_bounds(func, index, vec) {
            return true;
        }
        let below = self.lt_of(func, index);
        if below.is_empty() {
            return false;
        }
        // `len - c` is nonnegative when `len > c - 1`; any index may also
        // be known nonnegative by an earlier access or guard.
        let by_length = match func.insts[index] {
            Inst::IntSub(a, b) if matches!(func.insts[b], Inst::Const(Lit::Int(c)) if c >= 1) => {
                let Inst::Const(Lit::Int(c)) = func.insts[b] else {
                    unreachable!()
                };
                self.lengths_of(func, a)
                    .iter()
                    .any(|v| self.lt.contains(&(Key::Const(c - 1), v.clone())))
            }
            _ => false,
        };
        if !by_length && !self.nonneg(func, index) {
            return false;
        }
        self.keys_of(func, vec).iter().any(|v| below.contains(v))
    }

    /// `v[k]` ran without raising, or was proven not to.
    fn checked_access(&mut self, func: &MirFunc, vec: Idx<Inst>, index: Idx<Inst>) {
        let vecs = self.keys_of(func, vec);
        for k in self.keys_of(func, index) {
            self.nonneg.insert(k.clone());
            for v in &vecs {
                self.lt.insert((k.clone(), v.clone()));
            }
        }
    }
}

/// Sharpen `facts` with `cond == truth`.
fn cond_facts(facts: &mut Facts, func: &MirFunc, cond: Idx<Inst>, truth: bool, affine: bool) {
    if truth && affine {
        match func.insts[cond] {
            Inst::IntLt(a, b) | Inst::IntGt(b, a) => facts.ordered_fact(func, a, b, true),
            Inst::IntLeq(a, b) | Inst::IntGeq(b, a) => facts.ordered_fact(func, a, b, false),
            _ => {}
        }
    }
    match func.insts[cond] {
        Inst::Not(x) => cond_facts(facts, func, x, !truth, affine),
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
    for k in facts.keys_of(func, a) {
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
        for k in facts.keys_of(func, b) {
            facts.nonneg.insert(k);
        }
    }
}

/// `a <= b` holds.
fn leq_fact(facts: &mut Facts, func: &MirFunc, a: Idx<Inst>, b: Idx<Inst>) {
    if facts.nonneg(func, a) {
        for k in facts.keys_of(func, b) {
            facts.nonneg.insert(k);
        }
    }
}

/// What a direct call may have written, from the whole-program effect table
/// (`bone_core::effects`): a callee that writes nothing keeps every fact, a
/// push drops only facts through `len`, a callee that may shrink a vector
/// drops the length facts, and field, global and element writes drop the
/// facts through those places. Without a workspace every call is unknown.
fn after_direct_call(facts: &mut Facts, ctx: &Ctx<'_>, fref: FuncRef) {
    let Some(ws) = ctx.ws else {
        facts.after_unknown_call();
        return;
    };
    let effect = ws.effects().of(fref.module, fref.func);
    if effect.is_all() {
        facts.after_unknown_call();
        return;
    }
    if effect.lengths {
        facts.forget_lengths();
    }
    if effect.globals {
        facts.forget(|k| matches!(k, Key::Global(_)));
    }
    if effect.elems {
        facts.forget(|k| matches!(k, Key::Elem(..)));
    }
    match &effect.fields {
        None => facts.forget(|k| matches!(k, Key::Field(..))),
        Some(fields) => {
            for &field in fields {
                facts.forget_field(field);
            }
        }
    }
}

const EXACT_INT: i64 = 1 << 53;

fn affine_shape(func: &MirFunc, id: Idx<Inst>) -> Option<(Idx<Inst>, Idx<Inst>, i64, Idx<Inst>)> {
    let Inst::IntAdd(product, lane) = func.insts[id] else {
        return None;
    };
    let (row, stride) = constant_scale(func, product)?;
    Some((product, row, stride, lane))
}

fn constant_scale(func: &MirFunc, id: Idx<Inst>) -> Option<(Idx<Inst>, i64)> {
    let Inst::IntMul(a, b) = func.insts[id] else {
        return None;
    };
    for (value, constant) in [(a, b), (b, a)] {
        if let Inst::Const(Lit::Int(n)) = func.insts[constant]
            && n > 0
            && n <= EXACT_INT
        {
            return Some((value, n));
        }
    }
    None
}

const LEN: &str = "len";

/// The facts a callee declares about its result: `@returns_nonneg`, and
/// `@returns_index_below("<param>[.field]*")`, which names a vector held by
/// an argument's place when the call returns. A promise about a body the
/// compiler cannot read, so it rides on the same review as the body.
fn seed_declared_result_facts(
    facts: &mut Facts,
    func: &MirFunc,
    ctx: &Ctx<'_>,
    call: Idx<Inst>,
    fref: FuncRef,
    args: &[Idx<Inst>],
) {
    let Some(ws) = ctx.ws else {
        return;
    };
    let f = &ws.module(fref.module).functions[fref.func];
    for anno in &f.annotations {
        if anno.is_builtin("returns_nonneg") {
            facts.nonneg.insert(Key::Value(call));
        }
        if anno.is_builtin("returns_index_below")
            && let Some(bone_core::hir::AnnoArg {
                value: bone_core::hir::AnnoValue::String(path),
                ..
            }) = anno.args.first()
        {
            let mut parts = path.as_str().split('.');
            let Some(param) = parts.next() else { continue };
            let Some(pos) = f.param_decls.iter().position(|p| p.name.as_str() == param) else {
                continue;
            };
            let Some(&arg) = args.get(pos) else { continue };
            let fields: Vec<Ustr> = parts.map(Ustr::from).collect();
            for mut key in facts.keys_of(func, arg) {
                for &field in &fields {
                    key = Key::Field(Box::new(key), field);
                }
                facts.lt.insert((Key::Value(call), key));
            }
        }
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
    if ctx.relevant[raw(id)]
        && let Inst::IntMul(a, b) = inst
    {
        // Identical operand snapshots repeat the same floating-point operation.
        // Do not reassociate products or recursively expand expression trees.
        facts.same.insert((
            id,
            Key::Product(
                Box::new(facts.product_operand(func, *a)),
                Box::new(facts.product_operand(func, *b)),
            ),
        ));
    }
    if ctx.affine && inst.produces_value() && ctx.relevant[raw(id)] {
        let whole = match inst {
            Inst::FloatToInt(_) => true,
            Inst::IntAdd(a, b) | Inst::IntSub(a, b) | Inst::IntMul(a, b) => {
                facts.whole_value(func, *a) && facts.whole_value(func, *b)
            }
            _ => facts.whole_value(func, id),
        };
        if whole {
            facts.whole.insert(value_key(func, id));
        }
        if let Some((value, scale)) = constant_scale(func, id) {
            facts
                .same
                .insert((id, Key::Scale(Box::new(value_key(func, value)), scale)));
        }
        if let Inst::IntAdd(a, b) = inst {
            facts.same.insert((
                id,
                Key::Sum(Box::new(value_key(func, *a)), Box::new(value_key(func, *b))),
            ));
        }
    }
    let private_push = facts.track_private_vecs(func, ctx, id);
    match inst {
        Inst::MakeStruct { fields, .. } => {
            // A fresh descriptor's length equals its initializer snapshot.
            if let Some((_, len)) = fields.iter().rev().find(|(field, _)| field.as_str() == LEN) {
                for key in facts.keys_of(func, *len) {
                    facts.le_len.insert((key, Key::Value(id)));
                }
            }
        }
        Inst::Len { base, .. } => {
            for v in facts.keys_of(func, *base) {
                facts.le_len.insert((Key::Value(id), v));
            }
        }
        Inst::Field { base, field } if field.as_str() == LEN => {
            for v in facts.keys_of(func, *base) {
                facts.le_len.insert((Key::Value(id), v));
            }
        }
        Inst::Index {
            kind:
                ContainerKind::Vec
                | ContainerKind::VecInBounds
                | ContainerKind::Map
                | ContainerKind::Record,
            ..
        } => facts.after_unknown_call(),
        Inst::Index {
            base,
            index,
            kind:
                ContainerKind::VecNonNull
                | ContainerKind::VecNonNullInBounds
                | ContainerKind::NativeBuffer
                | ContainerKind::NativeBufferInBounds,
        }
        | Inst::VecCheckedIndex { vec: base, index } => facts.checked_access(func, *base, *index),
        Inst::StoreIndex {
            base, index, kind, ..
        } => {
            if matches!(
                kind,
                ContainerKind::Vec
                    | ContainerKind::VecInBounds
                    | ContainerKind::Map
                    | ContainerKind::Record
            ) {
                facts.after_unknown_call();
            } else {
                facts.forget(|k| matches!(k, Key::Elem(..)));
            }
            if matches!(
                kind,
                ContainerKind::VecNonNull
                    | ContainerKind::VecNonNullInBounds
                    | ContainerKind::NativeBuffer
                    | ContainerKind::NativeBufferInBounds
            ) {
                facts.checked_access(func, *base, *index);
            }
        }
        Inst::Store { cell, value } => {
            // Copy bounds owned by the descriptor, not just bounds on its value.
            let owners = facts.keys_of(func, *value);
            let capacities: Vec<_> = facts
                .le_len
                .iter()
                .filter(|(bound, owner)| {
                    owners.contains(owner) && !bound.mentions(&|k| *k == Key::Cell(*cell))
                })
                .map(|(bound, _)| bound.clone())
                .collect();
            let private = facts.private_root(&value_key(func, *value));
            let whole = ctx.affine && facts.whole_value(func, *value);
            let nonneg = facts.nonneg(func, *value);
            let le_len = facts.le_len_of(func, *value);
            let lt = facts.lt_of(func, *value);
            facts.forget_cell(*cell);
            let k = Key::Cell(*cell);
            for bound in capacities {
                facts.le_len.insert((bound, k.clone()));
            }
            if let Some(root) = private {
                facts.private_vecs.insert((k.clone(), root));
            }
            if whole {
                facts.whole.insert(k.clone());
            }
            if ctx.affine
                && let Inst::Const(Lit::Int(n)) = func.insts[*value]
            {
                facts.ordered.insert((k.clone(), Key::Const(n), false));
            }
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
        // A foreign backing can enter __newindex while appending an absent slot.
        Inst::ArrayPush { .. } => {
            if private_push {
                facts.after_private_push();
            } else {
                facts.after_unknown_call();
            }
        }
        Inst::Call { func: fref, args } => {
            if private_push {
                facts.after_private_push();
            } else {
                after_direct_call(facts, ctx, *fref);
            }
            if is_stdlib_fn(ctx.ws, *fref, "__nb_checked_index") && args.len() == 2 {
                facts.checked_access(func, args[0], args[1]);
            }
            seed_declared_result_facts(facts, func, ctx, id, *fref, args);
        }
        Inst::CallMulti {
            dests, func: fref, ..
        } => {
            after_direct_call(facts, ctx, *fref);
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
