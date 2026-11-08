use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::scope;
use crate::visit::{raw, top_level_blocks};
use bone_semantic::prelude::CompilerError;
use comfy_la_arena::{Idx, RawIdx};
use skull_mir::mir::*;

/// A scalarizable aggregate ctor's identity, consistent across all stores to one
/// cell (a cell holds one type). The rewrite is identical for all; the variant
/// only distinguishes the ctor instructions for the consistency check.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StructId {
    /// `MakeValueStruct` -- an immutable `value` cdata.
    Value(ustr::Ustr),
    /// `MakeStruct` -- a mutable plain-struct table (`ty` is the `__ty` nominal,
    /// `None` for a foreign nominal).
    Plain(Option<ustr::Ustr>),
    /// `MakeEnum` / `MakeEnumDyn` -- a payload-variant enum table
    /// (`{__variant, __ty, [i]=..}`).
    /// Keyed by the ENUM name (`__ty`), so every variant of one enum stored to a
    /// cell is consistent (the field union covers all variants' payload slots).
    Enum(ustr::Ustr),
    /// `MakeTuple` -- a positional table (`{v0, v1, ..}`), keyed by width. Fields
    /// are `_<i>`, the same naming `TupleIndex` reads already rewrite through.
    Tuple(usize),
}

/// The source of a scalarized field's value: an existing operand, or a string
/// CONST to materialize (an enum table bakes `__variant`/`__ty` as literals, not
/// operands, so splitting it must emit the `Const(Str)` insts).
enum FieldSrc {
    Operand(Idx<Inst>),
    ConstStr(ustr::Ustr),
    /// Leaf-wise copy from another family member's field cell (a value-struct
    /// alias member, see `copy_alias`) -- materializes a `Load` at the store site.
    CellLoad(Idx<Cell>),
    /// A nullable value cell's presence leaf on a ctor store.
    ConstTrue,
    ConstNil,
}

/// If `inst` is a scalarizable aggregate ctor, its identity + logical field list.
/// A `value`/plain struct's fields are its declared (name, operand) pairs. An
/// enum's are `__variant`/`__ty` (string consts) + one `_<i>` per positional
/// payload + each record-payload field by name -- the table's runtime keys (the
/// payload is at positional Lua index `[i+1]`, read via `TupleIndex`, named `_<i>`
/// here so a `match` payload read rewrites to the same field cell).
///
/// `enum_hint` is the cell's enum name (from its `MakeEnum` stores). A unit
/// variant of a MIXED enum is stored as a reference to a shared singleton GLOBAL
/// `__rock_enum_<symbol(E)>_<V>` (not a `MakeEnum`); given `enum_hint = E` we recognize it
/// as the same enum's ctor with fields `{__variant=V, __ty=E}` (no payload), so a
/// cell mixing payload + unit variants stays uniform and splits. The hint is what
/// disambiguates `V` from `E` in the underscore-joined global name.
fn ctor_fields(
    inst: &Inst,
    enum_hint: Option<ustr::Ustr>,
) -> Option<(StructId, Vec<(ustr::Ustr, FieldSrc)>)> {
    let op = |fs: &[(ustr::Ustr, Idx<Inst>)]| {
        fs.iter()
            .map(|(n, v)| (*n, FieldSrc::Operand(*v)))
            .collect::<Vec<_>>()
    };
    match inst {
        Inst::MakeValueStruct { name, fields, .. } => Some((StructId::Value(*name), op(fields))),
        Inst::MakeStruct { ty, fields } => Some((StructId::Plain(*ty), op(fields))),
        Inst::MakeTuple(vs) => Some((
            StructId::Tuple(vs.len()),
            vs.iter()
                .enumerate()
                .map(|(i, v)| (ustr::Ustr::from(&format!("_{i}")), FieldSrc::Operand(*v)))
                .collect(),
        )),
        Inst::MakeEnum {
            tag,
            ty,
            positional,
            named,
        } => {
            let mut f = vec![
                (ustr::Ustr::from("__variant"), FieldSrc::ConstStr(*tag)),
                (
                    ustr::Ustr::from("__ty"),
                    FieldSrc::ConstStr(skull_mir::mangle::symbol_of_key(*ty)),
                ),
            ];
            for (i, p) in positional.iter().enumerate() {
                f.push((ustr::Ustr::from(&format!("_{i}")), FieldSrc::Operand(*p)));
            }
            for (n, v) in named {
                f.push((*n, FieldSrc::Operand(*v)));
            }
            Some((StructId::Enum(*ty), f))
        }
        Inst::MakeEnumDyn {
            tag,
            ty,
            positional,
            named,
        } => {
            let mut f = vec![
                (ustr::Ustr::from("__variant"), FieldSrc::Operand(*tag)),
                (
                    ustr::Ustr::from("__ty"),
                    FieldSrc::ConstStr(skull_mir::mangle::symbol_of_key(*ty)),
                ),
            ];
            for (i, p) in positional.iter().enumerate() {
                f.push((ustr::Ustr::from(&format!("_{i}")), FieldSrc::Operand(*p)));
            }
            for (n, v) in named {
                f.push((*n, FieldSrc::Operand(*v)));
            }
            Some((StructId::Enum(*ty), f))
        }
        // A unit-variant singleton ref of the cell's enum: `__rock_enum_<E>_<V>`.
        Inst::Global(g) => {
            let e = enum_hint?;
            // The singleton and the `__ty` leaf carry the minted type symbol, as
            // the emitted global and every constructed box do; for a v1 enum that
            // differs from its DefKey.
            let symbol = skull_mir::mangle::symbol_of_key(e);
            let variant = g.as_str().strip_prefix(&format!("__rock_enum_{symbol}_"))?;
            Some((
                StructId::Enum(e),
                vec![
                    (
                        ustr::Ustr::from("__variant"),
                        FieldSrc::ConstStr(ustr::Ustr::from(variant)),
                    ),
                    (ustr::Ustr::from("__ty"), FieldSrc::ConstStr(symbol)),
                ],
            ))
        }
        _ => None,
    }
}

fn value_ctor_plan(inst: &Inst) -> Option<(skull_mir::lower::RepId, ValueCtorForm)> {
    match inst {
        Inst::MakeValueStruct {
            representation,
            form,
            ..
        }
        | Inst::MaterializeValueStruct {
            representation,
            form,
            ..
        } => Some((*representation, *form)),
        _ => None,
    }
}

/// One SROA round; returns whether it rewrote anything (drives the fixpoint).
///
/// The round runs in two halves. Analysis builds a [`Families`] table over the
/// cells that exist when the round starts: candidate collection from the
/// stores, alias-family resolution, the escape scan over every load, and the
/// family-wide settling of disqualification. The rewrite then allocates field
/// cells on demand ([`FieldCells`]) and redirects nil tests, projections,
/// stores and escaping loads onto them, splicing new insts into their blocks
/// last.
pub(crate) fn scalarize_struct_cells_once(func: &mut MirFunc) -> Result<bool, CompilerError> {
    let captured_cells = captured_cells(func);
    let mut fam = Families::collect(func);
    fam.resolve_aliases();
    fam.split_copy_aliases(&captured_cells);
    let uses = fam.scan_load_uses(func);
    fam.settle_disqualification(&uses.escape_loads, &uses.chained_roots);
    fam.add_presence_leaves(&uses.value_nil_tests);
    if !fam.any_candidate() {
        return Ok(false);
    }

    let mut cells = FieldCells::default();
    rewrite_nil_tests(
        func,
        &fam,
        &mut cells,
        uses.enum_nil_tests,
        uses.value_nil_tests,
    );
    rewrite_projections(func, &fam, &mut cells, &uses.load_cell);
    let store_expansions = plan_store_expansions(func, &fam)?;
    let mut replacement = alias_deletions(func, &fam);
    expand_stores(func, &mut cells, store_expansions, &mut replacement);
    rematerialize_escapes(func, &fam, &mut cells, uses.escape_loads, &mut replacement);
    splice_replacements(func, &replacement);
    Ok(true)
}

/// A nullable value family's presence leaf.
fn present_field() -> ustr::Ustr {
    ustr::Ustr::from("__present")
}

/// Cells some closure captures (empty when the body has no lambda).
fn captured_cells(func: &MirFunc) -> HashSet<Idx<Cell>> {
    if func
        .insts
        .iter()
        .any(|(_, inst)| matches!(inst, Inst::Lambda { .. }))
    {
        let tree = scope::ScopeTree::build(func);
        let ownership = tree.analyze(func);
        ownership
            .captures
            .iter()
            .flat_map(|captures| captures.iter().copied())
            .collect()
    } else {
        HashSet::default()
    }
}

/// A value ctor's declared field names, with its representation and form.
type ValueOrder = (Vec<ustr::Ustr>, skull_mir::lower::RepId, ValueCtorForm);

/// The per-cell analysis tables of one SROA round, indexed by raw cell id.
/// Every table has `nc` entries: field cells allocated during the rewrite lie
/// past it and are never candidates this round.
struct Families {
    nc: usize,
    // A cell is a candidate iff every store to it is a struct ctor of one
    // consistent type. `cand[c]`: None = not yet seen / disqualified, Some(id) =
    // all stores so far are that struct. `cand_fields[c]` is its declared field
    // set, used to reject a `Field`/`StoreField` of an undeclared field (e.g. a
    // `__ty` reflective read) -- splitting one would read a never-stored cell.
    cand: Vec<Option<StructId>>,
    cand_fields: Vec<Option<HashSet<ustr::Ustr>>>,
    disq: Vec<bool>,
    has_store: Vec<bool>,
    /// The enum hint per cell, see [`cell_enum_hints`].
    cell_enum: Vec<Option<ustr::Ustr>>,
    // ALIAS stores (`Store(c, Load(o))`): a cell whose every store copies
    // another candidate cell is the SAME object under Rock's by-ref struct
    // semantics (the shape a spliced inner `next`'s param has after the outer
    // cursor peels -- combinator chains nest cursors). Such a cell is UNIFIED
    // with its root: its field accesses share the root's field cells (so
    // by-ref mutation still lands in the shared cells) and the alias store
    // deletes. `alias[c] = Some(o)`; mixed ctor+alias stores stay whole.
    alias: Vec<Option<Idx<Cell>>>,
    // Escape-site rematerialization (item 8): a whole-value load of a
    // `nonnull_value_struct` binding that would otherwise ESCAPE (disqualifying
    // its family) is instead rebuilt from the ctor's per-field cells AT the use
    // site. `value_order` keeps each value ctor's declared field order (the
    // rebuilt `MakeValueStruct` is positional); `escape_loads` are the loads to
    // rewrite. The flag -- set at lowering only for non-null value bindings --
    // is the ONLY license: no store/CFG heuristic (both proven unsound/too weak).
    value_order: HashMap<usize, ValueOrder>,
    /// Each alias member's ctor-rooted representative (itself when none).
    rep: Vec<usize>,
    /// The source an immutable alias member copies its leaves from.
    copy_alias: Vec<Option<Idx<Cell>>>,
    /// Roots of families that carry a presence leaf.
    presence_roots: HashSet<usize>,
}

/// How every candidate load is consumed, gathered by [`Families::scan_load_uses`].
struct LoadUses {
    load_cell: HashMap<Idx<Inst>, Idx<Cell>>,
    escape_loads: Vec<(Idx<Inst>, Idx<Cell>)>,
    enum_nil_tests: Vec<(Idx<Inst>, Idx<Inst>, Idx<Cell>)>,
    // A nullable value cell (a `V?` read joined with `nil`, as `if let` and an
    // inlined `unwrap()` produce) is tested against nil before its fields are
    // read. The split cell answers that test from a presence leaf.
    value_nil_tests: Vec<(Idx<Inst>, Idx<Inst>, Idx<Cell>)>,
    // Roots of families that split only because a copy of a copy is accepted.
    chained_roots: HashSet<usize>,
}

/// Cells that can never split: params (the Lua signature), cells written
/// through a `ref` alias this pass cannot see (`Cell::borrowed`), the resume
/// argument, and a cell defined by anything OTHER than a `Store` (a call
/// multi-return dest, a closure param, a map-iteration variable), which has no
/// struct ctor to split.
fn initially_disqualified(func: &MirFunc, nc: usize) -> Vec<bool> {
    let mut disq = vec![false; nc];
    for c in &func.params {
        disq[raw(*c) as usize] = true; // params are the Lua signature; never split
    }
    for (c, cell) in func.cells.iter() {
        if cell.borrowed {
            disq[raw(c) as usize] = true;
        }
    }
    if let Some(ra) = func.resume_arg {
        disq[raw(ra) as usize] = true;
    }
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            match &func.insts[i] {
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => {
                    dests.iter().for_each(|c| disq[raw(*c) as usize] = true)
                }
                Inst::Lambda { params, .. } => {
                    params.iter().for_each(|c| disq[raw(*c) as usize] = true)
                }
                Inst::MapNext {
                    key_cell, val_cell, ..
                } => {
                    disq[raw(*key_cell) as usize] = true;
                    disq[raw(*val_cell) as usize] = true;
                }
                _ => {}
            }
        }
    }
    disq
}

/// Per-cell enum name from its `MakeEnum` stores -- the hint that lets a
/// unit-variant singleton-ref store (`__rock_enum_<E>_<V>`) be recognized as the
/// same enum's ctor (it carries no `ty` of its own). A cell with no `MakeEnum`
/// store has `None` and so never matches a singleton (all-singleton cells don't
/// allocate -- nothing to scalarize). Conflicting enums leave the first; the
/// `StructId` consistency check then disqualifies the cell.
fn cell_enum_hints(func: &MirFunc, nc: usize) -> Vec<Option<ustr::Ustr>> {
    let mut cell_enum: Vec<Option<ustr::Ustr>> = vec![None; nc];
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            if let Inst::Store { cell, value } = &func.insts[i] {
                if let Inst::MakeEnum { ty, .. } | Inst::MakeEnumDyn { ty, .. } =
                    &func.insts[*value]
                {
                    cell_enum[raw(*cell) as usize].get_or_insert(*ty);
                }
            }
        }
    }
    cell_enum
}

/// Value-use counts: a ctor stored into a candidate cell must have NO use
/// besides that store. Any other consumer (a `Field` read the interleaved
/// copy-prop forwarded onto the ctor, a second cell store) still sees the
/// ORIGINAL table -- splitting the cell would diverge the two.
fn value_use_counts(func: &MirFunc) -> Vec<u32> {
    let mut vuse = vec![0u32; func.insts.len()];
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            func.insts[i].uses_each(&mut |op| vuse[raw(op) as usize] += 1);
        }
        b.term.uses_each(&mut |op| vuse[raw(op) as usize] += 1);
    }
    vuse
}

impl Families {
    /// Candidate collection: classify every store as a ctor, an alias copy, a
    /// nil store, or a disqualifying write.
    fn collect(func: &MirFunc) -> Self {
        let nc = func.cells.len();
        let mut value_order: HashMap<usize, ValueOrder> = HashMap::default();
        let mut cand: Vec<Option<StructId>> = vec![None; nc];
        let mut cand_fields: Vec<Option<HashSet<ustr::Ustr>>> = vec![None; nc];
        let mut disq = initially_disqualified(func, nc);
        let mut has_store = vec![false; nc];
        let cell_enum = cell_enum_hints(func, nc);
        // Only the main CFG -- a cell touched inside a closure body is opaque (the
        // closure may run on any call), so leave it whole.
        let mut alias: Vec<Option<Idx<Cell>>> = vec![None; nc];
        let main: HashSet<Idx<Block>> = top_level_blocks(func).into_iter().collect();
        let vuse = value_use_counts(func);
        for (bid, b) in func.blocks.iter() {
            let in_main = main.contains(&bid);
            for &i in &b.insts {
                if let Inst::Store { cell, value } = &func.insts[i] {
                    let ck = raw(*cell) as usize;
                    has_store[ck] = true;
                    if !in_main {
                        disq[ck] = true;
                        continue;
                    }
                    if let Inst::Load(o) = &func.insts[*value] {
                        if *o == *cell || alias[ck].is_some_and(|p| p != *o) || cand[ck].is_some() {
                            disq[ck] = true;
                        } else {
                            alias[ck] = Some(*o);
                        }
                        continue;
                    }
                    if alias[ck].is_some() {
                        disq[ck] = true;
                        continue;
                    }
                    // A plain nil store (a folded `phantom()` on an iterator done
                    // path) is compatible with any ctor candidate: it neither
                    // establishes nor breaks the cell's struct id. The rewrite
                    // expands it to per-field nil stores.
                    if matches!(&func.insts[*value], Inst::Const(Lit::Nil)) {
                        continue;
                    }
                    match ctor_fields(&func.insts[*value], cell_enum[ck]) {
                        Some((id, fields)) => {
                            if cand[ck].is_some_and(|n| n != id) || vuse[raw(*value) as usize] != 1
                            {
                                disq[ck] = true;
                            } else {
                                cand[ck] = Some(id);
                                if matches!(id, StructId::Value(_)) {
                                    let Some((representation, form)) =
                                        value_ctor_plan(&func.insts[*value])
                                    else {
                                        disq[ck] = true;
                                        continue;
                                    };
                                    value_order.entry(ck).or_insert_with(|| {
                                        (
                                            fields.iter().map(|(n, _)| *n).collect(),
                                            representation,
                                            form,
                                        )
                                    });
                                }
                                cand_fields[ck]
                                    .get_or_insert_with(HashSet::default)
                                    .extend(fields.iter().map(|(n, _)| *n));
                            }
                        }
                        None => disq[ck] = true,
                    }
                }
            }
        }
        Families {
            nc,
            cand,
            cand_fields,
            disq,
            has_store,
            cell_enum,
            alias,
            value_order,
            rep: (0..nc).collect(),
            copy_alias: vec![None; nc],
            presence_roots: HashSet::default(),
        }
    }

    /// Resolve alias chains to their ctor-rooted representative. A chain that
    /// cycles, or doesn't end at a live ctor candidate, stays whole. Alias
    /// members adopt the root's struct id; their field set IS the root's.
    fn resolve_aliases(&mut self) {
        let Families {
            nc,
            cand,
            disq,
            alias,
            rep,
            ..
        } = self;
        let nc = *nc;
        for k in 0..nc {
            if alias[k].is_none() || disq[k] {
                continue;
            }
            let mut cur = k;
            let mut steps = 0usize;
            let root = loop {
                match alias[cur] {
                    Some(o) if steps <= nc => {
                        cur = raw(o) as usize;
                        steps += 1;
                        // An alias THROUGH a disqualified cell must not resolve past it:
                        // a disqualified intermediate stays a whole aggregate whose value
                        // is not its first alias's (e.g. a mixed-alias `if`-result cell,
                        // stored `a` on one arm and `b` on the other). Following its stale
                        // `alias[]` to that first root would unify this member with the
                        // wrong value -- latent until escape-remat actually reads the rep.
                        if disq[cur] {
                            break None;
                        }
                    }
                    Some(_) => break None, // cycle
                    None => break Some(cur),
                }
            };
            match root {
                Some(r) if !disq[r] && cand[r].is_some() => {
                    rep[k] = r;
                    cand[k] = cand[r];
                }
                _ => disq[k] = true,
            }
        }
    }

    /// An immutable aggregate alias member whose lifetime can escape is a
    /// distinct BINDING, not the same object. Value structs already keep their
    /// own leaves because whole-value rematerialization is available for them.
    /// An enum needs distinct leaves only when a closure captures the alias;
    /// otherwise the root cannot change before the alias's last observable use,
    /// and sharing its scalar cells avoids rematerializing enum tables.
    /// In either case,
    /// collapsing the cells hands the member the ROOT's live range -- which a
    /// closure capture observes: the leaves of a per-iteration `for` binding
    /// become the loop cursor's multi-store slots, boxed once for the whole
    /// function and nil-ed on the exhausted path. Such a member keeps its own
    /// leaf cells and its alias store expands to leaf-wise copies. Reference
    /// structs still unify: there `let q = p` aliases and by-ref mutation must
    /// land in shared cells.
    fn split_copy_aliases(&mut self, captured_cells: &HashSet<Idx<Cell>>) {
        let Families {
            nc,
            cand,
            cand_fields,
            disq,
            value_order,
            rep,
            copy_alias,
            ..
        } = self;
        for k in 0..*nc {
            let cell = Idx::from_raw(RawIdx::from_u32(k as u32));
            let copies_binding = matches!(cand[rep[k]], Some(StructId::Value(_)))
                || (matches!(cand[rep[k]], Some(StructId::Enum(_)))
                    && captured_cells.contains(&cell));
            if rep[k] == k || disq[k] || !copies_binding {
                continue;
            }
            copy_alias[k] = Some(Idx::from_raw(RawIdx::from_u32(rep[k] as u32)));
            cand_fields[k] = cand_fields[rep[k]].clone();
            if let Some(order) = value_order.get(&rep[k]).cloned() {
                value_order.insert(k, order);
            }
            rep[k] = k;
        }
    }

    /// Escape analysis. A `Load` of a candidate cell may be used ONLY as a
    /// `Field`/`StoreField` base of a declared field -- or as the value of an
    /// accepted alias store within the same family (that store deletes). Map
    /// each such load to its cell, then scan every operand use: any other
    /// consumer means the whole struct escapes -> disqualify.
    fn scan_load_uses(&mut self, func: &MirFunc) -> LoadUses {
        let Families {
            cand,
            cand_fields,
            disq,
            alias,
            rep,
            copy_alias,
            ..
        } = self;
        let mut escape_loads: Vec<(Idx<Inst>, Idx<Cell>)> = Vec::new();
        let mut load_cell: HashMap<Idx<Inst>, Idx<Cell>> = HashMap::default();
        for (_, b) in func.blocks.iter() {
            for &i in &b.insts {
                if let Inst::Load(c) = &func.insts[i] {
                    let ck = raw(*c) as usize;
                    if cand[ck].is_some() && !disq[ck] {
                        load_cell.insert(i, *c);
                    }
                }
            }
        }
        let mut enum_nil_tests: Vec<(Idx<Inst>, Idx<Inst>, Idx<Cell>)> = Vec::new();
        let mut value_nil_tests: Vec<(Idx<Inst>, Idx<Inst>, Idx<Cell>)> = Vec::new();
        let mut chained_roots: HashSet<usize> = HashSet::default();
        for (_, b) in func.blocks.iter() {
            for &i in &b.insts {
                func.insts[i].uses_each(&mut |op| {
                    if let Some(&c) = load_cell.get(&op) {
                        let known = |f: &ustr::Ustr| {
                            cand_fields[rep[raw(c) as usize]]
                                .as_ref()
                                .is_some_and(|s| s.contains(f))
                        };
                        let ok = match &func.insts[i] {
                            Inst::Field { base, field } => *base == op && known(field),
                            Inst::StoreField { base, field, .. } => *base == op && known(field),
                            // An enum positional payload read: `v[index+1]`, named `_<index>`.
                            Inst::TupleIndex { base, index } => {
                                *base == op && known(&ustr::Ustr::from(&format!("_{index}")))
                            }
                            Inst::Eq(a, b) | Inst::NotEq(a, b)
                                if matches!(
                                    cand[rep[raw(c) as usize]],
                                    Some(StructId::Value(_))
                                ) =>
                            {
                                let other = if *a == op { *b } else { *a };
                                let is_nil = (*a == op) != (*b == op)
                                    && matches!(func.insts[other], Inst::Const(Lit::Nil));
                                if is_nil {
                                    value_nil_tests.push((i, op, c));
                                }
                                is_nil
                            }
                            Inst::Eq(a, b)
                                if matches!(
                                    cand[rep[raw(c) as usize]],
                                    Some(StructId::Enum(_))
                                ) =>
                            {
                                let other = if *a == op {
                                    Some(*b)
                                } else if *b == op {
                                    Some(*a)
                                } else {
                                    None
                                };
                                let is_nil = other.is_some_and(|v| {
                                    matches!(func.insts[v], Inst::Const(Lit::Nil))
                                });
                                if is_nil {
                                    enum_nil_tests.push((i, op, c));
                                }
                                is_nil
                            }
                            // The alias store itself: `Store(y, Load(x))` where y and x
                            // unify to the same root (deleted by the rewrite), or where
                            // y copies x's leaves (expanded to leaf-wise stores). A copy
                            // of a copy (`let t = h.t ?? return` feeding a loop body's
                            // binding) reads its immediate source's leaves.
                            Inst::Store { cell, value } => {
                                let yk = raw(*cell) as usize;
                                let accepted = *value == op
                                    && alias[yk].is_some()
                                    && !disq[yk]
                                    && (rep[yk] == rep[raw(c) as usize]
                                        || (copy_alias[yk].is_some() && alias[yk] == Some(c)));
                                if accepted && copy_alias[yk].is_some_and(|r| r != c) {
                                    chained_roots.insert(raw(copy_alias[yk].unwrap()) as usize);
                                }
                                accepted
                            }
                            _ => false,
                        };
                        if !ok {
                            // A flagged (non-null value-struct) binding rebuilds at the
                            // use instead of pinning the family as an escaping aggregate.
                            if func.cells[c].nonnull_value_struct {
                                escape_loads.push((op, c));
                            } else {
                                disq[raw(c) as usize] = true;
                            }
                        }
                    }
                });
            }
            b.term.uses_each(&mut |op| {
                if let Some(&c) = load_cell.get(&op) {
                    if func.cells[c].nonnull_value_struct {
                        escape_loads.push((op, c)); // rebuild at the terminator use
                    } else {
                        disq[raw(c) as usize] = true; // a load used by a terminator escapes
                    }
                }
            });
        }
        LoadUses {
            load_cell,
            escape_loads,
            enum_nil_tests,
            value_nil_tests,
            chained_roots,
        }
    }

    /// The family root a cell's leaves live under: its copy source, else its
    /// representative.
    fn leaf_root(&self, k: usize) -> usize {
        self.copy_alias[k].map_or(self.rep[k], |r| raw(r) as usize)
    }

    fn settle_disqualification(
        &mut self,
        escape_loads: &[(Idx<Inst>, Idx<Cell>)],
        chained_roots: &HashSet<usize>,
    ) {
        // A chained family that also escapes would rebuild its value at every
        // escaping use, which can cost more than the one copy it replaces.
        for (_, c) in escape_loads {
            let root = self.leaf_root(raw(*c) as usize);
            if chained_roots.contains(&root) {
                self.disq[root] = true;
            }
        }

        let Families {
            nc,
            cand,
            disq,
            rep,
            copy_alias,
            ..
        } = self;
        // Disqualification is family-wide: an escaping alias member keeps the
        // whole object (root + every member) as an aggregate, and a dead root
        // takes its members with it. Two passes settle it (rep is one level deep).
        for _ in 0..2 {
            for k in 0..*nc {
                if rep[k] != k {
                    if disq[k] {
                        disq[rep[k]] = true;
                    }
                    if disq[rep[k]] || cand[rep[k]].is_none() {
                        disq[k] = true;
                    }
                }
                // A copy member reads the root's leaf cells, so a whole root takes
                // the member with it -- and a whole member's surviving `Store(k,
                // Load(root))` would read the root's now-uninit aggregate, so it
                // takes the root too.
                if let Some(root) = copy_alias[k] {
                    let rk = raw(root) as usize;
                    if disq[k] {
                        disq[rk] = true;
                    }
                    if disq[rk] || cand[rk].is_none() {
                        disq[k] = true;
                    }
                }
            }
        }
    }

    /// A family whose nullable value is nil-tested carries a presence leaf,
    /// written by every store: a constant on a ctor, nil on a nil store. Copy
    /// members read the root's leaves, so the whole family gets it.
    fn add_presence_leaves(&mut self, value_nil_tests: &[(Idx<Inst>, Idx<Inst>, Idx<Cell>)]) {
        let present_field = present_field();
        for (_, _, c) in value_nil_tests {
            let root = self.leaf_root(raw(*c) as usize);
            self.presence_roots.insert(root);
        }
        for k in 0..self.nc {
            let root = self.leaf_root(k);
            if self.presence_roots.contains(&root) {
                if let Some(fields) = self.cand_fields[k].as_mut() {
                    fields.insert(present_field);
                }
            }
        }
    }

    fn needs_presence(&self, c: Idx<Cell>) -> bool {
        let k = raw(c) as usize;
        k < self.nc && self.presence_roots.contains(&self.leaf_root(k))
    }

    // `k < nc` guards the field cells `get_field_cell` allocates during the
    // rewrite (a `StoreField`->`Store` rewrite emits a `Store` to a fresh field
    // cell that the later `store_expansions` scan sees): they are never candidates
    // this round, and indexing the round's `nc`-sized arrays with them would panic.
    fn is_cand(&self, c: Idx<Cell>) -> bool {
        let k = raw(c) as usize;
        k < self.nc && self.cand[k].is_some() && !self.disq[k] && self.has_store[k]
    }

    fn any_candidate(&self) -> bool {
        (0..self.nc).any(|k| self.cand[k].is_some() && !self.disq[k] && self.has_store[k])
    }

    // Alias members resolve through `rep` FIRST, so a family shares one set of
    // field cells (by-ref mutation through any member lands in the shared cell).
    fn to_rep(&self, c: Idx<Cell>) -> Idx<Cell> {
        let k = raw(c) as usize;
        if k < self.nc && self.rep[k] != k {
            Idx::from_raw(RawIdx::from_u32(self.rep[k] as u32))
        } else {
            c
        }
    }

    /// Only a field the CTOR actually provides has a cell to redirect to.
    fn provides(&self, c: Idx<Cell>, f: &ustr::Ustr) -> bool {
        self.cand_fields[self.rep[raw(c) as usize]]
            .as_ref()
            .is_some_and(|s| s.contains(f))
    }

    fn candidate_fields(&self, c: Idx<Cell>) -> Result<Vec<ustr::Ustr>, CompilerError> {
        let k = raw(c) as usize;
        let root = *self.rep.get(k).ok_or_else(|| {
            CompilerError::new_no_location("SROA candidate cell is outside its analysis tables")
        })?;
        let fields = self
            .cand_fields
            .get(root)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                CompilerError::new_no_location(format!(
                    "SROA candidate cell {} has no planned physical field set",
                    k
                ))
            })?;
        Ok(fields.iter().copied().collect())
    }
}

/// Per-field cells, allocated on demand: (cell, field name) -> field cell.
#[derive(Default)]
struct FieldCells(HashMap<(Idx<Cell>, ustr::Ustr), Idx<Cell>>);

impl FieldCells {
    fn get(&mut self, func: &mut MirFunc, c: Idx<Cell>, f: ustr::Ustr) -> Idx<Cell> {
        *self.0.entry((c, f)).or_insert_with(|| {
            let base = func.cells[c].name.map(|n| n.as_str().to_string());
            let name = base.map(|b| ustr::Ustr::from(&format!("{b}__{f}")));
            let capture_identity = func.cells[c]
                .capture_identity
                .as_ref()
                .map(|identity| identity.projected(f));
            let cell = func.new_cell(name);
            func.cells[cell].capture_identity = capture_identity;
            func.cells[cell].producer = CellProducer::Sroa;
            cell
        })
    }
}

/// Answer each recorded nil test from a field cell: an enum's `__ty` (every
/// enum ctor writes a nonnil one; exhaustion nils every lane) or a value
/// family's presence leaf. The new load is spliced right before its test.
fn rewrite_nil_tests(
    func: &mut MirFunc,
    fam: &Families,
    cells: &mut FieldCells,
    enum_nil_tests: Vec<(Idx<Inst>, Idx<Inst>, Idx<Cell>)>,
    value_nil_tests: Vec<(Idx<Inst>, Idx<Inst>, Idx<Cell>)>,
) {
    let mut nil_test_loads: HashMap<Idx<Inst>, Idx<Inst>> = HashMap::default();
    for (test, old, c) in enum_nil_tests {
        if !fam.is_cand(c) {
            continue;
        }
        // Every enum ctor writes a nonnil `__ty`; exhaustion nils every lane.
        let type_cell = cells.get(func, fam.to_rep(c), ustr::Ustr::from("__ty"));
        let present = func.alloc_inst(Inst::Load(type_cell));
        match &mut func.insts[test] {
            Inst::Eq(a, b) => {
                if *a == old {
                    *a = present;
                } else if *b == old {
                    *b = present;
                }
            }
            _ => unreachable!("recorded enum nil test changed instruction kind"),
        }
        nil_test_loads.insert(test, present);
    }
    for (test, old, c) in value_nil_tests {
        if !fam.is_cand(c) {
            continue;
        }
        let present_cell = cells.get(func, fam.to_rep(c), present_field());
        let present = func.alloc_inst(Inst::Load(present_cell));
        match &mut func.insts[test] {
            Inst::Eq(a, b) | Inst::NotEq(a, b) => {
                if *a == old {
                    *a = present;
                } else if *b == old {
                    *b = present;
                }
            }
            _ => unreachable!("recorded value nil test changed instruction kind"),
        }
        nil_test_loads.insert(test, present);
    }
    if !nil_test_loads.is_empty() {
        for (_, block) in func.blocks.iter_mut() {
            if !block.insts.iter().any(|i| nil_test_loads.contains_key(i)) {
                continue;
            }
            let old = std::mem::take(&mut block.insts);
            let mut new = Vec::with_capacity(old.len() + nil_test_loads.len());
            for i in old {
                if let Some(&load) = nil_test_loads.get(&i) {
                    new.push(load);
                }
                new.push(i);
            }
            block.insts = new;
        }
    }
}

/// Rewrite `Field { base: load(cand), field }` in place into `Load(field_cell)`,
/// and `StoreField { base: load(cand), field, value }` into `Store(field_cell,
/// value)`. Re-check `is_cand`: `load_cell` is not pruned as cells get
/// disqualified mid-analysis, so a cell escaping via a non-projection use (e.g.
/// whole-struct `==`) keeps its aggregate, and its field accesses must stay on
/// it -- rewriting them without expanding the store gives an uninit cell.
/// Only a field the CTOR actually provides has a cell to redirect to. A
/// nested value member is stored under its dotted LEAF names (`params.x`,
/// `params.y`), so the immediate `.params` read names no cell, and minting
/// one gives a load nothing ever stores -- the uninit-cell failure the
/// comment above describes, reached the other way. Such a read stays a
/// `Field` on the aggregate, which the `escape_loads` rebuild below
/// reconstitutes at exactly this load.
fn rewrite_projections(
    func: &mut MirFunc,
    fam: &Families,
    cells: &mut FieldCells,
    load_cell: &HashMap<Idx<Inst>, Idx<Cell>>,
) {
    let field_rewrites: Vec<(Idx<Inst>, Idx<Cell>, ustr::Ustr)> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter_map(|i| match &func.insts[i] {
            Inst::Field { base, field } => load_cell
                .get(base)
                .copied()
                .filter(|&c| fam.is_cand(c) && fam.provides(c, field))
                .map(|c| (i, c, *field)),
            _ => None,
        })
        .collect();
    for (i, c, field) in field_rewrites {
        let fc = cells.get(func, fam.to_rep(c), field);
        func.insts[i] = Inst::Load(fc);
    }
    // Same for an enum positional payload read `TupleIndex { base, index }` ->
    // `Load(field_cell "_<index>")` (the `match`-with-binding payload extract).
    let tuple_rewrites: Vec<(Idx<Inst>, Idx<Cell>, ustr::Ustr)> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter_map(|i| match &func.insts[i] {
            Inst::TupleIndex { base, index } => load_cell
                .get(base)
                .copied()
                .filter(|&c| fam.is_cand(c))
                .map(|c| (i, c, ustr::Ustr::from(&format!("_{index}")))),
            _ => None,
        })
        .collect();
    for (i, c, field) in tuple_rewrites {
        let fc = cells.get(func, fam.to_rep(c), field);
        func.insts[i] = Inst::Load(fc);
    }
    let storefield_rewrites: Vec<(Idx<Inst>, Idx<Cell>, ustr::Ustr, Idx<Inst>)> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter_map(|i| match &func.insts[i] {
            Inst::StoreField { base, field, value } => load_cell
                .get(base)
                .copied()
                .filter(|&c| fam.is_cand(c))
                .map(|c| (i, c, *field, *value)),
            _ => None,
        })
        .collect();
    for (i, c, field, value) in storefield_rewrites {
        let fc = cells.get(func, fam.to_rep(c), field);
        func.insts[i] = Inst::Store { cell: fc, value };
    }
}

/// One candidate store and the per-field sources that replace it.
type StoreExpansion = (Idx<Inst>, Idx<Cell>, Vec<(ustr::Ustr, FieldSrc)>);

/// Plan the expansion of each `Store { cand, ctor }` into per-field stores.
/// Collect first (immutable read of the ctor fields); [`expand_stores`] then
/// emits them.
fn plan_store_expansions(
    func: &MirFunc,
    fam: &Families,
) -> Result<Vec<StoreExpansion>, CompilerError> {
    Ok(func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .map(|i| -> Result<Option<StoreExpansion>, CompilerError> {
            match &func.insts[i] {
                Inst::Store { cell, value } if fam.is_cand(*cell) => {
                    // A nil store writes nil to EVERY field cell (sorted for
                    // deterministic emission), reusing the nil value inst.
                    if matches!(&func.insts[*value], Inst::Const(Lit::Nil)) {
                        let mut fs = fam.candidate_fields(*cell)?;
                        fs.sort_unstable();
                        return Ok(Some((
                            i,
                            *cell,
                            fs.into_iter()
                                .map(|f| (f, FieldSrc::Operand(*value)))
                                .collect(),
                        )));
                    }
                    // A value-struct alias member copies its source's leaves.
                    if let (Inst::Load(src), Some(_)) =
                        (&func.insts[*value], fam.copy_alias[raw(*cell) as usize])
                    {
                        let src = fam.to_rep(*src);
                        let mut fs = fam.candidate_fields(*cell)?;
                        fs.sort_unstable();
                        return Ok(Some((
                            i,
                            *cell,
                            fs.into_iter()
                                .map(|f| (f, FieldSrc::CellLoad(src)))
                                .collect(),
                        )));
                    }
                    let Some((_, mut fields)) =
                        ctor_fields(&func.insts[*value], fam.cell_enum[raw(*cell) as usize])
                    else {
                        return Ok(None);
                    };
                    if fam.needs_presence(*cell) {
                        fields.push((present_field(), FieldSrc::ConstTrue));
                    }
                    // An omitted box key reads nil, even after a previous variant
                    // populated the scalar cell in a loop or coroutine frame.
                    let mut missing = fam.candidate_fields(*cell)?;
                    missing.retain(|f| !fields.iter().any(|(name, _)| name == f));
                    missing.sort_unstable();
                    fields.extend(missing.into_iter().map(|f| (f, FieldSrc::ConstNil)));
                    Ok(Some((i, *cell, fields)))
                }
                _ => Ok(None),
            }
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect())
}

/// Accepted alias stores (`Store(member, Load(..))`) DELETE: the member's
/// accesses were rewritten onto the root's field cells above, so the
/// whole-cell copy has nothing left to do (and keeping it would resurrect
/// the aggregate). Returns the replacement map seeded with those deletions:
/// old store inst -> the insts that replace it (in order).
fn alias_deletions(func: &MirFunc, fam: &Families) -> HashMap<Idx<Inst>, Vec<Idx<Inst>>> {
    let alias_deletions: Vec<Idx<Inst>> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter(|&i| match &func.insts[i] {
            Inst::Store { cell, value } => {
                let ck = raw(*cell) as usize;
                ck < fam.nc
                    && fam.alias[ck].is_some()
                    && fam.copy_alias[ck].is_none()
                    && !fam.disq[ck]
                    && matches!(&func.insts[*value], Inst::Load(_))
            }
            _ => false,
        })
        .collect();
    let mut replacement: HashMap<Idx<Inst>, Vec<Idx<Inst>>> = HashMap::default();
    for i in alias_deletions {
        replacement.insert(i, Vec::new());
    }
    replacement
}

/// Emit the planned per-field stores. An enum's `__variant`/`__ty` are
/// `ConstStr` (baked literals, not operands), so materialize the `Const(Str)`
/// inst here before storing it to the field cell.
fn expand_stores(
    func: &mut MirFunc,
    cells: &mut FieldCells,
    store_expansions: Vec<StoreExpansion>,
    replacement: &mut HashMap<Idx<Inst>, Vec<Idx<Inst>>>,
) {
    // A field cell that ALWAYS holds a value-struct ctor inherits the parent's
    // non-null licence, so a later round can split it in turn instead of
    // disqualifying its family when the field escapes whole (`Outer { ..o }`
    // rebuilds `o.inner` as one operand). Nullable fields are excluded by
    // construction: a nil store expands to `Const(Nil)` per field, which is not
    // a ctor -- the licence `nonnull_value_struct` grants is exactly "this cell
    // can be rebuilt from its leaves", and nil cannot be.
    let mut nn_field: HashMap<(Idx<Cell>, ustr::Ustr), bool> = HashMap::default();
    for (_, c, fields) in &store_expansions {
        if !func.cells[*c].nonnull_value_struct {
            continue;
        }
        for (fname, src) in fields {
            let is_value_ctor = matches!(src,
                FieldSrc::Operand(v) if matches!(&func.insts[*v], Inst::MakeValueStruct { .. }));
            *nn_field.entry((*c, *fname)).or_insert(true) &= is_value_ctor;
        }
    }
    for (store_i, c, fields) in store_expansions {
        let mut news = Vec::with_capacity(fields.len());
        for (fname, src) in fields {
            let fval = match src {
                FieldSrc::Operand(v) => v,
                // A materialized const is a NEW value-producing inst: it must be
                // spliced into the block (in `news`) BEFORE the store that reads it,
                // or it floats free of any block and regalloc/codegen never emit it.
                FieldSrc::ConstStr(s) => {
                    let cv = func.alloc_inst(Inst::Const(Lit::Str(s)));
                    news.push(cv);
                    cv
                }
                FieldSrc::ConstTrue => {
                    let cv = func.alloc_inst(Inst::Const(Lit::Bool(true)));
                    news.push(cv);
                    cv
                }
                FieldSrc::ConstNil => {
                    let cv = func.alloc_inst(Inst::Const(Lit::Nil));
                    news.push(cv);
                    cv
                }
                FieldSrc::CellLoad(root) => {
                    let rc = cells.get(func, root, fname);
                    let lv = func.alloc_inst(Inst::Load(rc));
                    news.push(lv);
                    lv
                }
            };
            let fc = cells.get(func, c, fname);
            if nn_field.get(&(c, fname)).copied().unwrap_or(false) {
                func.mark_nonnull_value_struct(fc);
            }
            news.push(func.alloc_inst(Inst::Store {
                cell: fc,
                value: fval,
            }));
        }
        replacement.insert(store_i, news);
    }
}

/// Rematerialize each escaping value-struct load L in place: `Load(cell)` ->
/// `MakeValueStruct{name, [(field, Load(field_cell))..]}`, with the field loads
/// spliced before it. Reads the same field cells the ctor store now writes;
/// a load only reached on a non-materializing path (an arena-scan MISS) runs
/// no ctor. The escaping cell is flagged non-null, so its family root is a
/// value ctor candidate carrying `value_order` -- else the family stayed whole
/// and the load is untouched (a boxed read of the intact aggregate).
fn rematerialize_escapes(
    func: &mut MirFunc,
    fam: &Families,
    cells: &mut FieldCells,
    mut escape_loads: Vec<(Idx<Inst>, Idx<Cell>)>,
    replacement: &mut HashMap<Idx<Inst>, Vec<Idx<Inst>>>,
) {
    escape_loads.sort_by_key(|(l, _)| raw(*l));
    escape_loads.dedup_by_key(|(l, _)| *l);
    for (l, c) in escape_loads {
        if !fam.is_cand(c) {
            continue;
        }
        let root = fam.to_rep(c);
        let Some(StructId::Value(name)) = fam.cand[raw(root) as usize] else {
            continue;
        };
        let Some((order, representation, form)) =
            fam.value_order.get(&(raw(root) as usize)).cloned()
        else {
            continue;
        };
        let mut news = Vec::with_capacity(order.len() + 1);
        let mut mvs_fields = Vec::with_capacity(order.len());
        for fname in order {
            let fc = cells.get(func, root, fname);
            let fl = func.alloc_inst(Inst::Load(fc));
            news.push(fl);
            mvs_fields.push((fname, fl));
        }
        func.insts[l] = Inst::MakeValueStruct {
            name,
            representation,
            form,
            fields: mvs_fields,
        };
        news.push(l);
        replacement.insert(l, news);
    }
}

/// Splice each replaced inst's replacement list into its block.
fn splice_replacements(func: &mut MirFunc, replacement: &HashMap<Idx<Inst>, Vec<Idx<Inst>>>) {
    if replacement.is_empty() {
        return;
    }
    let block_ids: Vec<Idx<Block>> = func.blocks.iter().map(|(id, _)| id).collect();
    for bid in block_ids {
        if !func.blocks[bid]
            .insts
            .iter()
            .any(|i| replacement.contains_key(i))
        {
            continue;
        }
        let old = std::mem::take(&mut func.blocks[bid].insts);
        let mut new = Vec::with_capacity(old.len());
        for i in old {
            match replacement.get(&i) {
                Some(news) => new.extend_from_slice(news),
                None => new.push(i),
            }
        }
        func.blocks[bid].insts = new;
    }
}
