use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{Idx, Span};
use skull_mir::cfg::{
    dominates, dominators, predecessors, reachable_blocks_from, reverse_postorder, successors,
};
use skull_mir::mir::{Block, FuncRef, Inst, MirFunc};
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
    StringConcat,
    StringConversion,
    StringFormat,
    TupleConstruction,
    VecConstruction,
    VecColumnConstruction,
    VecColumnHeader,
    StructConstruction,
    ValueConstruction,
    ExplicitValueMaterialization,
    EnumConstruction,
    DynamicEnumConstruction,
    ClosureEnvironment,
    FfiArray,
    FfiView,
    ContainerGrowth,
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
            Self::StringConcat => "string-concat",
            Self::StringConversion => "string-conversion",
            Self::StringFormat => "string-format",
            Self::TupleConstruction => "tuple-construction",
            Self::VecConstruction => "vec-construction",
            Self::VecColumnConstruction => "vec-column-construction",
            Self::VecColumnHeader => "vec-column-header",
            Self::StructConstruction => "struct-construction",
            Self::ValueConstruction => "value-construction",
            Self::ExplicitValueMaterialization => "explicit-value-materialization",
            Self::EnumConstruction => "enum-construction",
            Self::DynamicEnumConstruction => "dynamic-enum-construction",
            Self::ClosureEnvironment => "closure-environment",
            Self::FfiArray => "ffi-array",
            Self::FfiView => "ffi-view",
            Self::ContainerGrowth => "container-growth",
            Self::OpaqueCall => "opaque-call",
        }
    }
}

impl From<skull_codegen_lua::EmittedAllocationKind> for AllocationKind {
    fn from(kind: skull_codegen_lua::EmittedAllocationKind) -> Self {
        use skull_codegen_lua::EmittedAllocationKind as Emitted;
        match kind {
            Emitted::StringConcat => Self::StringConcat,
            Emitted::StringConversion => Self::StringConversion,
            Emitted::StringFormat => Self::StringFormat,
            Emitted::TupleConstruction => Self::TupleConstruction,
            Emitted::VecConstruction => Self::VecConstruction,
            Emitted::VecColumnConstruction => Self::VecColumnConstruction,
            Emitted::VecColumnHeader => Self::VecColumnHeader,
            Emitted::StructConstruction => Self::StructConstruction,
            Emitted::ValueConstruction => Self::ValueConstruction,
            Emitted::ExplicitValueMaterialization => Self::ExplicitValueMaterialization,
            Emitted::EnumConstruction => Self::EnumConstruction,
            Emitted::DynamicEnumConstruction => Self::DynamicEnumConstruction,
            Emitted::ClosureEnvironment => Self::ClosureEnvironment,
            Emitted::FfiArray => Self::FfiArray,
            Emitted::FfiView => Self::FfiView,
            Emitted::ContainerGrowth => Self::ContainerGrowth,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationSite {
    /// Artifact-local identity shared by the static report and its instrumented
    /// build. It is deterministic for one emitted artifact; it is
    /// not promised stable after edits move or reshape this body.
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
    pub direct_sites: Vec<AllocationSite>,
    pub call_edges: Vec<AllocationCallEdge>,
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
}

impl AllocationReport {
    pub(crate) fn from_bodies(bodies: impl IntoIterator<Item = AllocationBody>) -> Self {
        let mut report = Self::default();
        for body in bodies {
            report.bodies.insert(body.emitted_name.clone(), body);
        }
        report
    }

    pub fn trace_from_diagnostic_names(&self, name: &str, roots: &[&str]) -> AllocationRoute {
        let emitted: Vec<String> = self
            .bodies
            .values()
            .filter(|body| roots.iter().any(|root| body.diagnostic_name == *root))
            .map(|body| body.emitted_name.clone())
            .collect();
        self.trace_from_emitted_names(name, emitted)
    }

    fn trace_from_emitted_names(&self, name: &str, mut roots: Vec<String>) -> AllocationRoute {
        roots.sort();
        roots.dedup();
        let mut parent: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut route_loop_depth: BTreeMap<String, u32> = BTreeMap::new();
        let mut direct_callers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut queue = VecDeque::new();
        for root in &roots {
            if self.bodies.contains_key(root) && !parent.contains_key(root) {
                parent.insert(root.clone(), None);
                route_loop_depth.insert(root.clone(), 0);
                queue.push_back(root.clone());
            }
        }
        while let Some(body_name) = queue.pop_front() {
            let Some(body) = self.bodies.get(&body_name) else {
                continue;
            };
            for edge in &body.call_edges {
                if self.bodies.contains_key(&edge.callee) {
                    direct_callers
                        .entry(edge.callee.clone())
                        .or_default()
                        .insert(body_name.clone());
                    if !parent.contains_key(&edge.callee) {
                        parent.insert(edge.callee.clone(), Some(body_name.clone()));
                        route_loop_depth.insert(
                            edge.callee.clone(),
                            route_loop_depth[&body_name] + edge.local_loop_depth,
                        );
                        queue.push_back(edge.callee.clone());
                    }
                }
            }
        }

        let mut findings = Vec::new();
        let mut opaque_boundaries = Vec::new();
        for body_name in parent.keys() {
            let body = &self.bodies[body_name];
            for site in &body.direct_sites {
                let mut call_chain = vec![body_name.clone()];
                let mut cursor = body_name;
                while let Some(Some(caller)) = parent.get(cursor) {
                    call_chain.push(caller.clone());
                    cursor = caller;
                }
                call_chain.reverse();
                let finding = AllocationFinding {
                    body: body_name.clone(),
                    site: site.clone(),
                    call_chain,
                    direct_callers: direct_callers
                        .get(body_name)
                        .map(|callers| callers.iter().cloned().collect())
                        .unwrap_or_default(),
                    structural_loop_depth: route_loop_depth[body_name] + site.local_loop_depth,
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
                        .map(|s| (s.file, s.offset_range))
                        .cmp(&b.site.span.map(|s| (s.file, s.offset_range)))
                })
                .then_with(|| a.site.kind.cmp(&b.site.kind))
                .then_with(|| a.site.id.cmp(&b.site.id))
        });
        opaque_boundaries.sort_by(|a, b| {
            a.call_chain
                .cmp(&b.call_chain)
                .then_with(|| a.site.id.cmp(&b.site.id))
        });
        AllocationRoute {
            name: name.to_string(),
            roots,
            reachable_bodies: parent.len(),
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
        out
    }
}

pub(crate) fn collect_emitted_body(
    ws: &Workspace,
    interner: &MonoInterner,
    emitted_name: &str,
    diagnostic_name: &str,
    func: &MirFunc,
    emitted_sites: Vec<skull_codegen_lua::EmittedAllocationSite>,
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
    let mut direct_sites = emitted_sites
        .into_iter()
        .map(|site| {
            let iid = Idx::from_raw(site.mir_inst.into());
            let use_facts = result_uses[site.mir_inst as usize];
            let (result_use, result_uses) = if func.insts[iid].produces_value() {
                (use_facts.result_use(), use_facts.uses)
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
                aggregate_type,
                aggregate_fields,
                lua_body_line: site.lua_body_line,
                local_loop_depth: inst_loop_depths.get(&site.mir_inst).copied().unwrap_or(0),
            }
        })
        .collect::<Vec<_>>();
    let mut call_edges: BTreeMap<String, u32> = BTreeMap::new();
    for block in reachable_blocks_from(func, func.entry) {
        let loop_depth = block_loop_depths.get(&block).copied().unwrap_or(0);
        for &iid in &func.blocks[block].insts {
            let inst = &func.insts[iid];
            match inst {
                Inst::Call { func: target, .. } | Inst::CallMulti { func: target, .. } => {
                    let callee = &ws.module(target.module).functions[target.func];
                    let target_name = call_target(ws, interner, func, iid, *target);
                    if callee.is_extern || callee.inline_body.is_some() || callee.is_coroutine {
                        direct_sites.push(site(
                            emitted_name,
                            iid,
                            AllocationKind::OpaqueCall,
                            func.inst_spans.get(iid).copied(),
                            Some(target_name),
                            loop_depth,
                        ));
                    } else {
                        let depth = call_edges.entry(target_name).or_default();
                        *depth = (*depth).max(loop_depth);
                    }
                }
                Inst::CallValue { .. } | Inst::CallValueMulti { .. } => direct_sites.push(site(
                    emitted_name,
                    iid,
                    AllocationKind::OpaqueCall,
                    func.inst_spans.get(iid).copied(),
                    Some("indirect function value".to_string()),
                    loop_depth,
                )),
                _ => {}
            }
        }
    }
    Some(AllocationBody {
        emitted_name: emitted_name.to_string(),
        diagnostic_name: diagnostic_name.to_string(),
        direct_sites,
        call_edges: call_edges
            .into_iter()
            .map(|(callee, local_loop_depth)| AllocationCallEdge {
                callee,
                local_loop_depth,
            })
            .collect(),
    })
}

fn allocation_aggregate(inst: &Inst) -> (Option<String>, u32) {
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
        Inst::MakeVecColumns { elem, columns, .. } => {
            (Some(format!("Vec<{elem}>")), columns.len() as u32)
        }
        Inst::MakeVecColumnHeader { elem, .. } => (Some(format!("Vec<{elem}>")), 0),
        Inst::MakeEnv { fields, .. } => (
            Some("<closure-environment>".to_string()),
            fields.len() as u32,
        ),
        _ => (None, 0),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct AllocationUseFacts {
    bits: u32,
    uses: u32,
}

impl AllocationUseFacts {
    fn record(&mut self, kind: AllocationResultUse) {
        self.bits |= kind.bit();
    }

    fn result_use(self) -> AllocationResultUse {
        if self.bits == 0 {
            return AllocationResultUse::Unused;
        }
        if self.bits.count_ones() > 1 {
            return AllocationResultUse::Mixed;
        }
        AllocationResultUse::from_bit(self.bits)
    }
}

impl AllocationResultUse {
    fn bit(self) -> u32 {
        match self {
            Self::DirectCall => 1 << 0,
            Self::FieldOnly => 1 << 1,
            Self::Returned => 1 << 2,
            Self::Yielded => 1 << 3,
            Self::LocalStore => 1 << 4,
            Self::GlobalStore => 1 << 5,
            Self::FieldStore => 1 << 6,
            Self::ContainerStore => 1 << 7,
            Self::Mutated => 1 << 8,
            Self::Identity => 1 << 9,
            Self::IndirectCall => 1 << 10,
            Self::NestedAggregate => 1 << 11,
            Self::Other => 1 << 12,
            Self::Effect | Self::Unused | Self::Mixed | Self::Unknown => 0,
        }
    }

    fn from_bit(bit: u32) -> Self {
        match bit {
            x if x == Self::DirectCall.bit() => Self::DirectCall,
            x if x == Self::FieldOnly.bit() => Self::FieldOnly,
            x if x == Self::Returned.bit() => Self::Returned,
            x if x == Self::Yielded.bit() => Self::Yielded,
            x if x == Self::LocalStore.bit() => Self::LocalStore,
            x if x == Self::GlobalStore.bit() => Self::GlobalStore,
            x if x == Self::FieldStore.bit() => Self::FieldStore,
            x if x == Self::ContainerStore.bit() => Self::ContainerStore,
            x if x == Self::Mutated.bit() => Self::Mutated,
            x if x == Self::Identity.bit() => Self::Identity,
            x if x == Self::IndirectCall.bit() => Self::IndirectCall,
            x if x == Self::NestedAggregate.bit() => Self::NestedAggregate,
            x if x == Self::Other.bit() => Self::Other,
            _ => Self::Unknown,
        }
    }
}

fn allocation_result_uses(func: &MirFunc) -> Vec<AllocationUseFacts> {
    let mut facts = vec![AllocationUseFacts::default(); func.insts.len()];
    for block in reachable_blocks_from(func, func.entry) {
        for &iid in &func.blocks[block].insts {
            let inst = &func.insts[iid];
            inst.uses_each(&mut |operand| {
                let use_facts = &mut facts[operand.into_raw().into_u32() as usize];
                use_facts.uses += 1;
                allocation_inst_use(inst, operand, use_facts);
            });
        }
        match &func.blocks[block].term {
            skull_mir::mir::Term::Return(values) => {
                for &value in values {
                    let use_facts = &mut facts[value.into_raw().into_u32() as usize];
                    use_facts.uses += 1;
                    use_facts.record(AllocationResultUse::Returned);
                }
            }
            skull_mir::mir::Term::Yield { values, .. } => {
                for &value in values {
                    let use_facts = &mut facts[value.into_raw().into_u32() as usize];
                    use_facts.uses += 1;
                    use_facts.record(AllocationResultUse::Yielded);
                }
            }
            skull_mir::mir::Term::Branch { cond, .. } => {
                let use_facts = &mut facts[cond.into_raw().into_u32() as usize];
                use_facts.uses += 1;
                use_facts.record(AllocationResultUse::Other);
            }
            skull_mir::mir::Term::Switch { discr, .. } => {
                let use_facts = &mut facts[discr.into_raw().into_u32() as usize];
                use_facts.uses += 1;
                use_facts.record(AllocationResultUse::Other);
            }
            _ => {}
        }
    }
    facts
}

fn allocation_inst_use(inst: &Inst, target: Idx<Inst>, facts: &mut AllocationUseFacts) {
    match inst {
        Inst::Field { base, .. } | Inst::TupleIndex { base, .. } if *base == target => {
            facts.record(AllocationResultUse::FieldOnly);
        }
        Inst::Call { args, .. }
        | Inst::CallMulti { args, .. }
        | Inst::CallBoundMulti { args, .. }
            if args.contains(&target) =>
        {
            facts.record(AllocationResultUse::DirectCall);
        }
        Inst::CallValue { callee, args } | Inst::CallValueMulti { callee, args, .. }
            if *callee == target || args.contains(&target) =>
        {
            facts.record(AllocationResultUse::IndirectCall);
        }
        Inst::Store { value, .. } if *value == target => {
            facts.record(AllocationResultUse::LocalStore);
        }
        Inst::StoreGlobal { value, .. } if *value == target => {
            facts.record(AllocationResultUse::GlobalStore);
        }
        Inst::StoreField { base, value, .. } => {
            if *base == target {
                facts.record(AllocationResultUse::Mutated);
            }
            if *value == target {
                facts.record(AllocationResultUse::FieldStore);
            }
        }
        Inst::StoreIndex { base, value, .. }
        | Inst::ArrayPush {
            arr: base, value, ..
        } => {
            if *base == target {
                facts.record(AllocationResultUse::Mutated);
            }
            if *value == target {
                facts.record(AllocationResultUse::ContainerStore);
            }
        }
        Inst::Eq(a, b) | Inst::NotEq(a, b) if *a == target || *b == target => {
            facts.record(AllocationResultUse::Identity);
        }
        Inst::MakeTuple(values) | Inst::MakeVec(values) if values.contains(&target) => {
            facts.record(AllocationResultUse::NestedAggregate);
        }
        Inst::MakeStruct { fields, .. }
        | Inst::MakeValueStruct { fields, .. }
        | Inst::MaterializeValueStruct { fields, .. }
        | Inst::MakeEnv { fields, .. }
            if fields.iter().any(|(_, value)| *value == target) =>
        {
            facts.record(AllocationResultUse::NestedAggregate);
        }
        Inst::MakeEnum {
            positional, named, ..
        } if positional.contains(&target) || named.iter().any(|(_, value)| *value == target) => {
            facts.record(AllocationResultUse::NestedAggregate);
        }
        Inst::MakeEnumDyn {
            tag,
            positional,
            named,
            ..
        } if *tag == target
            || positional.contains(&target)
            || named.iter().any(|(_, value)| *value == target) =>
        {
            facts.record(AllocationResultUse::NestedAggregate);
        }
        _ => {
            facts.record(AllocationResultUse::Other);
        }
    }
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
                    for &pred in all_preds.get(&block).map_or(&[][..], Vec::as_slice) {
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
    if let Some(span) = span {
        hash.write(span.file.as_str().as_bytes());
        hash.write(&span.offset_range.0.to_le_bytes());
        hash.write(&span.offset_range.1.to_le_bytes());
    }
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
