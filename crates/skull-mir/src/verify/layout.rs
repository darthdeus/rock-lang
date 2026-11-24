//! Writer/reader agreement on physical layouts. Every keyed access on an object
//! whose layout is known must name a key that layout stores, every constructor
//! must write exactly its layout's keys, and no pass may move a field store
//! into a fresh temporary that nothing reads.
//!
//! An object's layout is known when the body itself says what it is: a record
//! or value constructor, a typed record index, a typed parameter, a field
//! projected from a known object, or a cell every store of which agrees. The
//! layouts come from the same plans the constructors and the prelude use.
use super::Violation;
use crate::lower::{RepArena, ValueStorage, struct_field_plan};
use crate::mir::{Block, ContainerKind, Inst, Lit, MirFunc, ValueCtorForm};
use bone_core::hir::{FnBackend, StructDef};
use bone_core::modules::display_name;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::rc::Rc;
use ustr::Ustr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Object {
    Record(Ustr),
    Value(Ustr, ValueStorage),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Unknown,
    /// A presence tag: a boolean with no fields.
    Scalar,
    Object(Object),
}

type Keys = FxHashMap<Ustr, Kind>;

/// What the stores into a cell agree on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stored {
    Unset,
    Known(Kind),
    Mixed,
}

// An arena serves one workspace, so layouts cached per arena and target stay valid.
thread_local! {
    static LAYOUTS: RefCell<(u64, FnBackend, FxHashMap<Object, Option<Rc<Keys>>>)> =
        RefCell::new((0, FnBackend::Any, FxHashMap::default()));
}

pub(super) fn verify_layout(
    ws: &Workspace,
    target: FnBackend,
    mf: &MirFunc,
    out: &mut Vec<Violation>,
) {
    if !matches!(target, FnBackend::Lua | FnBackend::Js) {
        return;
    }
    let Some(arena) = mf.rep_arena.as_deref() else {
        return;
    };
    let mut check = Check {
        ws,
        target,
        arena,
        mf,
        out,
    };
    check.run();
    for nested in &mf.nested_fns {
        verify_layout(ws, target, nested, check.out);
    }
}

struct Check<'a> {
    ws: &'a Workspace,
    target: FnBackend,
    arena: &'a RepArena,
    mf: &'a MirFunc,
    out: &'a mut Vec<Violation>,
}

impl Check<'_> {
    fn report(&mut self, block: Idx<Block>, inst: Idx<Inst>, message: String) {
        self.out.push(Violation {
            func: self.mf.name,
            block: Some(block),
            inst: Some(inst),
            message,
        });
    }

    fn struct_def(&self, name: Ustr) -> Option<&StructDef> {
        let sid = self.ws.nominals.struct_id(name)?;
        Some(&self.ws.modules[sid.module.index()].structs[sid.def])
    }

    fn object_of(&self, name: Ustr) -> Option<Object> {
        let def = self.struct_def(name)?;
        if crate::lower::has_value_plan(def) {
            let storage = self
                .arena
                .nominal_value_storage_required(self.ws, self.target, name)
                .ok()?;
            return Some(Object::Value(name, storage));
        }
        (!def.is_value && !def.is_nobox && !def.lua_named && !def.fields.is_empty())
            .then_some(Object::Record(name))
    }

    fn child_kind(&self, child: Option<Ustr>) -> Kind {
        child
            .and_then(|name| self.object_of(name))
            .map_or(Kind::Unknown, Kind::Object)
    }

    fn layout(&self, object: Object) -> Option<Rc<Keys>> {
        let key = (self.arena.id(), self.target);
        let cached = LAYOUTS.with_borrow_mut(|(id, target, layouts)| {
            if (*id, *target) != key {
                (*id, *target) = key;
                layouts.clear();
            }
            layouts.get(&object).cloned()
        });
        if let Some(keys) = cached {
            return keys;
        }
        let keys = self.plan_layout(object).map(Rc::new);
        LAYOUTS.with_borrow_mut(|(_, _, layouts)| layouts.insert(object, keys.clone()));
        keys
    }

    fn plan_layout(&self, object: Object) -> Option<Keys> {
        let (ws, target) = (self.ws, self.target);
        let mut keys = Keys::default();
        match object {
            Object::Record(name) => {
                let def = self.struct_def(name)?;
                for key in crate::lower::record_keys(ws, name)? {
                    keys.insert(key, Kind::Unknown);
                }
                for field in &def.fields {
                    let plan = struct_field_plan(ws, target, name, field.name, false).ok()?;
                    let kind = match plan.embed() {
                        Some((_, true)) => Kind::Scalar,
                        Some(_) => continue,
                        None => self.child_kind(plan.direct_child()),
                    };
                    if let Some(slot) = keys.get_mut(&field.name) {
                        *slot = kind;
                    }
                }
            }
            Object::Value(name, ValueStorage::GcTable) => {
                for field in crate::lower::gc_table_fields(ws, target, name).ok()? {
                    if field.is_inline() {
                        for (key, presence) in field.keys() {
                            let kind = if presence {
                                Kind::Scalar
                            } else {
                                Kind::Unknown
                            };
                            keys.insert(key, kind);
                        }
                    } else {
                        let plan = struct_field_plan(ws, target, name, field.name, false).ok()?;
                        keys.insert(field.name, self.child_kind(plan.direct_child()));
                    }
                }
            }
            Object::Value(name, ValueStorage::LeafTable) => {
                let paths = self
                    .arena
                    .nominal_value_leaf_paths_required(ws, target, name)
                    .ok()?;
                let presence = self
                    .arena
                    .nominal_value_leaf_presence_flags_required(ws, target, name)
                    .ok()?;
                for (i, path) in paths.into_iter().enumerate() {
                    let tag = presence.get(i).copied().unwrap_or(false);
                    keys.insert(path, if tag { Kind::Scalar } else { Kind::Unknown });
                }
            }
            Object::Value(name, ValueStorage::CData) => {
                for (field, ctype) in crate::lower::value_c_fields(ws, name)? {
                    let kind = match ctype {
                        crate::lower::CFieldType::Scalar(_) => Kind::Unknown,
                        crate::lower::CFieldType::Aggregate(dep) => {
                            Kind::Object(Object::Value(dep, ValueStorage::CData))
                        }
                    };
                    keys.insert(field, kind);
                }
                let paths = self
                    .arena
                    .nominal_value_leaf_paths_required(ws, target, name)
                    .ok()?;
                for path in paths.iter().filter(|path| path.contains('.')) {
                    let alias = crate::lower::cdata_leaf_alias(ws, name, path.as_str());
                    keys.insert(alias, Kind::Unknown);
                }
            }
        }
        Some(keys)
    }

    fn run(&mut self) {
        let mf = self.mf;
        let live = live_insts(mf);
        let mut kinds = vec![Kind::Unknown; mf.insts.len()];
        let mut seed = vec![Stored::Unset; mf.cells.len()];
        for &cell in &mf.params {
            seed[raw(cell)] = Stored::Known(self.param_kind(cell));
        }
        for (cell, data) in mf.cells.iter() {
            if data.borrowed {
                seed[raw(cell)] = Stored::Mixed;
            }
        }
        // A cell's kind is the one every store into it agrees on. Each round
        // follows copies one cell further; a body that has not settled after a
        // few rounds is not checked.
        let mut cells = vec![Stored::Mixed; mf.cells.len()];
        let mut settled = false;
        for _ in 0..4 {
            let mut stores = seed.clone();
            for &(_, iid) in &live {
                let kind = self.kind_of(iid, &kinds, &cells, None);
                kinds[raw(iid)] = kind;
                match mf.insts[iid] {
                    Inst::Store { cell, value } => {
                        let slot = &mut stores[raw(cell)];
                        *slot = match *slot {
                            Stored::Unset => Stored::Known(kinds[raw(value)]),
                            Stored::Known(seen) if seen == kinds[raw(value)] => Stored::Known(seen),
                            _ => Stored::Mixed,
                        };
                    }
                    Inst::Load(_) => {}
                    // Call results, iterator cursors and the like are never known.
                    ref other => other.cells_each(&mut |cell| stores[raw(cell)] = Stored::Mixed),
                }
            }
            if stores == cells {
                settled = true;
                break;
            }
            cells = stores;
        }
        if !settled {
            return;
        }
        for &(bid, iid) in &live {
            let mut violation = None;
            self.kind_of(iid, &kinds, &cells, Some(&mut violation));
            if let Some(message) = violation {
                self.report(bid, iid, message);
            }
        }
    }

    fn param_kind(&self, cell: Idx<crate::mir::Cell>) -> Kind {
        let Some(ty) = self.mf.cells[cell].ty else {
            return Kind::Unknown;
        };
        match crate::lower::static_ty(self.ws, ty) {
            Some(bone_core::hir::TypeExpr::App {
                ctor: bone_core::hir::TypeCtor::Nominal(name),
                ..
            }) => match self.object_of(name) {
                Some(object @ Object::Record(_)) => Kind::Object(object),
                _ => Kind::Unknown,
            },
            _ => Kind::Unknown,
        }
    }

    /// The kind `iid` produces; with `violation`, also what is wrong with it.
    fn kind_of(
        &mut self,
        iid: Idx<Inst>,
        kinds: &[Kind],
        cells: &[Stored],
        mut violation: Option<&mut Option<String>>,
    ) -> Kind {
        let mf = self.mf;
        let mut flag = |message: String| {
            if let Some(slot) = violation.as_deref_mut() {
                slot.get_or_insert(message);
            }
        };
        match &mf.insts[iid] {
            Inst::MakeStruct {
                ty: Some(name),
                fields,
            } => {
                let Some(object) = self
                    .object_of(*name)
                    .filter(|o| matches!(o, Object::Record(_)))
                else {
                    return Kind::Unknown;
                };
                if let Some(keys) = self.layout(object) {
                    if let Some((key, _)) = fields.iter().find(|(key, _)| !keys.contains_key(key)) {
                        flag(format!(
                            "constructor of `{}` writes `{key}`, which its layout does not store",
                            display_name(name.as_str())
                        ));
                    }
                }
                Kind::Object(object)
            }
            Inst::MakeValueStruct {
                name,
                representation,
                form,
                fields,
            }
            | Inst::MaterializeValueStruct {
                name,
                representation,
                form,
                fields,
            } => {
                let Some(plan) = self.arena.try_get(*representation) else {
                    return Kind::Unknown;
                };
                if plan.target() != self.target {
                    return Kind::Unknown;
                }
                let Some(storage) = plan.value_storage() else {
                    return Kind::Unknown;
                };
                let object = Object::Value(*name, storage);
                if *form == ValueCtorForm::Declared && storage == ValueStorage::GcTable {
                    if let Some(keys) = self.layout(object) {
                        let missing = fields.iter().find(|(key, _)| !keys.contains_key(key));
                        if let Some((key, _)) = missing {
                            flag(format!(
                                "constructor of `{}` writes `{key}`, which its layout does not store",
                                display_name(name.as_str())
                            ));
                        } else if fields.len() != keys.len() {
                            flag(format!(
                                "constructor of `{}` writes {} keys, but its layout stores {}",
                                display_name(name.as_str()),
                                fields.len(),
                                keys.len()
                            ));
                        }
                    }
                }
                Kind::Object(object)
            }
            Inst::Load(cell) => match cells[raw(*cell)] {
                Stored::Known(kind) => kind,
                Stored::Unset | Stored::Mixed => Kind::Unknown,
            },
            Inst::Field { base, field, .. } => {
                self.access(kinds[raw(*base)], *field, "reads", &mut flag)
            }
            Inst::StoreField { base, field, .. } => {
                self.access(kinds[raw(*base)], *field, "writes", &mut flag);
                Kind::Unknown
            }
            Inst::Index {
                base,
                index,
                kind: ContainerKind::Record(owner),
            }
            | Inst::StoreIndex {
                base,
                index,
                kind: ContainerKind::Record(owner),
                ..
            } => {
                let Inst::Const(Lit::Str(key)) = mf.insts[*index] else {
                    return Kind::Unknown;
                };
                let verb = if matches!(mf.insts[iid], Inst::Index { .. }) {
                    "reads"
                } else {
                    "writes"
                };
                let base_kind = match owner.and_then(|name| self.object_of(name)) {
                    Some(object @ Object::Record(_)) => Kind::Object(object),
                    _ => kinds[raw(*base)],
                };
                let kind = self.access(base_kind, key, verb, &mut flag);
                if verb == "reads" { kind } else { Kind::Unknown }
            }
            _ => Kind::Unknown,
        }
    }

    fn access(&mut self, base: Kind, key: Ustr, verb: &str, flag: &mut impl FnMut(String)) -> Kind {
        match base {
            Kind::Unknown => Kind::Unknown,
            Kind::Scalar => {
                flag(format!(
                    "{verb} `{key}` of a presence tag, which has no fields"
                ));
                Kind::Unknown
            }
            Kind::Object(object) => {
                let Some(keys) = self.layout(object) else {
                    return Kind::Unknown;
                };
                match keys.get(&key) {
                    Some(kind) => *kind,
                    None => {
                        let (name, storage) = match object {
                            Object::Record(name) => (name, "record"),
                            Object::Value(name, ValueStorage::GcTable) => (name, "table"),
                            Object::Value(name, ValueStorage::LeafTable) => (name, "leaf table"),
                            Object::Value(name, ValueStorage::CData) => (name, "cdata"),
                        };
                        flag(format!(
                            "{verb} `{key}` of a `{}` {storage}, which does not store it",
                            display_name(name.as_str())
                        ));
                        Kind::Unknown
                    }
                }
            }
        }
    }
}

/// Field stores into a cell that is read again: a pass must keep them on
/// storage something reads.
pub(super) fn stores_into_read_cells(mf: &MirFunc) -> Vec<Idx<Inst>> {
    let live = live_insts(mf);
    let mut loads: FxHashMap<Idx<crate::mir::Cell>, usize> = FxHashMap::default();
    for &(_, iid) in &live {
        if let Inst::Load(cell) = mf.insts[iid] {
            *loads.entry(cell).or_default() += 1;
        }
    }
    live.into_iter()
        .filter_map(|(_, iid)| {
            let root = store_root(mf, iid)?;
            match mf.insts[root] {
                Inst::Load(cell) if loads.get(&cell).is_some_and(|&n| n > 1) => Some(iid),
                _ => None,
            }
        })
        .collect()
}

/// `before` came from [`stores_into_read_cells`] ahead of `pass`.
pub(super) fn lost_writes(
    mf: &MirFunc,
    before: &[Idx<Inst>],
    pass: &dyn std::fmt::Display,
    out: &mut Vec<Violation>,
) {
    let moved: Vec<_> = before
        .iter()
        .filter_map(|&store| {
            let root = store_root(mf, store)?;
            match &mf.insts[root] {
                Inst::MakeStruct { ty: Some(name), .. }
                | Inst::MakeValueStruct { name, .. }
                | Inst::MaterializeValueStruct { name, .. } => Some((store, root, *name)),
                _ => None,
            }
        })
        .collect();
    if moved.is_empty() {
        return;
    }
    let mut users: FxHashMap<Idx<Inst>, Vec<Idx<Inst>>> = FxHashMap::default();
    let mut placed = FxHashMap::default();
    let mut term_used = rustc_hash::FxHashSet::default();
    for bid in live_blocks(mf) {
        let block = &mf.blocks[bid];
        for &iid in &block.insts {
            placed.insert(iid, bid);
            mf.insts[iid].uses_each(&mut |operand| users.entry(operand).or_default().push(iid));
        }
        block.term.uses_each(&mut |operand| {
            term_used.insert(operand);
        });
    }
    for (store, root, name) in moved {
        let Some(&bid) = placed.get(&store) else {
            continue;
        };
        if !observed(mf, root, &users, &term_used) {
            out.push(Violation {
                func: mf.name,
                block: Some(bid),
                inst: Some(store),
                message: format!(
                    "{pass} moved a field store into a fresh `{}` that nothing reads, so the write is lost",
                    display_name(name.as_str())
                ),
            });
        }
    }
}

fn store_root(mf: &MirFunc, store: Idx<Inst>) -> Option<Idx<Inst>> {
    let mut root = match mf.insts[store] {
        Inst::StoreField { base, .. }
        | Inst::StoreIndex {
            base,
            kind: ContainerKind::Record(_),
            ..
        } => base,
        _ => return None,
    };
    while let Some(parent) = projection_base(mf, root) {
        root = parent;
    }
    Some(root)
}

fn projection_base(mf: &MirFunc, iid: Idx<Inst>) -> Option<Idx<Inst>> {
    match mf.insts[iid] {
        Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => Some(base),
        Inst::Index {
            base,
            kind: ContainerKind::Record(_),
            ..
        } => Some(base),
        _ => None,
    }
}

/// Whether anything other than a projection or a store through it uses `root`.
fn observed(
    mf: &MirFunc,
    root: Idx<Inst>,
    users: &FxHashMap<Idx<Inst>, Vec<Idx<Inst>>>,
    term_used: &rustc_hash::FxHashSet<Idx<Inst>>,
) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if term_used.contains(&node) {
            return true;
        }
        for &user in users.get(&node).map_or(&[][..], Vec::as_slice) {
            match mf.insts[user] {
                Inst::StoreField { base, value, .. } if base == node && value != node => {}
                Inst::StoreIndex {
                    base,
                    index,
                    value,
                    kind: ContainerKind::Record(_),
                } if base == node && index != node && value != node => {}
                _ if projection_base(mf, user) == Some(node) => stack.push(user),
                _ => return true,
            }
        }
    }
    false
}

fn raw<T>(idx: Idx<T>) -> usize {
    idx.into_raw().into_u32() as usize
}

/// Insts of the reachable blocks in block order, each with its block.
fn live_insts(mf: &MirFunc) -> Vec<(Idx<Block>, Idx<Inst>)> {
    live_blocks(mf)
        .into_iter()
        .flat_map(|bid| {
            mf.blocks[bid]
                .insts
                .iter()
                .filter(|&&iid| raw(iid) < mf.insts.len())
                .map(move |&iid| (bid, iid))
        })
        .collect()
}

/// The reachable blocks in block order.
fn live_blocks(mf: &MirFunc) -> Vec<Idx<Block>> {
    let mut seen = vec![false; mf.blocks.len()];
    let mut entries = vec![mf.entry];
    entries.extend(mf.coro_defers.iter().map(|&(_, entry)| entry));
    for inst in mf.insts.values() {
        if let Inst::Lambda { entry, .. } = inst {
            entries.push(*entry);
        }
    }
    let mut order = Vec::new();
    let mut stack = Vec::new();
    for entry in entries {
        if raw(entry) >= mf.blocks.len() || seen[raw(entry)] {
            continue;
        }
        seen[raw(entry)] = true;
        stack.push(entry);
        while let Some(bid) = stack.pop() {
            order.push(bid);
            crate::cfg::successors(&mf.blocks[bid].term, |s| {
                if raw(s) < mf.blocks.len() && !seen[raw(s)] {
                    seen[raw(s)] = true;
                    stack.push(s);
                }
            });
        }
    }
    order.sort_by_key(|b| raw(*b));
    order
}
