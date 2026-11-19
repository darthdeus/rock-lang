//! Hoist lambda bodies and replace their implicit captures with environment fields.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::scope::{Ownership, ScopeId, ScopeTree, non_dominating_by_value_captures};
use comfy_la_arena::Idx;
use skull_mir::cfg::reachable_blocks_from;
use skull_mir::code_identity::{
    CaptureField, CaptureIdentity, CaptureMode, CaptureSchema, CodeCompatibility, CodeIdentity,
    FrameSchema, LambdaSiteId, LambdaTopologyFingerprint,
};
use skull_mir::mir::*;

use crate::visit::raw;
use ustr::Ustr;

/// Environment slots are local to the capture plan, so copies of one lambda
/// site have the same body even when injected into different MIR arena slots.
fn field_of(plan: &ClosureEnvPlan, c: Idx<Cell>) -> Ustr {
    let slot = plan
        .captures()
        .binary_search_by_key(&raw(c), |cell| raw(*cell))
        .expect("environment field belongs to its capture plan");
    Ustr::from(format!("c{slot}").as_str())
}

// Only compiler-owned unwind closures are eligible: their identity and return
// value never escape the cleanup stack. A mutable capture must keep its box so
// reassignment after registration is still observed when unwinding.
fn forwarded_defer_capture(
    mf: &MirFunc,
    scope: &crate::scope::Scope,
    captures: &[Idx<Cell>],
    boxed: &HashSet<Idx<Cell>>,
) -> Option<Idx<Cell>> {
    let [capture] = captures else { return None };
    if !scope.params.is_empty() || boxed.contains(capture) {
        return None;
    }
    let mut block = scope.entry;
    let mut visited = HashSet::default();
    let mut loaded = None;
    let mut called = false;
    loop {
        if !visited.insert(block) {
            return None;
        }
        for &id in &mf.blocks[block].insts {
            match &mf.insts[id] {
                Inst::Load(cell) if cell == capture && loaded.is_none() && !called => {
                    loaded = Some(id)
                }
                Inst::CallValue { callee, args }
                    if Some(*callee) == loaded && args.is_empty() && !called =>
                {
                    called = true
                }
                Inst::Const(Lit::Nil) => {}
                _ => return None,
            }
        }
        match &mf.blocks[block].term {
            Term::Goto(next) => block = *next,
            Term::Return(values) if values.is_empty() && called => return Some(*capture),
            _ => return None,
        }
    }
}

pub(crate) fn frame_field(slot: u32) -> Ustr {
    skull_mir::names::indexed("c", "", slot as usize)
}

pub(crate) struct FrameInfo<'a> {
    pub s_cell: Idx<Cell>,
    pub slots: &'a HashMap<Idx<Cell>, u32>,
}

impl FrameInfo<'_> {
    pub fn schema(&self) -> FrameSchema {
        let mut slots: Vec<u32> = self.slots.values().copied().collect();
        slots.sort_unstable();
        FrameSchema { slots }
    }
}

pub fn convert_closures(mf: &mut MirFunc, name_base: &str) {
    convert_closures_framed(mf, name_base, None, None);
}

/// Reuse the pre-yield scope tree for a coroutine step. Rebuilding it after
/// dispatch rewriting could make a root frame cell look lambda-owned.
pub fn convert_closures_framed(
    mf: &mut MirFunc,
    name_base: &str,
    frame: Option<&FrameInfo<'_>>,
    precomputed: Option<(&ScopeTree, &Ownership)>,
) {
    if !mf
        .insts
        .iter()
        .any(|(_, i)| matches!(i, Inst::Lambda { .. }))
    {
        refresh_owner_compatibility(mf, None, LambdaTopologyFingerprint::default());
        return;
    }

    let owned_tree;
    let owned_own;
    let (tree, own): (&ScopeTree, &Ownership) = match precomputed {
        Some((t, o)) => (t, o),
        None => {
            owned_tree = ScopeTree::build(mf);
            owned_own = owned_tree.analyze(mf);
            (&owned_tree, &owned_own)
        }
    };
    let mut boxed: HashSet<Idx<Cell>> = tree.boxed_cells(own);
    if let Some(f) = frame {
        boxed.retain(|c| !f.slots.contains_key(c));
    }
    boxed.extend(non_dominating_by_value_captures(
        mf,
        tree,
        own,
        &boxed,
        frame.map(|f| f.slots),
    ));

    let mut lambda_sid: HashMap<Idx<Inst>, ScopeId> = HashMap::default();
    for (sid, s) in tree.scopes.iter().enumerate() {
        if let Some(lid) = s.lambda {
            lambda_sid.insert(lid, sid);
        }
    }
    let seeded_boxes = seeded_box_initializers(mf, tree, own, &boxed, &lambda_sid);
    // Keep the names indexed by lambda order in the instruction arena.
    let mut ordered: Vec<(usize, ScopeId)> = Vec::new();
    {
        let mut k = 0usize;
        for (lid, inst) in mf.insts.iter() {
            let Inst::Lambda { .. } = inst else { continue };
            let this_k = k;
            k += 1;
            if let Some(&sid) = lambda_sid.get(&lid) {
                ordered.push((this_k, sid));
            }
        }
    }

    let topology_sites: Vec<(LambdaSiteId, usize)> = ordered
        .iter()
        .filter_map(|&(_, sid)| {
            let lambda = tree.scopes[sid].lambda?;
            let site = mf
                .inst_spans
                .get(lambda)
                .copied()
                .and_then(LambdaSiteId::from_span)?;
            let Inst::Lambda { params, .. } = &mf.insts[lambda] else {
                return None;
            };
            Some((site, params.len()))
        })
        .collect();
    let topology = LambdaTopologyFingerprint::from_sites(&topology_sites);
    refresh_owner_compatibility(mf, frame, topology);

    let mut hoisted_name: HashMap<ScopeId, Ustr> = HashMap::default();
    let mut env_plans: HashMap<ScopeId, ClosureEnvPlan> = HashMap::default();
    for &(k, sid) in &ordered {
        hoisted_name.insert(sid, Ustr::from(format!("{name_base}__lambda{k}").as_str()));
        let mut caps: Vec<Idx<Cell>> = own.captures[sid].iter().copied().collect();
        // Loop bindings snapshot each iteration. Other frame captures reach
        // shared slots through a single capture of the frame itself.
        if let Some(f) = frame {
            let mut via_s = false;
            caps.retain(|c| {
                if !f.slots.contains_key(c) {
                    return true;
                }
                let lambda_written = own
                    .writing_scopes
                    .get(c)
                    .is_some_and(|ss| ss.iter().any(|&s| s != crate::scope::ROOT));
                if own.loop_bindings.contains(c) && !lambda_written {
                    true
                } else {
                    via_s = true;
                    false
                }
            });
            // A frame cell used only by a detached lambda may look lambda-owned.
            via_s |= f.slots.keys().any(|c| own.owner.get(c) == Some(&sid));
            if via_s {
                caps.push(f.s_cell);
            }
        }
        caps.sort_by_key(|c| raw(*c));
        let env_cell = (!caps.is_empty()).then(|| mf.new_cell(Some(Ustr::from("__rock_env"))));
        env_plans.insert(sid, ClosureEnvPlan::new(caps, env_cell));
    }
    for (&lambda, &sid) in &lambda_sid {
        if let Some(plan) = env_plans.get(&sid) {
            mf.closure_env_plans.insert(lambda, plan.clone());
        }
    }

    let mut forwarded_defers = HashMap::default();
    if frame.is_none() {
        for inst in mf.insts.values() {
            let Inst::CallValue { callee, args } = inst else {
                continue;
            };
            if !matches!(&mf.insts[*callee], Inst::Global(name) if name.as_str() == DEFER_PUSH) {
                continue;
            }
            let [lambda] = args.as_slice() else { continue };
            let Some(&sid) = lambda_sid.get(lambda) else {
                continue;
            };
            if let Some(capture) =
                forwarded_defer_capture(mf, &tree.scopes[sid], env_plans[&sid].captures(), &boxed)
            {
                forwarded_defers.insert(*lambda, capture);
            }
        }
    }

    let block_ids: Vec<Idx<Block>> = mf.blocks.iter().map(|(b, _)| b).collect();
    for b in block_ids {
        let Some(s) = tree.scope_of_block(b) else {
            continue;
        }; // dead-lambda block
        let orig = std::mem::take(&mut mf.blocks[b].insts);
        let mut new: Vec<Idx<Inst>> = Vec::with_capacity(orig.len());
        for id in orig {
            match mf.insts[id].clone() {
                Inst::Load(c) | Inst::Store { cell: c, .. } => {
                    let value = match mf.insts[id] {
                        Inst::Store { value, .. } => Some(value),
                        _ => None,
                    };
                    if let Some(value) = value
                        && seeded_boxes.get(&c) == Some(&id)
                    {
                        let seed = mf.alloc_inst(Inst::MakeEnv {
                            fn_ref: None,
                            fields: vec![(Ustr::from("v"), value)],
                        });
                        new.push(seed);
                        mf.insts[id] = Inst::Store {
                            cell: c,
                            value: seed,
                        };
                        new.push(id);
                        continue;
                    }

                    let plan = env_plans.get(&s).filter(|plan| plan.captures_cell(c));
                    let boxed = boxed.contains(&c);
                    let place = if let Some(plan) = plan {
                        if boxed {
                            let base = read_capture(mf, plan, c, &mut new);
                            Some((base, Ustr::from("v")))
                        } else if value.is_none() {
                            let base = mf.alloc_inst(Inst::Load(
                                plan.env_cell().expect("capture has an env plan"),
                            ));
                            new.push(base);
                            Some((base, field_of(plan, c)))
                        } else {
                            None
                        }
                    } else if s != crate::scope::ROOT
                        && let Some(frame) = frame
                        && let Some(&slot) = frame.slots.get(&c)
                    {
                        let base = read_capture(mf, &env_plans[&s], frame.s_cell, &mut new);
                        Some((base, frame_field(slot)))
                    } else if boxed {
                        let base = mf.alloc_inst(Inst::Load(c));
                        new.push(base);
                        Some((base, Ustr::from("v")))
                    } else {
                        None
                    };
                    if let Some((base, field)) = place {
                        mf.insts[id] = match value {
                            Some(value) => Inst::StoreField { base, field, value },
                            None => Inst::Field {
                                nonthrowing: false,
                                base,
                                field,
                            },
                        };
                    }
                    new.push(id);
                }
                Inst::Lambda { .. } => {
                    let child = lambda_sid[&id];
                    let caps = env_plans[&child].captures();
                    if let Some(&capture) = forwarded_defers.get(&id) {
                        let value = slot_in_scope(mf, own, &env_plans, s, capture, &mut new);
                        mf.insts[id] = mf.insts[value].clone();
                        new.push(id);
                        continue;
                    }
                    if caps.is_empty() {
                        let hoisted = hoisted_name[&child];
                        if !mf.hoisted_fn_globals.contains(&hoisted) {
                            mf.hoisted_fn_globals.push(hoisted);
                        }
                        mf.insts[id] = Inst::Global(hoisted);
                        new.push(id);
                        continue;
                    }
                    // Pass the box itself along a transitive capture.
                    let mut fields: Vec<(Ustr, Idx<Inst>)> = Vec::with_capacity(caps.len());
                    for &cc in caps {
                        let slot = slot_in_scope(mf, own, &env_plans, s, cc, &mut new);
                        fields.push((field_of(&env_plans[&child], cc), slot));
                    }
                    mf.insts[id] = Inst::MakeEnv {
                        fn_ref: Some(hoisted_name[&child]),
                        fields,
                    };
                    new.push(id);
                }
                _ => new.push(id),
            }
        }
        mf.blocks[b].insts = new;
    }

    let mut entry_prepend: HashMap<Idx<Block>, Vec<Idx<Inst>>> = HashMap::default();
    // Sorted so the created boxes / declarations are traversal-independent.
    let mut boxed_sorted: Vec<Idx<Cell>> = boxed.iter().copied().collect();
    boxed_sorted.sort_by_key(|c| raw(*c));
    for c in boxed_sorted {
        if seeded_boxes.contains_key(&c) {
            continue;
        }
        let owner = own.owner[&c];
        let entry = scope_entry(mf, tree, owner);
        let is_param = tree.scopes[owner].params.contains(&c);
        let dest = entry_prepend.entry(entry).or_default();
        if is_param {
            let seed = mf.alloc_inst(Inst::Load(c));
            let boxv = mf.alloc_inst(Inst::MakeEnv {
                fn_ref: None,
                fields: vec![(Ustr::from("v"), seed)],
            });
            let st = mf.alloc_inst(Inst::Store {
                cell: c,
                value: boxv,
            });
            dest.extend([seed, boxv, st]);
        } else {
            let boxv = mf.alloc_inst(Inst::MakeEnv {
                fn_ref: None,
                fields: Vec::new(),
            });
            let st = mf.alloc_inst(Inst::Store {
                cell: c,
                value: boxv,
            });
            dest.extend([boxv, st]);
        }
    }
    for (entry, mut pre) in entry_prepend {
        pre.extend(mf.blocks[entry].insts.iter().copied());
        mf.blocks[entry].insts = pre;
    }

    let live: HashSet<Idx<Block>> = reachable_blocks_from(mf, mf.entry).into_iter().collect();
    for b in &live {
        for &iid in &mf.blocks[*b].insts {
            assert!(
                !matches!(mf.insts[iid], Inst::Lambda { .. }),
                "convert_closures left an Inst::Lambda reachable from entry (pass bug)"
            );
        }
    }
    // Framed detached regions have their own conversion path.
    if frame.is_none() {
        for (_, inst) in mf.insts.iter_mut() {
            if matches!(inst, Inst::Lambda { .. }) {
                *inst = Inst::Const(Lit::Nil);
            }
        }
    }

    let mut nested: Vec<MirFunc> = Vec::with_capacity(ordered.len());
    let facts = crate::inline::ArenaFacts::of(mf);
    for &(_k, sid) in &ordered {
        let lambda = tree.scopes[sid]
            .lambda
            .expect("ordered lambda scope has a creator");
        if forwarded_defers.contains_key(&lambda) {
            continue;
        }
        let mut params: Vec<Idx<Cell>> = Vec::new();
        if let Some(e) = env_plans.get(&sid).and_then(ClosureEnvPlan::env_cell) {
            params.push(e);
        }
        params.extend(tree.scopes[sid].params.iter().copied());
        let code_identity = mf
            .code_identity
            .as_ref()
            .map(CodeIdentity::owner)
            .zip(
                mf.inst_spans
                    .get(lambda)
                    .copied()
                    .and_then(LambdaSiteId::from_span),
            )
            .map(|(enclosing, site)| CodeIdentity::Lambda { enclosing, site });
        let capture = capture_schema(mf, env_plans[&sid].captures(), &boxed, frame);
        let compatibility = Some(CodeCompatibility {
            call_abi: mf
                .lambda_abi_fingerprints
                .get(&lambda)
                .cloned()
                .unwrap_or_else(|| "missing-lambda-abi".to_string()),
            capture,
            frame: frame.map(FrameInfo::schema),
            lambda_topology: topology,
        });
        if let Some(identity) = code_identity.clone() {
            mf.code_value_identities.insert(lambda, identity.clone());
            mf.nested_fn_identities.insert(hoisted_name[&sid], identity);
        }
        let mut header = MirFunc::new(hoisted_name[&sid]);
        header.code_identity = code_identity;
        header.code_abi_fingerprint = mf.lambda_abi_fingerprints.get(&lambda).cloned();
        header.code_compatibility = compatibility;
        header.params = params;
        header.abi_representation = mf.lambda_abi_representations.get(&lambda).cloned();
        header.entry = tree.scopes[sid].entry;
        nested.push(crate::inline::compact_split_body(&header, mf, &facts));
    }
    mf.nested_fns.extend(nested);
}

/// Delay a local's box until its first store only when no access or capture can
/// observe the old entry box, and the allocation cannot repeat within the scope.
fn seeded_box_initializers(
    mf: &MirFunc,
    tree: &ScopeTree,
    own: &Ownership,
    boxed: &HashSet<Idx<Cell>>,
    lambda_sid: &HashMap<Idx<Inst>, ScopeId>,
) -> HashMap<Idx<Cell>, Idx<Inst>> {
    let locals: HashSet<_> = boxed
        .iter()
        .copied()
        .filter(|c| !tree.scopes[own.owner[c]].params.contains(c) && !own.loop_bindings.contains(c))
        .collect();
    if locals.is_empty() {
        return HashMap::default();
    }
    let mut accesses: HashMap<Idx<Cell>, Vec<(Idx<Block>, usize, Idx<Inst>)>> = HashMap::default();
    for (b, block) in mf.blocks.iter() {
        let Some(s) = tree.scope_of_block(b) else {
            continue;
        };
        for (pos, &id) in block.insts.iter().enumerate() {
            let mut record = |c| {
                if locals.contains(&c) && own.owner.get(&c) == Some(&s) {
                    accesses.entry(c).or_default().push((b, pos, id));
                }
            };
            mf.insts[id].cells_each(&mut record);
            if let Some(&child) = lambda_sid.get(&id) {
                for &c in &own.captures[child] {
                    record(c);
                }
            }
        }
    }
    let mut seeded = HashMap::default();
    let mut dominators = HashMap::default();
    let mut cyclic = HashMap::default();
    for (c, sites) in accesses {
        let owner = own.owner[&c];
        let entry = scope_entry(mf, tree, owner);
        for &(b, pos, id) in &sites {
            if !matches!(mf.insts[id], Inst::Store { cell, .. } if cell == c) {
                continue;
            }
            let dominates_all = sites.iter().all(|&(other, other_pos, _)| {
                if b == other {
                    pos <= other_pos
                } else {
                    let idom = dominators
                        .entry(owner)
                        .or_insert_with(|| skull_mir::cfg::dominators(mf, entry).intervals());
                    idom.dominates(b, other)
                }
            });
            if !dominates_all {
                continue;
            }
            // Entry allocation already repeats on entry backedges. Moving into
            // any other cycle would split a previously shared cell per iteration.
            if b != entry
                && *cyclic.entry(b).or_insert_with(|| {
                    let mut pending = Vec::new();
                    skull_mir::cfg::successors(&mf.blocks[b].term, |s| pending.push(s));
                    let mut seen = HashSet::default();
                    while let Some(next) = pending.pop() {
                        if next == b {
                            return true;
                        }
                        if seen.insert(next) {
                            skull_mir::cfg::successors(&mf.blocks[next].term, |s| pending.push(s));
                        }
                    }
                    false
                })
            {
                continue;
            }
            seeded.insert(c, id);
            break;
        }
    }
    seeded
}

/// State dispatch dominates the coroutine root after rewriting yields.
fn scope_entry(mf: &MirFunc, tree: &ScopeTree, scope: ScopeId) -> Idx<Block> {
    if scope == crate::scope::ROOT {
        mf.entry
    } else {
        tree.scopes[scope].entry
    }
}

fn refresh_owner_compatibility(
    mf: &mut MirFunc,
    frame: Option<&FrameInfo<'_>>,
    topology: LambdaTopologyFingerprint,
) {
    if mf.code_identity.is_none() {
        return;
    }
    mf.code_compatibility = Some(CodeCompatibility {
        call_abi: mf
            .code_abi_fingerprint
            .clone()
            .unwrap_or_else(|| "missing-code-abi".to_string()),
        capture: CaptureSchema::empty(),
        frame: frame.map(FrameInfo::schema),
        lambda_topology: topology,
    });
}

fn capture_schema(
    mf: &MirFunc,
    captures: &[Idx<Cell>],
    boxed: &HashSet<Idx<Cell>>,
    frame: Option<&FrameInfo<'_>>,
) -> CaptureSchema {
    let mut complete = true;
    let mut seen = HashSet::default();
    let fields = captures
        .iter()
        .map(|cell| {
            let identity = mf.cells[*cell].capture_identity.clone();
            if identity
                .as_ref()
                .is_none_or(|identity| !seen.insert(identity.clone()))
            {
                complete = false;
            }
            let mode = if frame.is_some_and(|f| f.s_cell == *cell || f.slots.contains_key(cell)) {
                CaptureMode::Frame
            } else if boxed.contains(cell) {
                CaptureMode::Shared
            } else {
                CaptureMode::ByValue
            };
            CaptureField {
                identity: identity.unwrap_or(CaptureIdentity::Generated {
                    role: Ustr::from("<unknown-capture>"),
                }),
                mode,
            }
        })
        .collect();
    CaptureSchema { fields, complete }
}

fn read_capture(
    mf: &mut MirFunc,
    plan: &ClosureEnvPlan,
    cell: Idx<Cell>,
    new: &mut Vec<Idx<Inst>>,
) -> Idx<Inst> {
    let env = plan.env_cell().expect("capture has an env plan");
    let il = mf.alloc_inst(Inst::Load(env));
    new.push(il);
    let f = mf.alloc_inst(Inst::Field {
        nonthrowing: false,
        base: il,
        field: field_of(plan, cell),
    });
    new.push(f);
    f
}

fn slot_in_scope(
    mf: &mut MirFunc,
    own: &Ownership,
    env_plans: &HashMap<ScopeId, ClosureEnvPlan>,
    s: ScopeId,
    cc: Idx<Cell>,
    new: &mut Vec<Idx<Inst>>,
) -> Idx<Inst> {
    // The synthetic state parameter is root-owned but absent from the pre-yield tree.
    let owner = own.owner.get(&cc).copied().unwrap_or(crate::scope::ROOT);
    if owner == s {
        let l = mf.alloc_inst(Inst::Load(cc));
        new.push(l);
        l
    } else {
        read_capture(mf, &env_plans[&s], cc, new)
    }
}
