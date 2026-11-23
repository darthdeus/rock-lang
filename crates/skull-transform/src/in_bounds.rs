//! Prove vector and native-buffer bounds with forward dataflow.
//!
//! Keys identify produced values and storage locations. A load ties its value
//! to its current place until a write invalidates the tie. Joins intersect
//! facts; a yield drops them. Unknown calls and Lua metamethods can invalidate
//! memory and length facts.
//!
//! For affine indexes, a true ordered edge gives integer bounds only when
//! whole operands and exact arithmetic can be established. Snapshot ties
//! prevent using a reexecuted operand to justify an older product.
//!
//! Fresh Lua tables are private until any alias escapes. Reads of private
//! records and recognized pushes cannot invoke metamethods.

use std::borrow::Cow;
use std::cell::{Cell as LocalCell, RefCell};
use std::ops::Deref;

use rustc_hash::{FxHashMap, FxHashSet as HashSet};
use smallvec::{SmallVec, smallvec};

use bone_core::hir::FnBackend;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::cfg::{predecessors, reverse_postorder, successors};
use skull_mir::lower::CDATA_LEAF_PREFIX;
use skull_mir::mir::*;

use crate::visit::raw;
use ustr::Ustr;

/// Use the allocation guarantees of the selected backend.
pub fn elide_bounds_checks_for_target(
    ws: Option<&Workspace>,
    func: &mut MirFunc,
    target: FnBackend,
) -> usize {
    if skull_mir::pass_disabled("bounds") {
        return 0;
    }
    let proven = proven_accesses(ws, func, target);
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

fn proven_accesses(ws: Option<&Workspace>, func: &MirFunc, target: FnBackend) -> Vec<Idx<Inst>> {
    let mut rpo = reverse_postorder(func, func.entry);
    let mut queries = Vec::new();
    let mut has_make_vec = false;
    let mut has_array_push = false;
    let mut has_make_struct = false;
    let mut has_record_read = false;
    for &b in &rpo {
        let mut last = None;
        for &i in &func.blocks[b].insts {
            match func.insts[i] {
                Inst::Lambda { .. } => return Vec::new(),
                Inst::MakeVec(_) => has_make_vec = true,
                Inst::MakeStruct { .. } => has_make_struct = true,
                Inst::Index {
                    kind: ContainerKind::Record(_),
                    ..
                } => has_record_read = true,
                Inst::ArrayPush { .. } => has_array_push = true,
                ref inst if needs_bounds_proof(ws, inst) => last = Some(i),
                _ => {}
            }
        }
        if let Some(last) = last {
            queries.push((b, last));
        }
    }
    if queries.is_empty() {
        return Vec::new();
    }
    let (preds, _) = predecessors(func, &rpo);
    let reexecuting = reexecuting_blocks(func, &rpo);
    let ctx = Ctx::new(ws, func, &rpo, target == FnBackend::Lua && has_make_vec);
    let ctx = Ctx {
        track_private: target == FnBackend::Lua
            && ((has_make_vec && (has_array_push || !ctx.pushes.is_empty()))
                || (has_make_struct && has_record_read)),
        ..ctx
    };
    // A block that cannot reach a proof query cannot affect any proof. Keep
    // relevance and reexecution based on the full CFG above.
    let mut needed = vec![false; func.blocks.len()];
    let mut pending: Vec<_> = queries.iter().map(|&(b, _)| b).collect();
    while let Some(b) = pending.pop() {
        for &p in preds.get(&b).into_iter().flatten() {
            if !std::mem::replace(&mut needed[raw(p)], true) {
                pending.push(p);
            }
        }
    }
    rpo.retain(|&b| needed[raw(b)]);
    let body = Body::new(func);
    let func = &body;
    let mut outs: Vec<Option<Facts>> = vec![None; func.blocks.len()];
    let mut dirty = vec![true; func.blocks.len()];
    let input = |outs: &[Option<Facts>], b| {
        if b == func.entry {
            return Some(Facts::default());
        }
        let mut inputs = preds.get(&b).into_iter().flatten().filter_map(|&p| {
            let out = outs[raw(p)].as_ref()?;
            Some(edge_facts(func, out, p, b, ctx.affine))
        });
        let mut merged = inputs.next()?.into_owned();
        for edge in inputs {
            merged.meet(edge.as_ref());
        }
        Some(merged)
    };
    loop {
        let mut changed = false;
        for &b in &rpo {
            if !std::mem::replace(&mut dirty[raw(b)], false) {
                continue;
            }
            let Some(mut facts) = input(&outs, b) else {
                continue;
            };
            for &id in &func.blocks[b].insts {
                step(&mut facts, func, &ctx, id, reexecuting[raw(b)]);
            }
            if outs[raw(b)].as_ref() != Some(&facts) {
                changed = true;
                outs[raw(b)] = Some(facts);
                successors(&func.blocks[b].term, |s| dirty[raw(s)] = true);
            }
        }
        if !changed {
            break;
        }
    }
    let mut proven = Vec::new();
    for &(b, last) in &queries {
        let Some(mut facts) = input(&outs, b) else {
            continue;
        };
        // Solved inputs are independent: replay only as far as this block's
        // last proof query, then discard its final state.
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
            step(&mut facts, func, &ctx, id, reexecuting[raw(b)]);
        }
    }
    proven
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

fn is_stdlib_fn(ws: Option<&Workspace>, fref: FuncRef, name: &str) -> bool {
    ws.is_some_and(|ws| {
        let module = ws.module(fref.module);
        module.functions[fref.func].name.as_str() == name
            && bone_core::modules::is_stdlib_module_file(module.file.as_str())
    })
}

// These two stdlib bodies only grow `len` and store the element. The
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
        .eq("vec[0] = vec[0] + 1 vec[vec[0]] = item".split_whitespace())
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

/// Every cyclic block lies within a backward-edge interval in RPO.
fn reexecuting_blocks(func: &MirFunc, rpo: &[Idx<Block>]) -> Vec<bool> {
    let mut order = vec![usize::MAX; func.blocks.len()];
    for (at, &b) in rpo.iter().enumerate() {
        order[raw(b)] = at;
    }
    let mut reexecuting = vec![false; func.blocks.len()];
    let mut first = usize::MAX;
    for (at, &b) in rpo.iter().enumerate().rev() {
        successors(&func.blocks[b].term, |s| {
            if order[raw(s)] <= at {
                first = first.min(order[raw(s)]);
            }
        });
        reexecuting[raw(b)] = first <= at;
    }
    reexecuting
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

impl<'a> Ctx<'a> {
    fn new(
        ws: Option<&'a Workspace>,
        func: &MirFunc,
        rpo: &[Idx<Block>],
        track_pushes: bool,
    ) -> Self {
        let mut affine = false;
        let mut pushes = HashSet::default();
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
                        affine |= !affine && affine_shape(func, *index).is_some();
                    }
                    // A call's arguments and result may carry the facts its
                    // callee declares, and the NativeBuffer index check is one.
                    Inst::Call { func: callee, args } => {
                        relevant[raw(id)] = true;
                        mark(args, &mut relevant);
                        if let [_, index] = args.as_slice() {
                            if !affine && is_stdlib_fn(ws, *callee, "__nb_checked_index") {
                                affine = affine_shape(func, *index).is_some();
                            }
                            if track_pushes && is_boxed_vec_push(ws, *callee) {
                                pushes.insert(id);
                            }
                        }
                    }
                    // A length read is tied to its place so a later use of the
                    // value (a modulo, a `len - 1`) can name the vector.
                    Inst::Len { base, .. } => mark(&[id, *base], &mut relevant),
                    Inst::Field { base, field, .. } if is_length(*field) => {
                        mark(&[id, *base], &mut relevant);
                    }
                    Inst::Store { value, .. } => mark(&[*value], &mut relevant),
                    Inst::MakeStruct { fields, .. } => {
                        for (field, value) in fields {
                            if is_length(*field) {
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
        // Propagate relevance backward through arithmetic and comparisons.
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
                    | Inst::IntGeq(a, b)
                    | Inst::Eq(a, b)
                    | Inst::NotEq(a, b) => mark(&[*a, *b], &mut relevant),
                    Inst::Not(a) => mark(&[*a], &mut relevant),
                    _ => {}
                }
            }
        }
        Self {
            ws,
            relevant,
            affine,
            pushes,
            track_private: false,
        }
    }
}

fn edge_facts<'a>(
    func: &Body<'_>,
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
/// Interned per body, so facts copy, compare and hash keys as integers.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
struct Key(u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum KeyData {
    Value(Idx<Inst>),
    /// An integer literal, so two `Const` instructions of one value agree.
    Const(i64),
    Cell(Idx<Cell>),
    Scale(Key, i64),
    Sum(Key, Key),
    Product(Key, Key),
    Global(Ustr),
    Field(Key, Ustr),
    /// The element `base[index]` of a container.
    Elem(Key, Key),
}

/// The body under analysis and the keys interned for it. Value and cell keys
/// are numbered by their index; other keys are hash-consed after them.
struct Body<'a> {
    func: &'a MirFunc,
    keys: RefCell<Vec<Interned>>,
    ids: RefCell<FxHashMap<KeyData, Key>>,
    cdata_keys: LocalCell<bool>,
}

#[derive(Clone, Copy)]
struct Interned {
    data: KeyData,
    /// Covers every key this one mentions, itself included.
    mentions: Mentions,
}

/// A cheap conservative filter for keys affected by a write.
#[derive(Clone, Copy, Default)]
struct Mentions {
    values: u64,
    cells: u64,
    kinds: u8,
}

const GLOBAL: u8 = 1;
const FIELD: u8 = 2;
const ELEM: u8 = 4;

impl Mentions {
    fn value(id: Idx<Inst>) -> Self {
        Mentions {
            values: 1 << (raw(id) % 64),
            ..Mentions::default()
        }
    }

    fn cell(c: Idx<Cell>) -> Self {
        Mentions {
            cells: 1 << (raw(c) % 64),
            ..Mentions::default()
        }
    }

    fn kinds(kinds: u8) -> Self {
        Mentions {
            kinds,
            ..Mentions::default()
        }
    }

    fn union(self, other: Mentions) -> Self {
        Mentions {
            values: self.values | other.values,
            cells: self.cells | other.cells,
            kinds: self.kinds | other.kinds,
        }
    }

    fn meets(self, other: Mentions) -> bool {
        self.values & other.values != 0
            || self.cells & other.cells != 0
            || self.kinds & other.kinds != 0
    }
}

impl Deref for Body<'_> {
    type Target = MirFunc;

    fn deref(&self) -> &MirFunc {
        self.func
    }
}

impl<'a> Body<'a> {
    fn new(func: &'a MirFunc) -> Self {
        let values = func.insts.iter().map(|(id, _)| Interned {
            data: KeyData::Value(id),
            mentions: Mentions::value(id),
        });
        let cells = func.cells.iter().map(|(c, _)| Interned {
            data: KeyData::Cell(c),
            mentions: Mentions::cell(c),
        });
        Body {
            func,
            keys: RefCell::new(values.chain(cells).collect()),
            ids: RefCell::default(),
            cdata_keys: LocalCell::new(false),
        }
    }

    fn key(&self, data: KeyData) -> Key {
        match data {
            KeyData::Value(id) => Key(raw(id) as u32),
            KeyData::Cell(c) => Key((self.func.insts.len() + raw(c)) as u32),
            _ => *self.ids.borrow_mut().entry(data).or_insert_with(|| {
                let mut keys = self.keys.borrow_mut();
                let of = |k: Key| keys[k.0 as usize].mentions;
                let mentions = match data {
                    KeyData::Global(_) => Mentions::kinds(GLOBAL),
                    KeyData::Field(base, field) => {
                        if !self.cdata_keys.get() && field.as_str().starts_with(CDATA_LEAF_PREFIX) {
                            self.cdata_keys.set(true);
                        }
                        of(base).union(Mentions::kinds(FIELD))
                    }
                    KeyData::Elem(a, b) => of(a).union(of(b)).union(Mentions::kinds(ELEM)),
                    KeyData::Scale(base, _) => of(base),
                    KeyData::Sum(a, b) | KeyData::Product(a, b) => of(a).union(of(b)),
                    _ => Mentions::default(),
                };
                keys.push(Interned { data, mentions });
                Key((keys.len() - 1) as u32)
            }),
        }
    }

    fn data(&self, key: Key) -> KeyData {
        self.keys.borrow()[key.0 as usize].data
    }

    fn mentions(&self, key: Key, pred: &impl Fn(KeyData) -> bool) -> bool {
        mentions(&self.keys.borrow(), key, pred)
    }
}

fn mentions(keys: &[Interned], key: Key, pred: &impl Fn(KeyData) -> bool) -> bool {
    let data = keys[key.0 as usize].data;
    pred(data)
        || match data {
            KeyData::Field(base, _) | KeyData::Scale(base, _) => mentions(keys, base, pred),
            KeyData::Elem(base, index)
            | KeyData::Sum(base, index)
            | KeyData::Product(base, index) => {
                mentions(keys, base, pred) || mentions(keys, index, pred)
            }
            _ => false,
        }
}

/// Reached through memory another body can write: a global, a field or an
/// element.
fn through_memory(key: KeyData) -> bool {
    matches!(
        key,
        KeyData::Global(_) | KeyData::Field(..) | KeyData::Elem(..)
    )
}

fn value_key(func: &Body<'_>, id: Idx<Inst>) -> Key {
    match func.insts[id] {
        Inst::Const(Lit::Int(n)) => func.key(KeyData::Const(n)),
        _ => func.key(KeyData::Value(id)),
    }
}

fn cdata_path_mentions(path: &str, field: &str) -> bool {
    let hex = b"0123456789abcdef";
    let field = field.as_bytes();
    let mut offset = 0;
    let mut equal = true;
    for pair in path.trim_start_matches('_').as_bytes().chunks_exact(2) {
        if pair == b"2e" {
            if equal && offset == field.len() {
                return true;
            }
            offset = 0;
            equal = true;
        } else {
            equal &= field
                .get(offset)
                .is_some_and(|byte| pair == [hex[(byte >> 4) as usize], hex[(byte & 15) as usize]]);
            offset += 1;
        }
    }
    equal && offset == field.len()
}

fn place_of(func: &Body<'_>, id: Idx<Inst>) -> Option<Key> {
    let via = |x: Idx<Inst>| place_of(func, x).unwrap_or_else(|| value_key(func, x));
    let data = match &func.insts[id] {
        Inst::Load(c) => KeyData::Cell(*c),
        Inst::Global(g) => KeyData::Global(*g),
        Inst::Field { base, field, .. } => KeyData::Field(via(*base), *field),
        Inst::Index { base, index, .. } => KeyData::Elem(via(*base), via(*index)),
        _ => return None,
    };
    Some(func.key(data))
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Facts {
    /// Value ties grouped by instruction so a query visits only that value's places.
    same: SmallVec<[(Idx<Inst>, Key); 4]>,
    nonneg: HashSet<Key>,
    /// `(k, v)`: the integer at `k` is at most the live length of `v`.
    le_len: HashSet<(Key, Key)>,
    /// `(k, v)`: the integer at `k` is below the live length of `v`.
    lt: HashSet<(Key, Key)>,
    // Whole-valued when finite; arithmetic may still produce NaN/infinity.
    whole: HashSet<Key>,
    // Ordered comparisons, seeded only by true edges (false admits NaN).
    ordered: HashSet<(Key, Key, bool)>,
    // One allocation site may represent several private objects in a loop.
    private_tables: HashSet<(Key, Idx<Inst>)>,
}

impl Facts {
    fn meet(&mut self, other: &Facts) {
        self.private_tables
            .retain(|k| other.private_tables.contains(k));
        self.whole.retain(|k| other.whole.contains(k));
        self.ordered.retain(|k| other.ordered.contains(k));
        self.same.retain(|k| other.same.binary_search(k).is_ok());
        self.nonneg.retain(|k| other.nonneg.contains(k));
        self.le_len.retain(|k| other.le_len.contains(k));
        self.lt.retain(|k| other.lt.contains(k));
    }

    fn is_empty(&self) -> bool {
        self.private_tables.is_empty()
            && self.whole.is_empty()
            && self.ordered.is_empty()
            && self.same.is_empty()
            && self.nonneg.is_empty()
            && self.le_len.is_empty()
            && self.lt.is_empty()
    }

    /// Drop every fact and tie whose keys mention a key `pred` accepts.
    /// `mask` meets the summary of every key `pred` accepts.
    fn forget(&mut self, func: &Body<'_>, mask: Mentions, pred: impl Fn(KeyData) -> bool) {
        if self.is_empty() {
            return;
        }
        let keys = func.keys.borrow();
        let m = |k: &Key| keys[k.0 as usize].mentions.meets(mask) && mentions(&keys, *k, &pred);
        self.private_tables.retain(|(k, _)| !m(k));
        self.whole.retain(|k| !m(k));
        self.ordered.retain(|(a, b, _)| !m(a) && !m(b));
        self.same
            .retain(|(v, p)| !pred(KeyData::Value(*v)) && !m(p));
        self.nonneg.retain(|k| !m(k));
        self.le_len.retain(|(k, v)| !m(k) && !m(v));
        self.lt.retain(|(k, v)| !m(k) && !m(v));
    }

    fn forget_cell(&mut self, func: &Body<'_>, cell: Idx<Cell>) {
        self.forget(func, Mentions::cell(cell), |k| k == KeyData::Cell(cell));
    }

    /// Use the summaries to skip invalidation when no fact mentions this value.
    fn forget_value(&mut self, func: &Body<'_>, id: Idx<Inst>) {
        let mask = Mentions::value(id);
        let found = {
            let keys = func.keys.borrow();
            let has = |k: &Key| keys[k.0 as usize].mentions.meets(mask);
            !self.ties(id).is_empty()
                || self.same.iter().any(|(_, p)| has(p))
                || self.private_tables.iter().any(|(k, _)| has(k))
                || self.whole.iter().any(has)
                || self.nonneg.iter().any(has)
                || self.ordered.iter().any(|(a, b, _)| has(a) || has(b))
                || self.le_len.iter().any(|(k, v)| has(k) || has(v))
                || self.lt.iter().any(|(k, v)| has(k) || has(v))
        };
        if found {
            self.forget(func, mask, |k| k == KeyData::Value(id));
        }
    }

    fn forget_field(&mut self, func: &Body<'_>, field: Ustr) {
        let read_aliases = func.cdata_keys.get();
        let name = field.as_str();
        let written_path = name.strip_prefix(CDATA_LEAF_PREFIX);
        let fields = Mentions::kinds(FIELD);
        if !read_aliases && written_path.is_none() {
            self.forget(
                func,
                fields,
                |k| matches!(k, KeyData::Field(_, f) if f == field),
            );
            return;
        }
        self.forget(func, fields, |k| match k {
            KeyData::Field(_, f) => {
                f == field
                    || (read_aliases
                        && f.as_str()
                            .strip_prefix(CDATA_LEAF_PREFIX)
                            .is_some_and(|path| cdata_path_mentions(path, name)))
                    || written_path.is_some_and(|path| cdata_path_mentions(path, f.as_str()))
            }
            _ => false,
        });
    }

    fn forget_elems(&mut self, func: &Body<'_>) {
        self.forget(func, Mentions::kinds(ELEM), |k| {
            matches!(k, KeyData::Elem(..))
        });
    }

    /// A vector may have shrunk.
    fn forget_lengths(&mut self) {
        self.ordered.clear();
        self.le_len.clear();
        self.lt.clear();
    }

    /// Unknown code ran: any vector may have shrunk, any global, field or
    /// element may hold something else.
    fn after_unknown_call(&mut self, func: &Body<'_>) {
        self.forget_lengths();
        self.forget(func, Mentions::kinds(GLOBAL | FIELD | ELEM), through_memory);
    }

    fn ties(&self, id: Idx<Inst>) -> &[(Idx<Inst>, Key)] {
        let start = self.same.partition_point(|&(value, _)| value < id);
        let rest = &self.same[start..];
        &rest[..rest.partition_point(|&(value, _)| value == id)]
    }

    fn tie(&mut self, value: Idx<Inst>, key: Key) {
        let row = (value, key);
        if let Err(at) = self.same.binary_search(&row) {
            self.same.insert(at, row);
        }
    }

    /// The value `id` and every place known to hold it.
    fn keys_of(&self, func: &Body<'_>, id: Idx<Inst>) -> SmallVec<[Key; 4]> {
        let mut keys = smallvec![value_key(func, id)];
        keys.extend(self.ties(id).iter().map(|&(_, key)| key));
        keys
    }

    fn product_operand(&self, func: &Body<'_>, id: Idx<Inst>) -> Key {
        if let Inst::Load(cell) = func.insts[id]
            && !func.cells[cell].borrowed
        {
            let cell = func.key(KeyData::Cell(cell));
            if self.same.binary_search(&(id, cell)).is_ok() {
                return cell;
            }
        }
        value_key(func, id)
    }

    fn nonneg(&self, func: &Body<'_>, id: Idx<Inst>) -> bool {
        // A user-written length field need not be a valid length.
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
        self.private_tables
            .iter()
            .find_map(|(k, root)| (k == key).then_some(*root))
    }

    fn track_private_tables(
        &mut self,
        func: &Body<'_>,
        ctx: &Ctx<'_>,
        id: Idx<Inst>,
    ) -> (bool, bool) {
        if !ctx.track_private {
            return (false, false);
        }
        let inst = &func.insts[id];
        if self.private_tables.is_empty() {
            if matches!(inst, Inst::MakeVec(_) | Inst::MakeStruct { .. }) {
                self.private_tables
                    .insert((func.key(KeyData::Value(id)), id));
            }
            return (false, false);
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
        let safe_push = push.is_some_and(|arr| {
            self.private_root(&value_key(func, arr))
                .is_some_and(|root| matches!(func.insts[root], Inst::MakeVec(_)))
        });
        let record = match inst {
            Inst::Index {
                base,
                kind: ContainerKind::Record(_),
                ..
            } if self
                .private_root(&value_key(func, *base))
                .is_some_and(|root| matches!(func.insts[root], Inst::MakeStruct { .. })) =>
            {
                Some(*base)
            }
            _ => None,
        };
        let allowed = match inst {
            Inst::Store { cell, value } if !func.cells[*cell].borrowed => Some(*value),
            Inst::Field { base, field, .. } if is_length(*field) => Some(*base),
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
            _ => record.or(push),
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
        self.private_tables
            .retain(|(_, root)| !escaped.contains(root));
        match inst {
            Inst::MakeVec(_) | Inst::MakeStruct { .. } => {
                self.private_tables
                    .insert((func.key(KeyData::Value(id)), id));
            }
            Inst::Load(cell) => {
                if let Some(root) = self.private_root(&func.key(KeyData::Cell(*cell))) {
                    self.private_tables
                        .insert((func.key(KeyData::Value(id)), root));
                }
            }
            _ => {}
        }
        (safe_push, record.is_some())
    }

    fn after_private_push(&mut self, func: &Body<'_>) {
        // The numeric length only grows, so captured upper bounds remain true.
        // Current length reads and element-derived facts no longer name memory.
        self.forget_field(func, Ustr::from("len"));
        self.forget_field(func, Ustr::from(skull_mir::mir::VEC_LENGTH_SLOT));
        self.forget_elems(func);
    }

    fn whole_key(&self, func: &Body<'_>, key: &Key) -> bool {
        matches!(func.data(*key), KeyData::Const(_)) || self.whole.contains(key)
    }

    fn whole_value(&self, func: &Body<'_>, id: Idx<Inst>) -> bool {
        self.keys_of(func, id)
            .iter()
            .any(|k| self.whole_key(func, k))
    }

    fn ordered_fact(&mut self, func: &Body<'_>, a: Idx<Inst>, b: Idx<Inst>, strict: bool) {
        let lengths = self.le_len_of(func, b);
        let right = self.keys_of(func, b);
        for ka in self.keys_of(func, a) {
            for &kb in &right {
                self.ordered.insert((ka, kb, strict));
            }
            if matches!(func.data(ka), KeyData::Scale(..)) {
                for &v in &lengths {
                    self.le_len.insert((ka, v));
                }
            }
        }
    }

    fn aliases(&self, func: &Body<'_>, key: &Key) -> SmallVec<[Key; 4]> {
        let mut keys = smallvec![*key];
        if let KeyData::Value(id) = func.data(*key) {
            keys.extend(self.ties(id).iter().map(|&(_, key)| key));
        }
        keys
    }

    // A bounded search through explicit comparisons, not an iteratively growing
    // interval domain: loops cannot require one solver iteration per integer.
    fn upper(&self, func: &Body<'_>, key: &Key, depth: usize) -> Option<i64> {
        self.upper_with_budget(func, key, depth, &mut 64)
    }

    fn upper_with_budget(
        &self,
        func: &Body<'_>,
        key: &Key,
        depth: usize,
        budget: &mut usize,
    ) -> Option<i64> {
        if depth == 0 || *budget == 0 {
            return None;
        }
        *budget -= 1;
        if let KeyData::Const(n) = func.data(*key) {
            return ((-EXACT_INT..=EXACT_INT).contains(&n)).then_some(n);
        }
        let aliases = self.aliases(func, key);
        self.ordered
            .iter()
            .filter_map(|(a, b, strict)| {
                if !aliases.contains(a) {
                    return None;
                }
                let bound = self.upper_with_budget(func, b, depth - 1, budget)?;
                Some(
                    bound
                        - i64::from(*strict && self.whole_key(func, a) && self.whole_key(func, b)),
                )
            })
            .min()
    }

    fn upper_value(&self, func: &Body<'_>, id: Idx<Inst>) -> Option<i64> {
        self.keys_of(func, id)
            .iter()
            .filter_map(|k| self.upper(func, k, 8))
            .min()
    }

    fn known_nonneg(&self, func: &Body<'_>, id: Idx<Inst>) -> bool {
        self.keys_of(func, id).iter().any(|k| {
            matches!(func.data(*k), KeyData::Const(n) if n >= 0) || self.nonneg.contains(k)
        })
    }

    fn affine_in_bounds(&self, func: &Body<'_>, index: Idx<Inst>, vec: Idx<Inst>) -> bool {
        let Some((product, row, stride, lane)) = affine_shape(func, index) else {
            return false;
        };
        // MIR instruction ids can execute again. Only expand arithmetic while
        // its operand snapshots still describe the value that was computed.
        let sum = func.key(KeyData::Sum(
            value_key(func, product),
            value_key(func, lane),
        ));
        let scale = func.key(KeyData::Scale(value_key(func, row), stride));
        if self.same.binary_search(&(index, sum)).is_err()
            || self.same.binary_search(&(product, scale)).is_err()
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
            let KeyData::Scale(count, scale) = func.data(*limit) else {
                return false;
            };
            if scale != stride || !vectors.contains(vector) || !self.whole_key(func, &count) {
                return false;
            }
            let Some(max) = self.upper(func, &count, 8) else {
                return false;
            };
            let counts = self.aliases(func, &count);
            max >= 0
                && max.checked_mul(stride).is_some_and(|n| n <= EXACT_INT)
                && self
                    .ordered
                    .iter()
                    .any(|(a, b, strict)| *strict && rows.contains(a) && counts.contains(b))
        })
    }

    fn le_len_of(&self, func: &Body<'_>, id: Idx<Inst>) -> Vec<Key> {
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
            .map(|&(_, v)| v)
            .collect()
    }

    /// Subtracting a positive integer does not prove a strict bound:
    /// large floating-point integers and infinity can round back to themselves.
    fn lt_of(&self, func: &Body<'_>, id: Idx<Inst>) -> Vec<Key> {
        let keys = self.keys_of(func, id);
        let mut out: Vec<Key> = self
            .lt
            .iter()
            .filter(|(k, _)| keys.contains(k))
            .map(|&(_, v)| v)
            .collect();
        match func.insts[id] {
            Inst::IntMod(_, b) => out.extend(self.positive_lengths(func, b)),
            _ => {}
        }
        out
    }

    /// The vectors whose live length the value `id` is: a `len` read of
    /// `v`, by the tie the read established.
    fn lengths_of(&self, func: &Body<'_>, id: Idx<Inst>) -> Vec<Key> {
        self.keys_of(func, id)
            .into_iter()
            .filter_map(|k| match func.data(k) {
                KeyData::Field(base, f) if is_length(f) => Some(base),
                _ => None,
            })
            .collect()
    }

    /// [`Facts::lengths_of`], kept to the vectors proven non-empty
    /// (`0 < len` holds).
    fn positive_lengths(&self, func: &Body<'_>, id: Idx<Inst>) -> Vec<Key> {
        self.lengths_of(func, id)
            .into_iter()
            .filter(|v| self.lt.contains(&(func.key(KeyData::Const(0)), *v)))
            .collect()
    }

    fn in_bounds(&self, func: &Body<'_>, index: Idx<Inst>, vec: Idx<Inst>) -> bool {
        if self.affine_in_bounds(func, index, vec) {
            return true;
        }
        let below = self.lt_of(func, index);
        if below.is_empty() {
            return false;
        }
        if !self.nonneg(func, index) {
            return false;
        }
        self.keys_of(func, vec).iter().any(|v| below.contains(v))
    }

    /// `v[k]` ran without raising, or was proven not to.
    fn checked_access(&mut self, func: &Body<'_>, vec: Idx<Inst>, index: Idx<Inst>) {
        let vecs = self.keys_of(func, vec);
        for k in self.keys_of(func, index) {
            self.nonneg.insert(k);
            for &v in &vecs {
                self.lt.insert((k, v));
            }
        }
    }
}

fn cond_facts(facts: &mut Facts, func: &Body<'_>, cond: Idx<Inst>, truth: bool, affine: bool) {
    if truth && affine {
        match func.insts[cond] {
            Inst::IntLt(a, b) | Inst::IntGt(b, a) => facts.ordered_fact(func, a, b, true),
            Inst::IntLeq(a, b) | Inst::IntGeq(b, a) => facts.ordered_fact(func, a, b, false),
            _ => {}
        }
    }
    match func.insts[cond] {
        Inst::Not(x) => cond_facts(facts, func, x, !truth, affine),
        Inst::Eq(a, b) if truth => equal_capacity_facts(facts, func, a, b),
        Inst::NotEq(a, b) if !truth => equal_capacity_facts(facts, func, a, b),
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

fn equal_capacity_facts(facts: &mut Facts, func: &Body<'_>, a: Idx<Inst>, b: Idx<Inst>) {
    for (value, equal) in [(a, b), (b, a)] {
        let capacities = facts.le_len_of(func, equal);
        for key in facts.keys_of(func, value) {
            for &owner in &capacities {
                facts.le_len.insert((key, owner));
            }
        }
    }
}

fn lt_fact(facts: &mut Facts, func: &Body<'_>, a: Idx<Inst>, b: Idx<Inst>) {
    let bounds = facts.le_len_of(func, b);
    for k in facts.keys_of(func, a) {
        for &v in &bounds {
            facts.lt.insert((k, v));
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

fn leq_fact(facts: &mut Facts, func: &Body<'_>, a: Idx<Inst>, b: Idx<Inst>) {
    if facts.nonneg(func, a) {
        for k in facts.keys_of(func, b) {
            facts.nonneg.insert(k);
        }
    }
}

fn after_direct_call(facts: &mut Facts, func: &Body<'_>, ctx: &Ctx<'_>, fref: FuncRef) {
    let Some(ws) = ctx.ws else {
        facts.after_unknown_call(func);
        return;
    };
    let effect = ws.effects().of(fref.module, fref.func);
    if effect.is_all() {
        facts.after_unknown_call(func);
        return;
    }
    if effect.lengths {
        facts.forget_lengths();
    }
    if effect.globals {
        facts.forget(func, Mentions::kinds(GLOBAL), |k| {
            matches!(k, KeyData::Global(_))
        });
    }
    if effect.elems {
        facts.forget_elems(func);
    }
    match &effect.fields {
        None => facts.forget(func, Mentions::kinds(FIELD), |k| {
            matches!(k, KeyData::Field(..))
        }),
        Some(fields) => {
            for &field in fields {
                facts.forget_field(func, field);
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

fn is_length(field: Ustr) -> bool {
    matches!(field.as_str(), "len" | skull_mir::mir::VEC_LENGTH_SLOT)
}

fn seed_declared_result_facts(
    facts: &mut Facts,
    func: &Body<'_>,
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
            facts.nonneg.insert(func.key(KeyData::Value(call)));
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
                    key = func.key(KeyData::Field(key, field));
                }
                facts.lt.insert((func.key(KeyData::Value(call)), key));
            }
        }
    }
}

fn step(facts: &mut Facts, func: &Body<'_>, ctx: &Ctx<'_>, id: Idx<Inst>, may_reexecute: bool) {
    let inst = &func.insts[id];
    if inst.produces_value() {
        if may_reexecute {
            facts.forget_value(func, id);
        }
        if ctx.relevant[raw(id)]
            && let Some(place) = place_of(func, id)
        {
            facts.tie(id, place);
        }
    }
    if ctx.relevant[raw(id)]
        && let Inst::IntMul(a, b) = inst
    {
        // Identical operand snapshots repeat the same floating-point operation.
        // Do not reassociate products or recursively expand expression trees.
        let product = KeyData::Product(
            facts.product_operand(func, *a),
            facts.product_operand(func, *b),
        );
        facts.tie(id, func.key(product));
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
            let scaled = func.key(KeyData::Scale(value_key(func, value), scale));
            facts.tie(id, scaled);
        }
        if let Inst::IntAdd(a, b) = inst {
            let sum = func.key(KeyData::Sum(value_key(func, *a), value_key(func, *b)));
            facts.tie(id, sum);
        }
    }
    let (private_push, private_record_read) = facts.track_private_tables(func, ctx, id);
    match inst {
        Inst::MakeStruct { fields, .. } => {
            // A fresh descriptor's length equals its initializer snapshot.
            if let Some((_, len)) = fields.iter().rev().find(|(field, _)| is_length(*field)) {
                for key in facts.keys_of(func, *len) {
                    facts.le_len.insert((key, func.key(KeyData::Value(id))));
                }
            }
        }
        Inst::Len { base, .. } => {
            for v in facts.keys_of(func, *base) {
                facts.le_len.insert((func.key(KeyData::Value(id)), v));
            }
        }
        Inst::Field { base, field, .. } if is_length(*field) => {
            for v in facts.keys_of(func, *base) {
                facts.le_len.insert((func.key(KeyData::Value(id)), v));
            }
        }
        Inst::Index {
            kind: ContainerKind::Record(_),
            ..
        } if private_record_read => {}
        Inst::Index {
            kind:
                ContainerKind::Vec
                | ContainerKind::VecInBounds
                | ContainerKind::Map
                | ContainerKind::Record(_),
            ..
        } => facts.after_unknown_call(func),
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
                    | ContainerKind::Record(_)
            ) {
                facts.after_unknown_call(func);
            } else {
                facts.forget_elems(func);
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
                    owners.contains(owner) && !func.mentions(*bound, &|k| k == KeyData::Cell(*cell))
                })
                .map(|&(bound, _)| bound)
                .collect();
            let private = facts.private_root(&value_key(func, *value));
            let whole = ctx.affine && facts.whole_value(func, *value);
            let nonneg = facts.nonneg(func, *value);
            let le_len = facts.le_len_of(func, *value);
            let lt = facts.lt_of(func, *value);
            facts.forget_cell(func, *cell);
            let k = func.key(KeyData::Cell(*cell));
            for bound in capacities {
                facts.le_len.insert((bound, k));
            }
            if let Some(root) = private {
                facts.private_tables.insert((k, root));
            }
            if whole {
                facts.whole.insert(k);
            }
            if ctx.affine
                && let Inst::Const(Lit::Int(n)) = func.insts[*value]
            {
                facts
                    .ordered
                    .insert((k, func.key(KeyData::Const(n)), false));
            }
            if nonneg {
                facts.nonneg.insert(k);
            }
            for v in le_len {
                facts.le_len.insert((k, v));
            }
            for v in lt {
                facts.lt.insert((k, v));
            }
            if ctx.relevant[raw(*value)] {
                facts.tie(*value, k);
            }
        }
        Inst::StoreGlobal { name, .. } => facts.forget(func, Mentions::kinds(GLOBAL), |k| {
            k == KeyData::Global(*name)
        }),
        Inst::StoreField {
            field, native_leaf, ..
        } => {
            let field = native_leaf.unwrap_or(*field);
            if is_length(field) {
                facts.forget_lengths();
            }
            facts.forget_field(func, field);
        }
        // A foreign backing can enter __newindex while appending an absent slot.
        Inst::ArrayPush { .. } => {
            if private_push {
                facts.after_private_push(func);
            } else {
                facts.after_unknown_call(func);
            }
        }
        Inst::Call { func: fref, args } => {
            if private_push {
                facts.after_private_push(func);
            } else {
                after_direct_call(facts, func, ctx, *fref);
            }
            if is_stdlib_fn(ctx.ws, *fref, "__nb_checked_index") && args.len() == 2 {
                facts.checked_access(func, args[0], args[1]);
            }
            seed_declared_result_facts(facts, func, ctx, id, *fref, args);
        }
        Inst::CallMulti {
            dests, func: fref, ..
        } => {
            after_direct_call(facts, func, ctx, *fref);
            for d in dests {
                facts.forget_cell(func, *d);
            }
        }
        Inst::CallValue { .. } => facts.after_unknown_call(func),
        Inst::CallValueMulti { dests, .. } | Inst::CallBoundMulti { dests, .. } => {
            facts.after_unknown_call(func);
            for d in dests {
                facts.forget_cell(func, *d);
            }
        }
        // String conversion can reach a user `to_string`.
        Inst::Format { .. } | Inst::FormatDyn { .. } | Inst::ToString(_) => {
            facts.after_unknown_call(func)
        }
        Inst::MapNext {
            key_cell, val_cell, ..
        } => {
            facts.forget_cell(func, *key_cell);
            facts.forget_cell(func, *val_cell);
        }
        _ => {}
    }
}
