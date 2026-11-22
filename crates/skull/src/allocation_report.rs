use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{Idx, Span};
use skull_mir::cfg::{
    dominates, dominators, predecessors, reachable_blocks_from, reverse_postorder, successors,
};
use skull_mir::mir::{Block, FuncRef, Inst, MirFunc, Term};
use skull_mir::monoty::MonoInterner;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static ACTIVATION: Cell<bool> = const { Cell::new(false) };
}

pub struct Scope(bool);
pub struct ActivationScope(bool);
pub(crate) struct EnvironmentScope {
    _report: Scope,
    _activation: ActivationScope,
}

impl Drop for Scope {
    fn drop(&mut self) {
        ENABLED.set(self.0);
    }
}

impl Drop for ActivationScope {
    fn drop(&mut self) {
        ACTIVATION.set(self.0);
    }
}

pub fn scope(enabled: bool) -> Scope {
    let previous = ENABLED.replace(enabled);
    Scope(previous)
}

pub fn activation_scope(enabled: bool) -> ActivationScope {
    let previous = ACTIVATION.replace(enabled);
    ActivationScope(previous)
}

pub(crate) fn environment_scope() -> EnvironmentScope {
    let activation = ACTIVATION.get() || std::env::var_os("SKULL_ALLOCATION_ACTIVATION").is_some();
    let report =
        ENABLED.get() || activation || std::env::var_os("SKULL_ALLOCATION_REPORT").is_some();
    EnvironmentScope {
        _report: scope(report),
        _activation: activation_scope(activation),
    }
}

pub fn enabled() -> bool {
    ENABLED.get()
}

pub(crate) fn activation_enabled() -> bool {
    ACTIVATION.get()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AllocationKind {
    Emitted(skull_codegen_lua::EmittedAllocationKind),
    OpaqueCall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AllocationResultUse {
    Effect,
    Unused,
    DirectCall,
    FieldOnly,
    Returned,
    Yielded,
    LocalStore,
    GlobalStore,
    FieldStore,
    ContainerStore,
    Mutated,
    Identity,
    IndirectCall,
    NestedAggregate,
    Other,
    Mixed,
    Unknown,
}

impl AllocationResultUse {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Effect => "effect",
            Self::Unused => "unused",
            Self::DirectCall => "direct-call",
            Self::FieldOnly => "field-only",
            Self::Returned => "returned",
            Self::Yielded => "yielded",
            Self::LocalStore => "local-store",
            Self::GlobalStore => "global-store",
            Self::FieldStore => "field-store",
            Self::ContainerStore => "container-store",
            Self::Mutated => "mutated",
            Self::Identity => "identity",
            Self::IndirectCall => "indirect-call",
            Self::NestedAggregate => "nested-aggregate",
            Self::Other => "other",
            Self::Mixed => "mixed",
            Self::Unknown => "unknown",
        }
    }
}

impl AllocationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Emitted(kind) => kind.as_str(),
            Self::OpaqueCall => "opaque-call",
        }
    }
}

impl From<skull_codegen_lua::EmittedAllocationKind> for AllocationKind {
    fn from(kind: skull_codegen_lua::EmittedAllocationKind) -> Self {
        Self::Emitted(kind)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationSite {
    /// Artifact-local identity; not stable after edits move or reshape this body.
    pub id: u64,
    pub kind: AllocationKind,
    pub span: Option<Span>,
    pub detail: Option<String>,
    pub result_use: AllocationResultUse,
    pub result_uses: u32,
    pub aggregate_type: Option<String>,
    pub aggregate_fields: u32,
    pub lua_body_line: u32,
    pub local_loop_depth: u32,
    reason: Option<String>,
}

impl AllocationSite {
    pub(crate) fn with_reason(mut self, reason: String) -> Self {
        self.reason = Some(reason);
        self
    }

    pub fn reason(&self) -> Option<&str> {
        use crate::mir_emit::noalloc::allocation_description;
        self.reason.as_deref().or_else(|| match self.kind {
            AllocationKind::Emitted(kind) => Some(allocation_description(kind)),
            AllocationKind::OpaqueCall => None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationCallEdge {
    pub callee: String,
    pub local_loop_depth: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationBody {
    pub emitted_name: String,
    pub diagnostic_name: String,
    pub(crate) declaration_span: Option<Span>,
    pub direct_sites: Vec<AllocationSite>,
    pub call_edges: Vec<AllocationCallEdge>,
    pub(crate) pc_sources: Vec<skull_codegen_lua::PcSource>,
}

impl AllocationBody {
    pub fn source_span(&self, site: &AllocationSite) -> Option<Span> {
        site.span
            .filter(|span| span.file.as_str() != "<unknown>")
            .or(self.declaration_span)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationFinding {
    pub body: String,
    pub site: AllocationSite,
    pub call_chain: Vec<String>,
    pub direct_callers: Vec<String>,
    pub structural_loop_depth: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllocationRoute {
    pub name: String,
    pub roots: Vec<String>,
    pub reachable_bodies: usize,
    pub findings: Vec<AllocationFinding>,
    pub opaque_boundaries: Vec<AllocationFinding>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllocationReport {
    pub bodies: BTreeMap<String, AllocationBody>,
    pub lifecycle_routes: Vec<AllocationRoute>,
    pub(crate) pc_owners: BTreeMap<String, (&'static str, Span)>,
}

impl AllocationReport {
    pub(crate) fn from_bodies(bodies: impl IntoIterator<Item = AllocationBody>) -> Self {
        let mut report = Self::default();
        for body in bodies {
            report.bodies.insert(body.emitted_name.clone(), body);
        }
        report
    }

    fn trace_from_emitted_names(&self, name: &str, mut roots: Vec<String>) -> AllocationRoute {
        roots.sort();
        roots.dedup();
        #[derive(Default)]
        struct Visit<'a> {
            parent: Option<&'a str>,
            loop_depth: u32,
            callers: BTreeSet<&'a str>,
        }

        let mut visits = BTreeMap::new();
        let mut queue = VecDeque::new();
        for root in roots.iter().map(String::as_str) {
            if self.bodies.contains_key(root) {
                visits.insert(root, Visit::default());
                queue.push_back(root);
            }
        }
        while let Some(body) = queue.pop_front() {
            let loop_depth = visits[body].loop_depth;
            for edge in &self.bodies[body].call_edges {
                let callee = edge.callee.as_str();
                if !self.bodies.contains_key(callee) {
                    continue;
                }
                let visit = visits.entry(callee).or_insert_with(|| {
                    queue.push_back(callee);
                    Visit {
                        parent: Some(body),
                        loop_depth: loop_depth + edge.local_loop_depth,
                        callers: BTreeSet::new(),
                    }
                });
                visit.callers.insert(body);
            }
        }

        let mut findings = Vec::new();
        let mut opaque_boundaries = Vec::new();
        for (&body, visit) in &visits {
            let sites = &self.bodies[body].direct_sites;
            if sites.is_empty() {
                continue;
            }
            let mut chain = vec![body];
            while let Some(caller) = visits[chain[chain.len() - 1]].parent {
                chain.push(caller);
            }
            chain.reverse();
            for site in sites {
                let finding = AllocationFinding {
                    body: body.to_string(),
                    site: site.clone(),
                    call_chain: chain.iter().map(|name| name.to_string()).collect(),
                    direct_callers: visit.callers.iter().map(|name| name.to_string()).collect(),
                    structural_loop_depth: visit.loop_depth + site.local_loop_depth,
                };
                if site.kind == AllocationKind::OpaqueCall {
                    opaque_boundaries.push(finding);
                } else {
                    findings.push(finding);
                }
            }
        }
        findings.sort_by(|a, b| {
            a.call_chain
                .cmp(&b.call_chain)
                .then_with(|| {
                    a.site
                        .span
                        .map(|s| (s.file, s.offset_range()))
                        .cmp(&b.site.span.map(|s| (s.file, s.offset_range())))
                })
                .then_with(|| a.site.kind.cmp(&b.site.kind))
                .then_with(|| a.site.id.cmp(&b.site.id))
        });
        opaque_boundaries.sort_by(|a, b| {
            a.call_chain
                .cmp(&b.call_chain)
                .then_with(|| a.site.id.cmp(&b.site.id))
        });
        let reachable_bodies = visits.len();
        AllocationRoute {
            name: name.to_string(),
            roots,
            reachable_bodies,
            findings,
            opaque_boundaries,
        }
    }

    pub(crate) fn populate_lifecycle_routes(
        &mut self,
        plan: &skull_mir::lifecycle::LifecyclePlan,
        artifact: &skull_mir::code_identity::CodeArtifact,
    ) {
        let roots_for = |phases: &[&str]| {
            plan.entries
                .iter()
                .filter(|entry| phases.contains(&entry.phase.as_str()))
                .filter_map(|entry| artifact.entries.get(&entry.identity)?.body_name)
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        let coroutine_roots = self
            .bodies
            .keys()
            .filter(|name| name.ends_with("__step"))
            .cloned()
            .collect::<Vec<_>>();
        self.lifecycle_routes = vec![
            self.trace_from_emitted_names("frame.update", roots_for(&["update", "late_update"])),
            self.trace_from_emitted_names("frame.prepare", roots_for(&["prepare"])),
            self.trace_from_emitted_names(
                "frame.draw",
                roots_for(&["draw", "late_draw", "post_imgui_draw"]),
            ),
            self.trace_from_emitted_names("coroutine.resume", coroutine_roots),
        ];
    }

    pub fn render_text(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        let site_count: usize = self
            .bodies
            .values()
            .map(|body| body.direct_sites.len())
            .sum();
        let _ = writeln!(
            out,
            "emitted allocation-capable Lua report: {} bodies, {} direct sites",
            self.bodies.len(),
            site_count
        );
        let emitted_sites = self
            .bodies
            .values()
            .flat_map(|body| {
                body.direct_sites
                    .iter()
                    .filter(|site| site.kind != AllocationKind::OpaqueCall)
                    .map(move |site| (site, body.emitted_name.as_str()))
            })
            .collect::<Vec<_>>();
        let _ = writeln!(
            out,
            "\nemitted site manifest: {} sites",
            emitted_sites.len()
        );
        for (site, body) in emitted_sites {
            let source = site
                .span
                .map(|span| span.format_site())
                .unwrap_or_else(|| "<compiler-generated>".to_string());
            let _ = writeln!(
                out,
                "  {:016x}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                site.id,
                site.kind.as_str(),
                source,
                body,
                site.lua_body_line,
                site.local_loop_depth,
                site.result_use.as_str(),
                site.result_uses,
                site.aggregate_type.as_deref().unwrap_or("-"),
                site.aggregate_fields,
            );
        }
        for route in &self.lifecycle_routes {
            let _ = writeln!(
                out,
                "\nroute {}: {} roots, {} reachable bodies, {} allocation-capable sites, {} opaque boundaries",
                route.name,
                route.roots.len(),
                route.reachable_bodies,
                route.findings.len(),
                route.opaque_boundaries.len(),
            );
            let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
            for finding in &route.findings {
                *kinds.entry(finding.site.kind.as_str()).or_default() += 1;
            }
            for (kind, count) in kinds {
                let _ = writeln!(out, "  kind {kind}: {count}");
            }
            for finding in &route.findings {
                let site = finding
                    .site
                    .span
                    .map(|span| span.format_site())
                    .unwrap_or_else(|| "<compiler-generated>".to_string());
                let detail = finding
                    .site
                    .detail
                    .as_deref()
                    .map(|detail| format!(" ({detail})"))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "  {:016x} {}{} at {} via {}\tloop_depth\t{}\tcallers\t{}",
                    finding.site.id,
                    finding.site.kind.as_str(),
                    detail,
                    site,
                    finding.call_chain.join(" -> "),
                    finding.structural_loop_depth,
                    finding.direct_callers.join(" | "),
                );
            }
        }
        for (name, &(kind, span)) in &self.pc_owners {
            let _ = writeln!(out, "pc-source\t{name}\t0\t{kind}\t{}", span.format_site());
        }
        let mut files = BTreeMap::new();
        for body in self
            .bodies
            .values()
            .filter(|body| !body.pc_sources.is_empty())
        {
            let _ = writeln!(out, "pc-body\t{}", body.emitted_name);
            for &(line, kind, span) in &body.pc_sources {
                let next = files.len();
                let file = files.entry(span.file).or_insert_with(|| {
                    let _ = writeln!(out, "pc-file\t{next}\t{}", span.file);
                    next
                });
                let source_line = span.line_range().0 + 1;
                let _ = writeln!(out, "pc-line\t{line}\t{kind}\t{file}\t{source_line}");
            }
        }
        out
    }
}

pub(crate) fn collect_emitted_body(
    ws: &Workspace,
    interner: &MonoInterner,
    emitted_name: &str,
    diagnostic_name: &str,
    func: &MirFunc,
    emission: &mut skull_codegen_lua::LuaEmission,
) -> Option<AllocationBody> {
    if !enabled() {
        return None;
    }
    let block_loop_depths = natural_loop_depths(func);
    let mut inst_loop_depths = HashMap::new();
    for block in reachable_blocks_from(func, func.entry) {
        let depth = block_loop_depths.get(&block).copied().unwrap_or(0);
        for &iid in &func.blocks[block].insts {
            inst_loop_depths.insert(iid.into_raw().into_u32(), depth);
        }
    }
    let result_uses = allocation_result_uses(func);
    let mut direct_sites = std::mem::take(&mut emission.allocation_sites)
        .into_iter()
        .map(|site| {
            let iid = Idx::from_raw(site.mir_inst.into());
            let use_facts = result_uses[site.mir_inst as usize];
            let (result_use, result_uses) = if func.insts[iid].produces_value() {
                (use_facts.kind, use_facts.uses)
            } else {
                (AllocationResultUse::Effect, 0)
            };
            let (aggregate_type, aggregate_fields) = allocation_aggregate(&func.insts[iid]);
            AllocationSite {
                id: site.id,
                kind: site.kind.into(),
                span: site.span,
                detail: None,
                result_use,
                result_uses,
                aggregate_fields,
                lua_body_line: site.lua_body_line,
                local_loop_depth: inst_loop_depths.get(&site.mir_inst).copied().unwrap_or(0),
                reason: source_reason(&func.insts[iid], site.kind, aggregate_type.as_deref()),
                aggregate_type,
            }
        })
        .collect::<Vec<_>>();
    let mut call_edges: BTreeMap<String, u32> = BTreeMap::new();
    for block in reachable_blocks_from(func, func.entry) {
        let loop_depth = block_loop_depths.get(&block).copied().unwrap_or(0);
        for &iid in &func.blocks[block].insts {
            if emission.call_sites.binary_search(&iid).is_err() {
                continue;
            }
            match &func.insts[iid] {
                Inst::Call { func: target, args }
                | Inst::CallMulti {
                    func: target, args, ..
                } => {
                    let module = ws.module(target.module);
                    let callee = &module.functions[target.func];
                    let target_name = call_target(ws, interner, func, iid, *target);
                    if callee.is_extern || callee.inline_body.is_some() || callee.is_coroutine {
                        let mut finding = site(
                            emitted_name,
                            iid,
                            AllocationKind::OpaqueCall,
                            func.inst_spans.get(iid).copied(),
                            Some(target_name),
                            loop_depth,
                        );
                        let name = skull_mir::mangle::declared_name(callee.name.as_str());
                        if callee.is_coroutine {
                            finding.reason = Some(format!("starts coroutine `{name}`"));
                        } else if name == "push"
                            && module.file.as_str() == "<stdlib>/unified.rock"
                            && skull_mir::intrinsics::is_intrinsic(callee)
                        {
                            finding.reason = Some("push may grow the Vec table".to_string());
                        } else if !crate::mir_emit::noalloc::marked(callee)
                            && !crate::mir_emit::noalloc::scalar_intrinsic(
                                ws,
                                module.file.as_str(),
                                callee,
                                func,
                                iid,
                                args,
                            )
                        {
                            finding.reason = Some(format!("calls unverified `{name}`"));
                        }
                        direct_sites.push(finding);
                    } else {
                        let depth = call_edges.entry(target_name).or_default();
                        *depth = (*depth).max(loop_depth);
                    }
                }
                Inst::CallValue { .. } | Inst::CallValueMulti { .. } => {
                    let mut finding = site(
                        emitted_name,
                        iid,
                        AllocationKind::OpaqueCall,
                        func.inst_spans.get(iid).copied(),
                        Some("indirect function value".to_string()),
                        loop_depth,
                    );
                    if !crate::mir_emit::noalloc::checked_integer_guard(func, iid) {
                        finding.reason = Some("calls an unverified function value".to_string());
                    }
                    direct_sites.push(finding);
                }
                _ => {}
            }
        }
    }
    Some(AllocationBody {
        emitted_name: emitted_name.to_string(),
        diagnostic_name: diagnostic_name.to_string(),
        declaration_span: func
            .site_owners
            .first()
            .map(|owner| ws.module(owner.func.module).functions[owner.func.func].name_span),
        direct_sites,
        pc_sources: std::mem::take(&mut emission.pc_sources),
        call_edges: call_edges
            .into_iter()
            .map(|(callee, local_loop_depth)| AllocationCallEdge {
                callee,
                local_loop_depth,
            })
            .collect(),
    })
}

fn source_reason(
    inst: &Inst,
    kind: skull_codegen_lua::EmittedAllocationKind,
    aggregate: Option<&str>,
) -> Option<String> {
    use skull_codegen_lua::EmittedAllocationKind as Kind;
    match inst {
        Inst::MaterializeValueStruct { name, .. } => {
            return Some(format!("materializes `{name}` at a boxing boundary"));
        }
        Inst::MakeValueStruct {
            name,
            form: skull_mir::mir::ValueCtorForm::Leaves,
            ..
        } => {
            return Some(format!("boxes `{name}` from scalar fields"));
        }
        Inst::MakeEnumDyn { ty, .. } => {
            return Some(format!("boxes `{ty}` from a dynamic tag"));
        }
        Inst::ArrayPush { .. } => return Some("push may grow the Vec table".to_string()),
        Inst::MakeEnv { fn_ref: None, .. } => {
            return Some("boxes a shared mutable capture".to_string());
        }
        Inst::MakeEnv { .. } => return Some("allocates a closure environment".to_string()),
        _ => {}
    }
    match (kind, aggregate) {
        (
            Kind::StructConstruction | Kind::ValueConstruction | Kind::EnumConstruction,
            Some(name),
        ) => Some(format!("constructs `{name}`")),
        (Kind::VecColumnConstruction | Kind::VecColumnHeader, Some(name)) => {
            Some(format!("constructs column storage for `{name}`"))
        }
        _ => None,
    }
}

pub(crate) fn allocation_aggregate(inst: &Inst) -> (Option<String>, u32) {
    match inst {
        Inst::MakeStruct { ty, fields } => (
            Some(ty.map_or_else(|| "<anonymous>".to_string(), |ty| ty.to_string())),
            fields.len() as u32,
        ),
        Inst::MakeValueStruct { name, fields, .. }
        | Inst::MaterializeValueStruct { name, fields, .. } => {
            (Some(name.to_string()), fields.len() as u32)
        }
        Inst::MakeEnum {
            ty,
            positional,
            named,
            ..
        } => (
            Some(ty.to_string()),
            (positional.len() + named.len()) as u32,
        ),
        Inst::MakeEnumDyn {
            ty,
            positional,
            named,
            ..
        } => (
            Some(ty.to_string()),
            (1 + positional.len() + named.len()) as u32,
        ),
        Inst::MakeTuple(values) => (Some("<tuple>".to_string()), values.len() as u32),
        Inst::MakeVec(values) => (Some("<vec>".to_string()), values.len() as u32),
        Inst::MakeFlatVec(vec) => {
            let count = match &vec.contents {
                skull_mir::mir::FlatVecContents::Columns { columns, .. } => columns.len() as u32,
                skull_mir::mir::FlatVecContents::Header(None) => 0,
                _ => return (None, 0),
            };
            (Some(format!("Vec<{}>", vec.elem.name)), count)
        }
        Inst::MakeEnv { fields, .. } => (
            Some("<closure-environment>".to_string()),
            fields.len() as u32,
        ),
        _ => (None, 0),
    }
}

#[derive(Clone, Copy, Debug)]
struct AllocationUseFacts {
    kind: AllocationResultUse,
    uses: u32,
}

impl AllocationUseFacts {
    fn record(&mut self, kind: AllocationResultUse) {
        self.kind = match self.kind {
            AllocationResultUse::Unused => kind,
            current if current == kind => current,
            _ => AllocationResultUse::Mixed,
        };
    }
}

fn allocation_result_uses(func: &MirFunc) -> Vec<AllocationUseFacts> {
    use AllocationResultUse::*;
    let mut facts = vec![
        AllocationUseFacts {
            kind: Unused,
            uses: 0
        };
        func.insts.len()
    ];
    let mut record = |operand: Idx<Inst>, kind| {
        let fact = &mut facts[operand.into_raw().into_u32() as usize];
        fact.uses += 1;
        fact.record(kind);
    };
    for bb in reachable_blocks_from(func, func.entry) {
        let block = &func.blocks[bb];
        for &iid in &block.insts {
            let inst = &func.insts[iid];
            match inst {
                Inst::StoreField { base, value, .. } => {
                    record(*base, Mutated);
                    record(*value, FieldStore);
                }
                Inst::ArrayPush { arr, value, .. } => {
                    record(*arr, Mutated);
                    record(*value, ContainerStore);
                }
                Inst::StoreIndex {
                    base, index, value, ..
                } => {
                    record(*base, Mutated);
                    let index_use = if index == base {
                        Mutated
                    } else if index == value {
                        ContainerStore
                    } else {
                        Other
                    };
                    record(*index, index_use);
                    record(*value, ContainerStore);
                }
                _ => {
                    let kind = match inst {
                        Inst::Field { .. } | Inst::TupleIndex { .. } => FieldOnly,
                        Inst::Call { .. }
                        | Inst::CallMulti { .. }
                        | Inst::CallBoundMulti { .. } => DirectCall,
                        Inst::CallValue { .. } | Inst::CallValueMulti { .. } => IndirectCall,
                        Inst::Store { .. } => LocalStore,
                        Inst::StoreGlobal { .. } => GlobalStore,
                        Inst::Eq(..) | Inst::NotEq(..) => Identity,
                        Inst::MakeTuple(_)
                        | Inst::MakeVec(_)
                        | Inst::MakeStruct { .. }
                        | Inst::MakeValueStruct { .. }
                        | Inst::MaterializeValueStruct { .. }
                        | Inst::MakeEnv { .. }
                        | Inst::MakeEnum { .. }
                        | Inst::MakeEnumDyn { .. } => NestedAggregate,
                        _ => Other,
                    };
                    inst.uses_each(&mut |operand| record(operand, kind));
                }
            }
        }
        let kind = match block.term {
            Term::Return(_) => Returned,
            Term::Yield { .. } => Yielded,
            _ => Other,
        };
        block.term.uses_each(&mut |operand| record(operand, kind));
    }
    facts
}

fn natural_loop_depths(func: &MirFunc) -> HashMap<Idx<Block>, u32> {
    let rpo = reverse_postorder(func, func.entry);
    let (all_preds, _) = predecessors(func, &rpo);
    let idom = dominators(func, func.entry);
    let mut loops: HashMap<Idx<Block>, HashSet<Idx<Block>>> = HashMap::new();

    for &latch in &rpo {
        successors(&func.blocks[latch].term, |header| {
            if !dominates(&idom, header, latch) {
                return;
            }
            let body = loops.entry(header).or_default();
            body.insert(header);
            if body.insert(latch) {
                let mut stack = vec![latch];
                while let Some(block) = stack.pop() {
                    for &pred in all_preds.get(&block).unwrap_or_default() {
                        if pred != header && body.insert(pred) {
                            stack.push(pred)
                        }
                    }
                }
            }
        });
    }

    let mut depths = HashMap::new();
    for body in loops.values() {
        for &block in body {
            *depths.entry(block).or_default() += 1
        }
    }
    depths
}

fn call_target(
    ws: &Workspace,
    interner: &MonoInterner,
    owner: &MirFunc,
    iid: Idx<Inst>,
    target: FuncRef,
) -> String {
    match owner.spec_bindings.get(&iid) {
        Some(instance) => instance.symbol(ws, interner),
        None => skull_codegen_lua::mangle_fn(ws, &ws.module(target.module).functions[target.func]),
    }
}

pub(crate) fn generated_site(
    body: &str,
    span: Span,
    kind: skull_codegen_lua::EmittedAllocationKind,
    lua_body_line: u32,
    result_use: AllocationResultUse,
) -> AllocationSite {
    let mut allocation = site(
        body,
        Idx::from_raw(lua_body_line.into()),
        kind.into(),
        Some(span),
        None,
        0,
    );
    allocation.lua_body_line = lua_body_line;
    allocation.result_use = result_use;
    allocation.result_uses = 1;
    allocation
}

pub(crate) fn instrument_generated(site: Option<&AllocationSite>, value: String) -> String {
    if let Some(site) = site {
        return format!("(__rock_alloc_site[\"{:016x}\"] and {value})", site.id);
    }
    value
}

fn site(
    body: &str,
    iid: Idx<Inst>,
    kind: AllocationKind,
    span: Option<Span>,
    detail: Option<String>,
    local_loop_depth: u32,
) -> AllocationSite {
    let mut hash = Fnv64::new();
    hash.write(body.as_bytes());
    hash.write(kind.as_str().as_bytes());
    hash.write(&iid.into_raw().into_u32().to_le_bytes());
    AllocationSite {
        id: hash.finish(),
        kind,
        span,
        detail,
        result_use: AllocationResultUse::Unknown,
        result_uses: 0,
        aggregate_type: None,
        aggregate_fields: 0,
        lua_body_line: 0,
        local_loop_depth,
        reason: None,
    }
}

struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod report_behavior_tests {
    use super::*;

    #[test]
    fn full_emission_reports_live_aggregate_uses_and_stable_shared_routes() {
        let file = ustr::Ustr::from("<allocation-report>");
        let source = r#"
struct Pair { x: int, y: int }
global RETAINED: Pair = Pair { x: 0, y: 0 }
@lifecycle(phase="update", owner="process") deco init tick {}
fn projected(seed: int) -> int {
    let pair = Pair { x: seed, y: seed + 1 }
    return pair.x
}
fn consume(pair: Pair) -> int { return pair.x }
fn passed(seed: int) -> int { return consume(Pair { x: seed, y: seed + 1 }) }
fn returned(seed: int) -> Pair { return Pair { x: seed, y: seed + 1 } }
fn retained(seed: int) { RETAINED = Pair { x: seed, y: seed + 1 } }
fn shared(seed: int) -> [int] { return [seed] }
fn zebra(seed: int) -> int { return shared(seed).len() }
fn alpha(seed: int) -> int { return shared(seed).len() }
@tick fn update_system() {
    let _ = projected(1)
    let _ = passed(6)
    let _ = returned(2)
    retained(3)
    let _ = zebra(4) + alpha(5)
}
fn main() { update_system() }
"#;
        let user = skull_parser::parse_module_clean(source, file).expect("parse");
        let mut modules = skull_parser::stdlib::ModuleSet::with_stdlib([user]).expect("stdlib");
        let ws = crate::pipeline::semantic_pass(&mut modules, &Default::default())
            .expect("semantic pass");
        let mid = *ws.module_ids.get(&file).expect("user module");
        let _report = scope(true);
        let (emit, _) = crate::mir_emit::emit_full(&ws, mid).expect("full emission");
        let report = emit.allocation_report.render_text();

        let has_use = |body: &str, kind: &str| {
            report
                .lines()
                .any(|line| line.contains(body) && line.contains(&format!("\t{kind}\t1\t")))
        };
        assert!(has_use("passed", "direct-call"));
        assert!(has_use("returned", "returned"));
        assert!(has_use("retained", "global-store"));
        assert!(
            !report.contains("field-only"),
            "the projected temporary is scalarized"
        );
        let shared = report
            .lines()
            .find(|line| line.contains("shared") && line.contains("callers\t"))
            .expect("shared leaf route");
        assert!(
            shared.contains("callers\talpha_int | zebra_int"),
            "{shared}"
        );
        assert!(
            shared.split(" via ").nth(1).is_some_and(|path| {
                let path = path.split("\tloop_depth").next().unwrap_or(path);
                path.contains("alpha_int") && !path.contains("zebra_int")
            }),
            "{shared}"
        );
    }
}
