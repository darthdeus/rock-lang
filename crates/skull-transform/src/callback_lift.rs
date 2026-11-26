//! Specialize synchronous delivery of a capturing lambda while retaining the
//! call boundaries of its wrappers.
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use bone_core::hir::FnBackend;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as Map, FxHashSet as Set};
use skull_mir::code_identity::LambdaSiteId;
use skull_mir::concrete::ConcretePlans;
use skull_mir::instance::Instance;
use skull_mir::lower::{BackendFnAbiPlanHandle, FunctionRepPlan, ScalarCtx};
use skull_mir::lua_interop::{
    LUAJIT_CALL_BASE_SLOTS, LUAJIT_FRAME_SLOT_LIMIT, LUAJIT_MAX_ACTIVE_LOCALS,
};
use skull_mir::mir::*;
use ustr::Ustr;

use crate::inline::{ArenaFacts, call_target, compact_split_body};
use crate::scope::{ROOT, ScopeTree};
use crate::visit::{raw, top_level_blocks};

const PREFIX: &str = "__rock_lifted_callback_";

type Parameter = (Instance, usize);
type Call = (Idx<Block>, Idx<Inst>);

#[derive(Clone)]
pub(crate) struct Template {
    body: MirFunc,
    abi: BackendFnAbiPlanHandle,
    results: usize,
    fingerprint: u64,
}

#[derive(Clone, Default)]
pub(crate) struct Cache(Map<Instance, Option<Arc<Template>>>);

impl Cache {
    pub(crate) fn invalidate(&mut self, func: FuncRef) {
        self.0.retain(|inst, _| inst.func != func);
    }

    fn get(&mut self, cx: &Context<'_>, inst: &Instance) -> Option<Arc<Template>> {
        self.0
            .entry(inst.clone())
            .or_insert_with(|| Template::lower(cx, inst).map(Arc::new))
            .clone()
    }
}

pub(crate) struct Context<'a> {
    pub ws: &'a Workspace,
    pub scx: &'a ScalarCtx,
    pub plans: &'a ConcretePlans,
    pub allow_acyclic: bool,
}

impl Template {
    fn lower(cx: &Context<'_>, inst: &Instance) -> Option<Self> {
        let f = &cx.ws.module(inst.func.module).functions[inst.func.func];
        if f.is_coroutine || f.is_extern || f.inline_body.is_some() || f.has_iterator_bounds() {
            return None;
        }
        let plan = cx.plans.get(inst)?;
        let abi = skull_mir::lower::owner_abi_plan(cx.ws, cx.scx, cx.plans, inst).ok()?;
        let mut body =
            skull_mir::lower::lower_instance_with_plans(cx.ws, cx.scx, plan, cx.plans).ok()?;
        crate::cleanup(&mut body).ok()?;
        // The cloned wrapper must have no independently published code slots.
        if !body.nested_fns.is_empty()
            || body
                .insts
                .values()
                .any(|inst| matches!(inst, Inst::Lambda { .. }))
        {
            return None;
        }
        let results = result_lanes(&body, body.entry)?;
        let fingerprint =
            skull_mir::code_identity::direct_body_fingerprint(&body, cx.ws, cx.scx.interner());
        Some(Self {
            body,
            abi,
            results,
            fingerprint,
        })
    }
}

fn result_lanes(mf: &MirFunc, entry: Idx<Block>) -> Option<usize> {
    let mut count = None;
    for b in crate::visit::top_level_blocks_from(mf, entry) {
        match &mf.blocks[b].term {
            Term::Yield { .. } => return None,
            Term::Return(values) => {
                if count
                    .replace(values.len())
                    .is_some_and(|n| n != values.len())
                {
                    return None;
                }
            }
            _ => {}
        }
    }
    count
}

enum Delivery {
    Invoke,
    Forward(Parameter),
}

struct Wrapper {
    param: usize,
    source: Arc<Template>,
    calls: Vec<(Call, Delivery)>,
}

fn direct_call(mf: &MirFunc, id: Idx<Inst>) -> Option<(FuncRef, &[Idx<Inst>])> {
    match &mf.insts[id] {
        Inst::Call { func, args } | Inst::CallMulti { func, args, .. } => Some((*func, args)),
        _ => None,
    }
}

fn call_shape(inst: &Inst) -> (usize, Option<usize>) {
    match inst {
        Inst::Call { args, .. } | Inst::CallValue { args, .. } => (args.len(), None),
        Inst::CallMulti { args, dests, .. } | Inst::CallValueMulti { args, dests, .. } => {
            (args.len(), Some(dests.len()))
        }
        _ => unreachable!("call shape"),
    }
}

fn call_matches_results(inst: &Inst, results: usize) -> bool {
    call_shape(inst).1.is_none_or(|dests| dests <= results)
}

fn argument_position(args: &[Idx<Inst>], reads: &Set<Idx<Inst>>) -> Option<usize> {
    let mut positions = args.iter().enumerate().filter(|(_, a)| reads.contains(a));
    let (pos, _) = positions.next()?;
    positions.next().is_none().then_some(pos)
}

fn uses(mf: &MirFunc, param: Idx<Cell>) -> Option<Set<Idx<Inst>>> {
    let mut reads = Set::default();
    for b in top_level_blocks(mf) {
        for &id in &mf.blocks[b].insts {
            let inst = &mf.insts[id];
            if matches!(inst, Inst::Load(c) if *c == param) {
                reads.insert(id);
            } else {
                let mut touched = false;
                inst.cells_each(&mut |c| touched |= c == param);
                if touched {
                    return None;
                }
            }
        }
    }
    Some(reads)
}

impl Wrapper {
    fn analyze(
        cx: &Context<'_>,
        param: usize,
        source: Arc<Template>,
        lambda_rep: &FunctionRepPlan,
        lambda_params: usize,
        lambda_results: usize,
    ) -> Option<Self> {
        let mf = &source.body;
        let cell = *mf.params.get(param)?;
        if mf.cells[cell].borrowed {
            return None;
        }
        let reads = uses(mf, cell)?;
        let mut calls = Vec::new();
        let mentions = |inst: &Inst| {
            let mut found = false;
            inst.uses_each(&mut |v| found |= reads.contains(&v));
            found
        };
        for b in top_level_blocks(mf) {
            for &id in &mf.blocks[b].insts {
                let inst = &mf.insts[id];
                if !mentions(inst) {
                    continue;
                }
                let delivery = match inst {
                    Inst::Call { func, args } | Inst::CallMulti { func, args, .. } => {
                        let (owner, _) = call_target(mf, id, *func, cx.ws).ok()??;
                        Delivery::Forward((owner.clone(), argument_position(args, &reads)?))
                    }
                    Inst::CallValue { callee, args }
                    | Inst::CallValueMulti { callee, args, .. }
                        if reads.contains(callee)
                            && !args.iter().any(|arg| reads.contains(arg))
                            && args.len() == lambda_params
                            && call_matches_results(inst, lambda_results)
                            && mf.call_rep_plans.get(&id) == Some(lambda_rep) =>
                    {
                        Delivery::Invoke
                    }
                    Inst::Eq(a, b) | Inst::NotEq(a, b)
                        if [a, b]
                            .into_iter()
                            .any(|v| matches!(mf.insts[*v], Inst::Const(Lit::Nil))) =>
                    {
                        continue;
                    }
                    _ => return None,
                };
                calls.push(((b, id), delivery));
            }
            let mut escapes = false;
            mf.blocks[b]
                .term
                .uses_each(&mut |v| escapes |= reads.contains(&v));
            if escapes {
                return None;
            }
        }
        Some(Self {
            param,
            source,
            calls,
        })
    }
}

struct Family {
    indices: Map<Parameter, usize>,
    wrappers: Vec<Wrapper>,
}

impl Family {
    fn fits_lua_wire(&self, captures: usize, lambda_params: usize, caller_params: usize) -> bool {
        let width = |index: usize| self.wrappers[index].source.body.params.len() - 1 + captures;
        // LuaJIT places call operands above active locals, including the
        // temporary that holds the generated callee.
        let call_fits = |params, args| {
            params < LUAJIT_MAX_ACTIVE_LOCALS
                && params + 1 + LUAJIT_CALL_BASE_SLOTS + args < LUAJIT_FRAME_SLOT_LIMIT
        };
        let lambda_width = lambda_params + captures;
        lambda_width <= LUAJIT_MAX_ACTIVE_LOCALS
            && call_fits(caller_params, width(0))
            && self.wrappers.iter().enumerate().all(|(index, wrapper)| {
                width(index) <= LUAJIT_MAX_ACTIVE_LOCALS
                    && wrapper.calls.iter().all(|(_, delivery)| {
                        let args = match delivery {
                            Delivery::Invoke => lambda_width,
                            Delivery::Forward(target) => width(self.indices[target]),
                        };
                        call_fits(width(index), args)
                    })
            })
    }

    fn has_recursive_forward(&self) -> bool {
        let successors = |index: usize| {
            self.wrappers[index]
                .calls
                .iter()
                .filter_map(|(_, delivery)| match delivery {
                    Delivery::Forward(target) => Some(self.indices[target]),
                    Delivery::Invoke => None,
                })
        };
        let mut incoming = vec![0; self.wrappers.len()];
        for index in 0..incoming.len() {
            for target in successors(index) {
                incoming[target] += 1;
            }
        }
        let mut available: Vec<_> = (0..incoming.len())
            .filter(|&index| incoming[index] == 0)
            .collect();
        let mut removed = 0;
        while let Some(index) = available.pop() {
            removed += 1;
            for target in successors(index) {
                incoming[target] -= 1;
                if incoming[target] == 0 {
                    available.push(target);
                }
            }
        }
        removed != incoming.len()
    }

    fn prove(
        cx: &Context<'_>,
        cache: &mut Cache,
        entry: Parameter,
        abi: BackendFnAbiPlanHandle,
        lambda_rep: &FunctionRepPlan,
        lambda_params: usize,
        lambda_results: usize,
        caller: (&MirFunc, Idx<Inst>),
        dependencies: &mut Set<FuncRef>,
    ) -> Option<Self> {
        let mut indices = Map::default();
        let mut wrappers = Vec::new();
        let mut pending = vec![(entry, abi, call_shape(&caller.0.insts[caller.1]))];
        while let Some((key, abi, (args, dests))) = pending.pop() {
            dependencies.insert(key.0.func);
            let source = cache.get(cx, &key.0)?;
            if source.abi != abi
                || source.body.params.len() != args
                || dests.is_some_and(|dests| dests > source.results)
            {
                return None;
            }
            if indices.contains_key(&key) {
                continue;
            }
            // Recursive edges are allowed: every reachable parameter is
            // independently checked before any call or closure is rewritten.
            let wrapper =
                Wrapper::analyze(cx, key.1, source, lambda_rep, lambda_params, lambda_results)?;
            for ((_, call), delivery) in &wrapper.calls {
                if let Delivery::Forward(target) = delivery {
                    pending.push((
                        target.clone(),
                        wrapper.source.body.call_abi_plans[call].clone(),
                        call_shape(&wrapper.source.body.insts[*call]),
                    ));
                }
            }
            indices.insert(key, wrappers.len());
            wrappers.push(wrapper);
        }
        Some(Self { indices, wrappers })
    }
}

fn loaded(mf: &mut MirFunc, cells: impl IntoIterator<Item = Idx<Cell>>) -> Vec<Idx<Inst>> {
    cells
        .into_iter()
        .map(|cell| mf.alloc_inst(Inst::Load(cell)))
        .collect()
}

fn append_state_returns(mf: &mut MirFunc, state: &[Idx<Cell>]) {
    if state.is_empty() {
        return;
    }
    for b in top_level_blocks(mf) {
        if let Term::Return(mut values) = mf.blocks[b].term.clone() {
            let extra = loaded(mf, state.iter().copied());
            mf.blocks[b].insts.extend(extra.iter().copied());
            values.extend(extra);
            mf.blocks[b].term = Term::Return(values);
        }
    }
}

fn insert_before(mf: &mut MirFunc, (block, before): Call, ids: Vec<Idx<Inst>>) {
    let list = &mut mf.blocks[block].insts;
    let pos = list.iter().position(|&id| id == before).expect("live call");
    list.splice(pos..pos, ids);
}

fn specialize_call(
    mf: &mut MirFunc,
    at: Call,
    name: Ustr,
    removed_arg: Option<usize>,
    extra: &[Idx<Inst>],
    results: usize,
    writeback: &[Idx<Cell>],
) {
    let (_, id) = at;
    let (mut args, mut dests) = match &mf.insts[id] {
        Inst::Call { args, .. } | Inst::CallValue { args, .. } => (args.clone(), None),
        Inst::CallMulti { args, dests, .. } | Inst::CallValueMulti { args, dests, .. } => {
            (args.clone(), Some(dests.clone()))
        }
        _ => unreachable!("specialized call"),
    };
    if let Some(pos) = removed_arg {
        args.remove(pos);
    }
    args.extend_from_slice(extra);
    let callee = mf.alloc_inst(Inst::Global(name));
    let mut before = vec![callee];
    let mut call = id;
    let inst = if writeback.is_empty() {
        match dests {
            None => Inst::CallValue { callee, args },
            Some(dests) => Inst::CallValueMulti {
                callee,
                args,
                dests,
            },
        }
    } else {
        let value = dests.is_none().then(|| mf.new_cell(None));
        let mut dests = dests
            .take()
            .unwrap_or_else(|| value.into_iter().take(results).collect());
        // A discarded result lane still precedes the returned state.
        dests.resize_with(results, || mf.new_cell(None));
        dests.extend_from_slice(writeback);
        let multi = Inst::CallValueMulti {
            callee,
            args,
            dests,
        };
        if let Some(value) = value {
            call = mf.alloc_inst(multi);
            if let Some(&span) = mf.inst_spans.get(id) {
                mf.inst_spans.insert(call, span);
            }
            before.push(call);
            if results == 0 {
                Inst::Const(Lit::Nil)
            } else {
                Inst::Load(value)
            }
        } else {
            multi
        }
    };
    insert_before(mf, at, before);
    mf.insts[id] = inst;
    mf.spec_bindings.remove(&id);
    mf.callee_instances.remove(&id);
    mf.call_abi_plans.remove(&id);
    mf.call_rep_plans.remove(&id);
    mf.flat_call_lanes.remove(&id);
    mf.call_sites.remove(&id);
    mf.flat_call_lanes.insert(call, results + writeback.len());
}

// The ordinary closure converter still owns allocation and initialization of
// cells shared with other closures. Pass their existing pointers into the lift.
fn use_shared_cells(mf: &mut MirFunc, shared: &Set<Idx<Cell>>) {
    if shared.is_empty() {
        return;
    }
    let field = Ustr::from("v");
    for b in top_level_blocks(mf) {
        let mut insts = Vec::new();
        for id in std::mem::take(&mut mf.blocks[b].insts) {
            let replacement = match mf.insts[id] {
                Inst::Load(cell) if shared.contains(&cell) => {
                    let base = mf.alloc_inst(Inst::Load(cell));
                    insts.push(base);
                    Some(Inst::Field {
                        base,
                        field,
                        nonthrowing: false,
                    })
                }
                Inst::Store { cell, value } if shared.contains(&cell) => {
                    let base = mf.alloc_inst(Inst::Load(cell));
                    insts.push(base);
                    Some(Inst::StoreField {
                        base,
                        field,
                        value,
                        native_leaf: None,
                    })
                }
                _ => None,
            };
            if let Some(inst) = replacement {
                mf.insts[id] = inst;
            }
            insts.push(id);
        }
        mf.blocks[b].insts = insts;
    }
}

pub(crate) fn run(
    cx: Context<'_>,
    cache: &mut Cache,
    mf: &mut MirFunc,
    name: &str,
    dependencies: &mut Set<FuncRef>,
    lambda_sites: &mut Option<Map<Idx<Inst>, LambdaSiteId>>,
    shared_loads: &mut Map<Idx<Inst>, Idx<Cell>>,
) -> bool {
    if crate::inline::inline_off_active()
        || skull_mir::pass_disabled("lambda")
        || mf.lambda_abis.is_empty()
    {
        return false;
    }
    let mut sites = Vec::new();
    for b in top_level_blocks(mf) {
        for &id in &mf.blocks[b].insts {
            let Some((func, args)) = direct_call(mf, id) else {
                continue;
            };
            for (param, &arg) in args.iter().enumerate() {
                if matches!(mf.insts[arg], Inst::Lambda { .. }) {
                    sites.push(((b, id), func, param, arg));
                }
            }
        }
    }
    if sites.is_empty() {
        return false;
    }
    let tree = ScopeTree::build(mf);
    let own = tree.analyze(mf);
    let mut boxed = tree.boxed_cells(&own);
    boxed.extend(crate::scope::non_dominating_by_value_captures(
        mf, &tree, &own, &boxed, None,
    ));
    let lambda_scopes: Map<_, _> = tree
        .scopes
        .iter()
        .enumerate()
        .filter_map(|(sid, scope)| Some((scope.lambda?, sid)))
        .collect();
    let target_values: Set<_> = sites.iter().map(|&(_, _, _, arg)| arg).collect();
    let mut counts = Map::<_, usize>::default();
    for scope in &tree.scopes {
        for &b in &scope.region {
            let mut observe = |v| {
                if target_values.contains(&v) {
                    *counts.entry(v).or_default() += 1;
                }
            };
            for &id in &mf.blocks[b].insts {
                mf.insts[id].uses_each(&mut observe);
            }
            mf.blocks[b].term.uses_each(&mut observe);
        }
    }
    let repeated = crate::capture_lifetime::repeated_captures(mf, &tree, &own, None);
    let mut changed = false;
    for (at, func, param, lambda) in sites {
        if counts[&lambda] != 1 || direct_call(mf, at.1).is_none() {
            continue;
        }
        let sid = lambda_scopes[&lambda];
        let scope = &tree.scopes[sid];
        let mut captures: Vec<_> = own.captures[sid].iter().copied().collect();
        captures.sort_unstable();
        if scope.parent != Some(ROOT)
            || captures.is_empty()
            || captures.iter().any(|&c| mf.cells[c].borrowed)
        {
            continue;
        }
        let shared: Set<_> = captures
            .iter()
            .copied()
            .filter(|c| {
                boxed.contains(c)
                    && own
                        .captures
                        .iter()
                        .enumerate()
                        .any(|(other, caps)| other != sid && caps.contains(c))
            })
            .collect();
        if shared.iter().any(|cell| repeated.contains_key(cell)) {
            continue;
        }
        if scope.region.iter().any(|&b| {
            mf.blocks[b].insts.iter().any(|&id| match mf.insts[id] {
                Inst::Load(_) | Inst::Store { .. } => false,
                Inst::Lambda { .. } => true,
                ref inst => {
                    let mut touches_shared = false;
                    inst.cells_each(&mut |c| touches_shared |= shared.contains(&c));
                    touches_shared
                }
            })
        }) {
            continue;
        }
        let Some(lambda_abi) = mf.lambda_abis.get(&lambda) else {
            continue;
        };
        let Some(lambda_results) = result_lanes(mf, scope.entry) else {
            continue;
        };
        let Ok(Some((owner, abi))) = call_target(mf, at.1, func, cx.ws) else {
            continue;
        };
        let Some(family) = Family::prove(
            &cx,
            cache,
            (owner.clone(), param),
            abi.clone(),
            &lambda_abi.representation,
            scope.params.len(),
            lambda_results,
            (mf, at.1),
            dependencies,
        ) else {
            continue;
        };
        // Cloning a nonrecursive wrapper can split a hot shared LuaJIT trace.
        // Reserve that specialization for explicit allocation-free contracts.
        if !cx.allow_acyclic && !family.has_recursive_forward() {
            continue;
        }
        if cx.scx.target() == FnBackend::Lua
            && !family.fits_lua_wire(captures.len(), scope.params.len(), mf.params.len())
        {
            continue;
        }
        lambda_sites.get_or_insert_with(|| crate::closure::lambda_sites(mf));
        let mut written = Set::default();
        for &b in &scope.region {
            for &id in &mf.blocks[b].insts {
                mf.insts[id].cell_writes_each(&mut |c| {
                    written.insert(c);
                });
            }
        }
        let state: Vec<_> = captures
            .iter()
            .copied()
            .filter(|c| written.contains(c) && !shared.contains(c))
            .collect();
        let mut header = MirFunc::new(Ustr::default());
        header.direct_only = true;
        header.entry = scope.entry;
        header.params.extend(scope.params.iter().copied());
        header.params.extend(captures.iter().copied());
        let mut lifted = compact_split_body(&header, mf, &ArenaFacts::of(mf));
        let lifted_state: Vec<_> = state
            .iter()
            .map(|c| lifted.params[scope.params.len() + captures.binary_search(c).unwrap()])
            .collect();
        let shared_params: Set<_> = captures
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                shared
                    .contains(c)
                    .then_some(lifted.params[scope.params.len() + i])
            })
            .collect();
        use_shared_cells(&mut lifted, &shared_params);
        append_state_returns(&mut lifted, &lifted_state);
        // Old activations keep referring to the same private functions if an
        // incremental overlay changes the forwarded targets or capture ABI.
        let mut hash = rustc_hash::FxHasher::default();
        skull_mir::code_identity::direct_body_fingerprint(&lifted, cx.ws, cx.scx.interner())
            .hash(&mut hash);
        lambda_abi.fingerprint.hash(&mut hash);
        for wrapper in &family.wrappers {
            wrapper.source.fingerprint.hash(&mut hash);
            wrapper.param.hash(&mut hash);
        }
        let base = format!("{PREFIX}{name}_{}_{:016x}", raw(lambda), hash.finish());
        let lifted_name = Ustr::from(format!("{base}_body").as_str());
        let wrapper_name = |idx| Ustr::from(format!("{base}_wrap{idx}").as_str());
        lifted.name = lifted_name;
        mf.nested_fns.push(lifted);
        for (index, wrapper) in family.wrappers.iter().enumerate() {
            let mut body = wrapper.source.body.clone();
            body.name = wrapper_name(index);
            body.direct_only = true;
            body.code_identity = None;
            body.code_compatibility = None;
            body.code_abi_fingerprint = None;
            body.abi_plan = None;
            body.abi_representation = None;
            let callback = body.params.remove(wrapper.param);
            for inst in body.insts.values_mut() {
                if matches!(inst, Inst::Load(c) if *c == callback) {
                    *inst = Inst::Const(Lit::Bool(true));
                }
            }
            let capture_params: Vec<_> = captures
                .iter()
                .map(|&c| {
                    let cell = body.new_cell(None);
                    body.cells[cell].ty = mf.cells[c].ty;
                    cell
                })
                .collect();
            body.params.extend(capture_params.iter().copied());
            let state_params: Vec<_> = state
                .iter()
                .map(|c| capture_params[captures.binary_search(c).unwrap()])
                .collect();
            for &((block, id), ref delivery) in &wrapper.calls {
                let (name, removed, results) = match delivery {
                    Delivery::Invoke => (lifted_name, None, lambda_results),
                    Delivery::Forward(target) => {
                        let index = family.indices[target];
                        (
                            wrapper_name(index),
                            Some(target.1),
                            family.wrappers[index].source.results,
                        )
                    }
                };
                let reads = loaded(&mut body, capture_params.iter().copied());
                insert_before(&mut body, (block, id), reads.clone());
                specialize_call(
                    &mut body,
                    (block, id),
                    name,
                    removed,
                    &reads,
                    results,
                    &state_params,
                );
            }
            append_state_returns(&mut body, &state_params);
            mf.comptime_reads
                .extend(body.comptime_reads.iter().cloned());
            mf.struct_registry_roots
                .extend(body.struct_registry_roots.iter().copied());
            mf.nested_fns.push(body);
        }
        let extra: Vec<_> = captures
            .iter()
            .map(|&c| {
                let read = mf.alloc_inst(Inst::Load(c));
                let at = if boxed.contains(&c) {
                    at
                } else {
                    (scope.creation_block.unwrap(), lambda)
                };
                insert_before(mf, at, vec![read]);
                if shared.contains(&c) {
                    shared_loads.insert(read, c);
                }
                read
            })
            .collect();
        specialize_call(
            mf,
            at,
            wrapper_name(0),
            Some(param),
            &extra,
            family.wrappers[0].source.results,
            &state,
        );
        mf.insts[lambda] = Inst::Const(Lit::Nil);
        mf.lambda_abis.remove(&lambda);
        changed = true;
    }
    changed
}
