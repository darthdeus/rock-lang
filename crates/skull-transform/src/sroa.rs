use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::scope;
use crate::visit::{raw, splice_replacements, top_level_blocks};
use bone_core::prelude::CompilerError;
use comfy_la_arena::{Idx, RawIdx};
use skull_mir::mir::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum StructId {
    /// `MakeValueStruct` -- an immutable `value` cdata.
    Value(ustr::Ustr),
    /// `MakeStruct` -- a mutable plain-struct table (`ty` is the `__ty` nominal,
    /// `None` for a foreign nominal).
    Plain(Option<ustr::Ustr>),
    /// One enum family shares the union of its variants' fields.
    Enum(ustr::Ustr),
    /// `MakeTuple` -- a positional table (`{v0, v1, ..}`), keyed by width. Fields
    /// are `_<i>`, the same naming `TupleIndex` reads already rewrite through.
    Tuple(usize),
}

#[derive(Clone, Copy)]
enum FieldSrc {
    Operand(Idx<Inst>),
    ConstStr(ustr::Ustr),
    /// Leaf-wise copy from another family member's field cell (a value-struct
    /// alias member, see `copy_alias`) -- materializes a `Load` at the store site.
    CellLoad(Idx<Cell>),
    /// A presence leaf represents nullable value cells.
    ConstTrue,
    ConstNil,
}

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
                .map(|(i, v)| (skull_mir::names::indexed("_", "", i), FieldSrc::Operand(*v)))
                .collect(),
        )),
        Inst::MakeEnum {
            tag,
            ty,
            positional,
            named,
        } => {
            let mut f = vec![
                (
                    skull_mir::names::ustr("__variant"),
                    FieldSrc::ConstStr(*tag),
                ),
                (
                    skull_mir::names::ustr("__ty"),
                    FieldSrc::ConstStr(skull_mir::mangle::symbol_of_key(*ty)),
                ),
            ];
            for (i, p) in positional.iter().enumerate() {
                f.push((skull_mir::names::indexed("_", "", i), FieldSrc::Operand(*p)));
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
                (skull_mir::names::ustr("__variant"), FieldSrc::Operand(*tag)),
                (
                    skull_mir::names::ustr("__ty"),
                    FieldSrc::ConstStr(skull_mir::mangle::symbol_of_key(*ty)),
                ),
            ];
            for (i, p) in positional.iter().enumerate() {
                f.push((skull_mir::names::indexed("_", "", i), FieldSrc::Operand(*p)));
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
                        skull_mir::names::ustr("__variant"),
                        FieldSrc::ConstStr(skull_mir::names::ustr(variant)),
                    ),
                    (skull_mir::names::ustr("__ty"), FieldSrc::ConstStr(symbol)),
                ],
            ))
        }
        _ => None,
    }
}

pub(crate) fn scalarize_struct_cells_once(func: &mut MirFunc) -> Result<bool, CompilerError> {
    let mut fam = Families::collect(func);
    fam.resolve_aliases();
    fam.split_copy_aliases(func);
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

fn present_field() -> ustr::Ustr {
    skull_mir::names::ustr("__present")
}

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

/// These tables cover cells present before the rewrite starts.
struct Families {
    nc: usize,
    // A projection may split only to a field present in the constructor family.
    cand: Vec<Option<StructId>>,
    cand_fields: Vec<Option<HashSet<ustr::Ustr>>>,
    disq: Vec<bool>,
    cell_enum: Vec<Option<ustr::Ustr>>,
    // A mutable record alias shares its root's field cells.
    alias: Vec<Option<Idx<Cell>>>,
    // Only a known non-null value can be rebuilt from its scalar fields when it escapes.
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

fn store_inputs(func: &MirFunc) -> (Vec<bool>, Vec<Option<ustr::Ustr>>, Vec<u32>) {
    let mut disqualified: Vec<_> = func.cells.values().map(|cell| cell.borrowed).collect();
    for &cell in func.params.iter().chain(func.resume_arg.iter()) {
        disqualified[raw(cell)] = true;
    }
    let mut enum_hints = vec![None; func.cells.len()];
    let mut uses = vec![0u32; func.insts.len()];
    for (_, block) in func.blocks.iter() {
        for &i in &block.insts {
            let inst = &func.insts[i];
            inst.uses_each(&mut |op| uses[raw(op)] += 1);
            match inst {
                Inst::Store { cell, value } => {
                    // A unit-variant singleton needs the enum name from a payload store.
                    if let Inst::MakeEnum { ty, .. } | Inst::MakeEnumDyn { ty, .. } =
                        func.insts[*value]
                    {
                        enum_hints[raw(*cell)].get_or_insert(ty);
                    }
                }
                Inst::Lambda { params, .. } => {
                    for &param in params {
                        disqualified[raw(param)] = true;
                    }
                }
                _ => inst.cell_writes_each(&mut |cell| disqualified[raw(cell)] = true),
            }
        }
        block.term.uses_each(&mut |op| uses[raw(op)] += 1);
    }
    (disqualified, enum_hints, uses)
}

impl Families {
    /// Candidate collection: classify every store as a ctor, an alias copy, a
    /// nil store, or a disqualifying write.
    fn collect(func: &MirFunc) -> Self {
        let nc = func.cells.len();
        let mut value_order: HashMap<usize, ValueOrder> = HashMap::default();
        let mut cand: Vec<Option<StructId>> = vec![None; nc];
        let mut cand_fields: Vec<Option<HashSet<ustr::Ustr>>> = vec![None; nc];
        let (mut disq, cell_enum, vuse) = store_inputs(func);
        // Only the main CFG -- a cell touched inside a closure body is opaque (the
        // closure may run on any call), so leave it whole.
        let mut alias: Vec<Option<Idx<Cell>>> = vec![None; nc];
        let main: HashSet<Idx<Block>> = top_level_blocks(func).into_iter().collect();
        for (bid, b) in func.blocks.iter() {
            let in_main = main.contains(&bid);
            for &i in &b.insts {
                if let Inst::Store { cell, value } = &func.insts[i] {
                    let ck = raw(*cell);
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
                    // Nil is compatible with any aggregate shape.
                    if matches!(&func.insts[*value], Inst::Const(Lit::Nil)) {
                        continue;
                    }
                    match ctor_fields(&func.insts[*value], cell_enum[ck]) {
                        Some((id, fields)) => {
                            if cand[ck].is_some_and(|n| n != id) || vuse[raw(*value)] != 1 {
                                disq[ck] = true;
                            } else {
                                cand[ck] = Some(id);
                                if let Inst::MakeValueStruct {
                                    representation,
                                    form,
                                    ..
                                } = func.insts[*value]
                                {
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
            cell_enum,
            alias,
            value_order,
            rep: (0..nc).collect(),
            copy_alias: vec![None; nc],
            presence_roots: HashSet::default(),
        }
    }

    /// Cycles and chains without a constructor remain whole.
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
                        cur = raw(o);
                        steps += 1;
                        // An intermediate assigned from several roots cannot share any one of them.
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

    /// Value aliases and captured enum aliases get their own field cells.
    /// Otherwise a later write to the root could change the captured snapshot.
    fn split_copy_aliases(&mut self, func: &MirFunc) {
        let captured = std::cell::OnceCell::new();
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
            if rep[k] == k || disq[k] {
                continue;
            }
            let cell = Idx::from_raw(RawIdx::from_u32(k as u32));
            let copies_binding = matches!(cand[rep[k]], Some(StructId::Value(_)))
                || (matches!(cand[rep[k]], Some(StructId::Enum(_)))
                    && captured
                        .get_or_init(|| captured_cells(func))
                        .contains(&cell));
            if !copies_binding {
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

    /// A candidate load must have only projections or supported escape uses.
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
                    let ck = raw(*c);
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
                            cand_fields[rep[raw(c)]]
                                .as_ref()
                                .is_some_and(|s| s.contains(f))
                        };
                        let ok = match &func.insts[i] {
                            Inst::Field { .. }
                            | Inst::Index {
                                kind: ContainerKind::Record,
                                ..
                            } => field_read(func, i, op).is_some_and(|(_, field)| known(&field)),
                            Inst::StoreField { .. }
                            | Inst::StoreIndex {
                                kind: ContainerKind::Record,
                                ..
                            } => {
                                field_store(func, i, op).is_some_and(|(_, field, _)| known(&field))
                            }
                            // An enum positional payload read: `v[index+1]`, named `_<index>`.
                            Inst::TupleIndex { base, index } => {
                                *base == op && known(&skull_mir::names::indexed("_", "", *index))
                            }
                            Inst::Eq(a, b) | Inst::NotEq(a, b)
                                if matches!(cand[rep[raw(c)]], Some(StructId::Value(_))) =>
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
                                if matches!(cand[rep[raw(c)]], Some(StructId::Enum(_))) =>
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
                            Inst::Store { cell, value } => {
                                let yk = raw(*cell);
                                let accepted = *value == op
                                    && alias[yk].is_some()
                                    && !disq[yk]
                                    && (rep[yk] == rep[raw(c)]
                                        || (copy_alias[yk].is_some() && alias[yk] == Some(c)));
                                if accepted && copy_alias[yk].is_some_and(|r| r != c) {
                                    chained_roots.insert(raw(copy_alias[yk].unwrap()));
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
                                disq[raw(c)] = true;
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
                        disq[raw(c)] = true; // a load used by a terminator escapes
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
        self.copy_alias[k].map_or(self.rep[k], raw)
    }

    fn settle_disqualification(
        &mut self,
        escape_loads: &[(Idx<Inst>, Idx<Cell>)],
        chained_roots: &HashSet<usize>,
    ) {
        // A chained family that also escapes would rebuild its value at every
        // escaping use, which can cost more than the one copy it replaces.
        for (_, c) in escape_loads {
            let root = self.leaf_root(raw(*c));
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
                // An alias copy needs the source leaf cells to exist.
                if let Some(root) = copy_alias[k] {
                    let rk = raw(root);
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

    fn add_presence_leaves(&mut self, value_nil_tests: &[(Idx<Inst>, Idx<Inst>, Idx<Cell>)]) {
        let present_field = present_field();
        for (_, _, c) in value_nil_tests {
            let root = self.leaf_root(raw(*c));
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
        let k = raw(c);
        k < self.nc && self.presence_roots.contains(&self.leaf_root(k))
    }

    // Field cells allocated while rewriting cannot be candidates this round.
    fn is_cand(&self, c: Idx<Cell>) -> bool {
        let k = raw(c);
        k < self.nc && self.cand[k].is_some() && !self.disq[k]
    }

    fn any_candidate(&self) -> bool {
        (0..self.nc).any(|k| self.cand[k].is_some() && !self.disq[k])
    }

    // Alias members resolve through `rep` FIRST, so a family shares one set of
    // field cells (by-ref mutation through any member lands in the shared cell).
    fn to_rep(&self, c: Idx<Cell>) -> Idx<Cell> {
        let k = raw(c);
        if k < self.nc && self.rep[k] != k {
            Idx::from_raw(RawIdx::from_u32(self.rep[k] as u32))
        } else {
            c
        }
    }

    /// Only a field the CTOR actually provides has a cell to redirect to.
    fn provides(&self, c: Idx<Cell>, f: &ustr::Ustr) -> bool {
        self.cand_fields[self.rep[raw(c)]]
            .as_ref()
            .is_some_and(|s| s.contains(f))
    }

    fn candidate_fields(&self, c: Idx<Cell>) -> Result<Vec<ustr::Ustr>, CompilerError> {
        let k = raw(c);
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
            let name = func.cells[c]
                .name
                .map(|base| skull_mir::names::concat(&[base.as_str(), "__", f.as_str()]));
            let capture_identity = func.cells[c]
                .capture_identity
                .as_ref()
                .map(|identity| identity.projected(f));
            let cell = func.new_cell(name);
            func.cells[cell].capture_identity = capture_identity;
            cell
        })
    }
}

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
        let type_cell = cells.get(func, fam.to_rep(c), skull_mir::names::ustr("__ty"));
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

/// Normalize struct-field operations. Heap records lowered as `Index`/
/// `StoreIndex` are equivalent only when the index is a constant string.
fn field_read(func: &MirFunc, i: Idx<Inst>, load: Idx<Inst>) -> Option<(Idx<Inst>, ustr::Ustr)> {
    let (base, field) = func.field_projection(i)?;
    (base == load && uses_once(func, i, load)).then_some((base, field))
}

/// As above for writes. Requiring exactly one occurrence of the candidate load
/// as an operand prevents a self-store (`base == value`) from being mistaken
/// for a base-only projection during escape analysis and rewriting.
fn field_store(
    func: &MirFunc,
    i: Idx<Inst>,
    load: Idx<Inst>,
) -> Option<(Idx<Inst>, ustr::Ustr, Idx<Inst>)> {
    let (base, field, value) = match &func.insts[i] {
        Inst::StoreField { base, field, value } => (*base, *field, *value),
        Inst::StoreIndex {
            base,
            index,
            value,
            kind: ContainerKind::Record,
        } => {
            let Inst::Const(Lit::Str(field)) = &func.insts[*index] else {
                return None;
            };
            (*base, *field, *value)
        }
        _ => return None,
    };
    (base == load && uses_once(func, i, load)).then_some((base, field, value))
}

fn uses_once(func: &MirFunc, i: Idx<Inst>, value: Idx<Inst>) -> bool {
    let mut count = 0;
    func.insts[i].uses_each(&mut |op| {
        if op == value {
            count += 1;
        }
    });
    count == 1
}

/// Only declared fields can get field cells. A dotted leaf layout may have
/// no cell for a whole nested member; let its escaping load rebuild the value.
fn rewrite_projections(
    func: &mut MirFunc,
    fam: &Families,
    cells: &mut FieldCells,
    load_cell: &HashMap<Idx<Inst>, Idx<Cell>>,
) {
    let mut fields = Vec::new();
    let mut tuples = Vec::new();
    let mut stores = Vec::new();
    for (_, block) in func.blocks.iter() {
        for &i in &block.insts {
            match func.insts[i] {
                Inst::Field { base, .. }
                | Inst::Index {
                    base,
                    kind: ContainerKind::Record,
                    ..
                } => {
                    if let Some(&c) = load_cell.get(&base)
                        && fam.is_cand(c)
                        && let Some((_, field)) = field_read(func, i, base)
                        && fam.provides(c, &field)
                    {
                        fields.push((i, c, field));
                    }
                }
                Inst::TupleIndex { base, index } => {
                    if let Some(&c) = load_cell.get(&base)
                        && fam.is_cand(c)
                    {
                        tuples.push((i, c, skull_mir::names::indexed("_", "", index)));
                    }
                }
                Inst::StoreField { base, .. }
                | Inst::StoreIndex {
                    base,
                    kind: ContainerKind::Record,
                    ..
                } => {
                    if let Some(&c) = load_cell.get(&base)
                        && fam.is_cand(c)
                        && let Some((_, field, value)) = field_store(func, i, base)
                    {
                        stores.push((i, c, field, value));
                    }
                }
                _ => {}
            }
        }
    }
    // Keep field-cell numbering stable across the three projection classes.
    for (i, c, field) in fields.into_iter().chain(tuples) {
        let cell = cells.get(func, fam.to_rep(c), field);
        func.insts[i] = Inst::Load(cell);
    }
    for (i, c, field, value) in stores {
        let cell = cells.get(func, fam.to_rep(c), field);
        func.insts[i] = Inst::Store { cell, value };
    }
}

/// One candidate store and the per-field sources that replace it.
type StoreExpansion = (Idx<Inst>, Idx<Cell>, Vec<(ustr::Ustr, FieldSrc)>);

fn plan_store_expansions(
    func: &MirFunc,
    fam: &Families,
) -> Result<Vec<StoreExpansion>, CompilerError> {
    let mut expansions = Vec::new();
    for i in func
        .blocks
        .values()
        .flat_map(|block| block.insts.iter().copied())
    {
        let Inst::Store { cell, value } = func.insts[i] else {
            continue;
        };
        if !fam.is_cand(cell) {
            continue;
        }
        let src = match func.insts[value] {
            Inst::Const(Lit::Nil) => Some(FieldSrc::Operand(value)),
            Inst::Load(src) if fam.copy_alias[raw(cell)].is_some() => {
                Some(FieldSrc::CellLoad(fam.to_rep(src)))
            }
            _ => None,
        };
        let fields = if let Some(src) = src {
            let mut names = fam.candidate_fields(cell)?;
            names.sort_unstable();
            names.into_iter().map(|field| (field, src)).collect()
        } else if let Some((_, mut fields)) =
            ctor_fields(&func.insts[value], fam.cell_enum[raw(cell)])
        {
            if fam.needs_presence(cell) {
                fields.push((present_field(), FieldSrc::ConstTrue));
            }
            // A missing key must read nil even when an earlier variant wrote it.
            let mut missing = fam.candidate_fields(cell)?;
            missing.retain(|f| !fields.iter().any(|(name, _)| name == f));
            missing.sort_unstable();
            fields.extend(missing.into_iter().map(|f| (f, FieldSrc::ConstNil)));
            fields
        } else {
            continue;
        };
        expansions.push((i, cell, fields));
    }
    Ok(expansions)
}

fn alias_deletions(func: &MirFunc, fam: &Families) -> HashMap<Idx<Inst>, Vec<Idx<Inst>>> {
    func.blocks
        .values()
        .flat_map(|block| block.insts.iter().copied())
        .filter_map(|i| {
            let Inst::Store { cell, value } = func.insts[i] else {
                return None;
            };
            let c = raw(cell);
            (c < fam.nc
                && fam.alias[c].is_some()
                && fam.copy_alias[c].is_none()
                && !fam.disq[c]
                && matches!(func.insts[value], Inst::Load(_)))
            .then_some((i, Vec::new()))
        })
        .collect()
}

fn expand_stores(
    func: &mut MirFunc,
    cells: &mut FieldCells,
    store_expansions: Vec<StoreExpansion>,
    replacement: &mut HashMap<Idx<Inst>, Vec<Idx<Inst>>>,
) {
    // A field of a non-null value inherits that guarantee only if every store is a value constructor.
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
        let Some(StructId::Value(name)) = fam.cand[raw(root)] else {
            continue;
        };
        let Some((order, representation, form)) = fam.value_order.get(&(raw(root))).cloned() else {
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
