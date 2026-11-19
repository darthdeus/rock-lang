use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use bone_semantic::inference::Workspace;
use skull_codegen_glsl::GlslProfile;
use skull_mir::mir::FuncRef;
use skull_mir::shader_ir::ShaderStage;
use skull_parser::Span;

use crate::instance_graph::{Concreteness, DeclKey, Instance, InstanceGraph, SiteOutcome};
use crate::target_projection::{ShaderTargetSpec, TargetSpec};

pub use crate::shader::{ShaderProgram, ShaderProgramView, ShaderStageView};

pub struct ShaderArtifactRequest<'a> {
    pub spec: TargetSpec,
    /// The shader programs, read from the `rock_shader` collector.
    pub programs: &'a [ShaderProgram],
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShaderRejection {
    /// The entry whose signature, body or helper the GLSL pass refused.
    pub instance: Instance,
    pub message: String,
    pub span: Span,
}

#[derive(Clone)]
pub struct ShaderArtifactView {
    pub profile: GlslProfile,
    /// The entries this artifact registers, as instances of the shared graph.
    pub roots: BTreeSet<Instance>,
    /// Helpers reached from those entries. Disjoint from `roots`.
    pub helpers: BTreeSet<Instance>,
    /// Instances rendered inline at call sites, never emitted as definitions.
    pub builtins: BTreeSet<Instance>,
    pub rejections: Vec<ShaderRejection>,
    /// Per-program roots and retained populations, aligned with `programs`.
    pub program_views: Vec<ShaderProgramView>,
    /// The GLSL plan for each accepted program, aligned with `program_views`.
    pub program_plans: Vec<Option<Arc<skull_codegen_glsl::ShaderPlan>>>,
}

impl ShaderArtifactView {
    pub fn render_rejections(&self, ws: &Workspace) -> Vec<String> {
        self.rejections
            .iter()
            .map(|r| {
                let f = &ws.module(r.instance.func.module).functions[r.instance.func.func];
                format!("`{}`: {}", f.name, r.message)
            })
            .collect()
    }
}

/// Realize the shader half of `req.spec` over the shared `g`, or `None` when
/// this target packages no GLSL beside its CPU artifact.
pub fn realize_shader(
    ws: &Workspace,
    g: &InstanceGraph,
    req: &ShaderArtifactRequest<'_>,
) -> Option<ShaderArtifactView> {
    let scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
        .sharing(std::sync::Arc::clone(g.interner()));
    realize_shader_with_context_and_plans(ws, g, req, &scx, None)
}

/// Realize a shader view with the lowering context that shares the graph's interner.
pub fn realize_shader_with_context(
    ws: &Workspace,
    g: &InstanceGraph,
    req: &ShaderArtifactRequest<'_>,
    scx: &skull_mir::lower::ScalarCtx,
) -> Option<ShaderArtifactView> {
    realize_shader_with_context_and_plans(ws, g, req, scx, None)
}

pub(crate) fn realize_shader_with_context_and_plans(
    ws: &Workspace,
    g: &InstanceGraph,
    req: &ShaderArtifactRequest<'_>,
    scx: &skull_mir::lower::ScalarCtx,
    plans: Option<Arc<skull_mir::concrete::ConcretePlans>>,
) -> Option<ShaderArtifactView> {
    let ShaderTargetSpec::Glsl(profile) = req.spec.shader?;

    let root_decls: BTreeSet<DeclKey> = req
        .programs
        .iter()
        .flat_map(|p| p.vert.into_iter().chain(std::iter::once(p.frag)))
        .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, r)))
        .collect();
    let roots: BTreeSet<Instance> = g
        .instances
        .iter()
        .filter(|i| root_decls.contains(&DeclKey(i.func)))
        .cloned()
        .collect();

    let mut helpers: BTreeSet<Instance> = BTreeSet::new();
    let mut builtins: BTreeSet<Instance> = BTreeSet::new();
    let mut seen: BTreeSet<Instance> = roots.clone();
    let mut queue: VecDeque<Instance> = roots.iter().cloned().collect();
    let mut edges: BTreeMap<Instance, Vec<(Instance, Span)>> = BTreeMap::new();
    while let Some(caller) = queue.pop_front() {
        let Some(sites) = g.sites_of(&caller) else {
            continue;
        };
        for rec in sites.values() {
            let SiteOutcome::Resolved(callee) = &rec.outcome else {
                continue;
            };
            let f = &ws.module(callee.func.module).functions[callee.func.func];
            if skull_codegen_glsl::renders_inline_instance(ws, f, callee, g.interner()) {
                builtins.insert(callee.clone());
                continue;
            }
            if skull_codegen_glsl::is_lowered_native_operator(ws, f, callee, g.interner()) {
                continue;
            }
            edges
                .entry(caller.clone())
                .or_default()
                .push((callee.clone(), rec.span));
            if seen.insert(callee.clone()) {
                helpers.insert(callee.clone());
                queue.push_back(callee.clone());
            }
        }
    }

    let all_concrete_plans =
        plans.unwrap_or_else(|| skull_graph::concrete_body::build(ws, g).plans);
    let entry_of = |r: FuncRef| -> Option<&Instance> {
        let key = DeclKey(skull_mir::lower::canonical_variant(ws, r));
        roots.iter().find(|i| DeclKey(i.func) == key)
    };
    let program_views: Vec<ShaderProgramView> = {
        use bone_core::prelude::*;
        req.programs
            .par_iter()
            .map(|program| {
                let mut stages = Vec::new();
                if let Some(vertex) = program.vert.and_then(entry_of) {
                    stages.push(stage_view(ws, g, &edges, vertex, ShaderStage::Vertex));
                }
                if let Some(fragment) = entry_of(program.frag) {
                    stages.push(stage_view(ws, g, &edges, fragment, ShaderStage::Fragment));
                }
                let concrete_plans = Arc::new(skull_mir::concrete::select_concrete_plan_closure(
                    &all_concrete_plans,
                    stages.iter().flat_map(|stage| stage.retained.iter()),
                ));
                ShaderProgramView {
                    program: *program,
                    stages,
                    concrete_plans,
                }
            })
            .collect()
    };
    let programs: Vec<(&ShaderProgram, &ShaderProgramView)> =
        req.programs.iter().zip(&program_views).collect();
    let checks: Vec<_> = {
        use bone_core::prelude::*;
        programs
            .par_iter()
            .map(|(program, program_view)| {
                check_program(ws, g, scx, &roots, &edges, program, program_view)
            })
            .collect()
    };
    let mut rejections = Vec::new();
    let mut program_plans = Vec::with_capacity(program_views.len());
    for check in checks {
        program_plans.push(match check {
            Some(Ok(plan)) => Some(Arc::new(plan)),
            Some(Err(rejection)) => {
                rejections.push(rejection);
                None
            }
            None => None,
        });
    }

    Some(ShaderArtifactView {
        profile,
        roots,
        helpers,
        builtins,
        rejections,
        program_views,
        program_plans,
    })
}

fn check_program(
    ws: &Workspace,
    g: &InstanceGraph,
    scx: &skull_mir::lower::ScalarCtx,
    roots: &BTreeSet<Instance>,
    edges: &BTreeMap<Instance, Vec<(Instance, Span)>>,
    program: &ShaderProgram,
    program_view: &ShaderProgramView,
) -> Option<Result<skull_codegen_glsl::ShaderPlan, ShaderRejection>> {
    let entry_of = |r: FuncRef| -> Option<&Instance> {
        let key = DeclKey(skull_mir::lower::canonical_variant(ws, r));
        roots.iter().find(|i| DeclKey(i.func) == key)
    };
    let root = entry_of(program.frag)?;
    let mut program_roots: Vec<Instance> = vec![root.clone()];
    if let Some(v) = program.vert {
        match entry_of(v) {
            Some(i) => program_roots.push(i.clone()),
            None => {
                return None;
            }
        }
    }

    if reachable_from(&program_roots, edges)
        .iter()
        .any(|i| g.concreteness(i) == Concreteness::Missing)
    {
        return None;
    }
    Some(
        skull_codegen_glsl::plan_shader_with_view(ws, scx, program_view).map_err(|error| {
            let f = &ws.module(root.func.module).functions[root.func.func];
            ShaderRejection {
                instance: root.clone(),
                span: error
                    .labels
                    .first()
                    .map(|label| label.span)
                    .unwrap_or(f.name_span),
                message: error.message,
            }
        }),
    )
}

fn stage_view(
    ws: &Workspace,
    g: &InstanceGraph,
    edges: &BTreeMap<Instance, Vec<(Instance, Span)>>,
    entry: &Instance,
    stage: ShaderStage,
) -> ShaderStageView {
    let retained = reachable_from(std::slice::from_ref(entry), edges);
    let helpers: BTreeSet<Instance> = retained.iter().filter(|i| *i != entry).cloned().collect();
    let mut builtins = BTreeSet::new();
    for caller in &retained {
        let Some(sites) = g.sites_of(caller) else {
            continue;
        };
        for rec in sites.values() {
            let SiteOutcome::Resolved(callee) = &rec.outcome else {
                continue;
            };
            let f = &ws.module(callee.func.module).functions[callee.func.func];
            if skull_codegen_glsl::renders_inline_instance(ws, f, callee, g.interner()) {
                builtins.insert(callee.clone());
            }
            if skull_codegen_glsl::is_lowered_native_operator(ws, f, callee, g.interner()) {
                continue;
            }
        }
    }
    let mut order = Vec::new();
    let mut active = BTreeSet::new();
    let mut done = BTreeSet::new();
    postorder(entry, edges, &retained, &mut active, &mut done, &mut order);
    order.retain(|i| i != entry);
    let mut dependencies = Vec::new();
    let mut seen = BTreeSet::new();
    for instance in order.iter().chain(std::iter::once(entry)) {
        let key = (
            instance.func.module.raw(),
            instance.func.func.into_raw().into_u32(),
        );
        if seen.insert(key) {
            dependencies.push(instance.func);
        }
    }
    ShaderStageView {
        stage,
        entry: entry.clone(),
        retained,
        helpers,
        builtins,
        order,
        dependencies,
    }
}

fn postorder(
    node: &Instance,
    edges: &BTreeMap<Instance, Vec<(Instance, Span)>>,
    retained: &BTreeSet<Instance>,
    active: &mut BTreeSet<Instance>,
    done: &mut BTreeSet<Instance>,
    order: &mut Vec<Instance>,
) {
    if done.contains(node) || !active.insert(node.clone()) {
        return;
    }
    for (callee, _) in edges.get(node).into_iter().flatten() {
        if retained.contains(callee) {
            postorder(callee, edges, retained, active, done, order);
        }
    }
    active.remove(node);
    done.insert(node.clone());
    if !order.contains(node) {
        order.push(node.clone());
    }
}

/// The instances reachable from `roots` over the retained shader call relation.
fn reachable_from(
    roots: &[Instance],
    edges: &BTreeMap<Instance, Vec<(Instance, Span)>>,
) -> BTreeSet<Instance> {
    let mut seen: BTreeSet<Instance> = roots.iter().cloned().collect();
    let mut queue: VecDeque<Instance> = roots.iter().cloned().collect();
    while let Some(n) = queue.pop_front() {
        for (callee, _) in edges.get(&n).into_iter().flatten() {
            if seen.insert(callee.clone()) {
                queue.push_back(callee.clone());
            }
        }
    }
    seen
}
