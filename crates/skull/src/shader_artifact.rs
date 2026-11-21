use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bone_semantic::inference::Workspace;
use skull_codegen_glsl::GlslProfile;
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
    /// A missing root keeps an empty stage list so the denominator cannot shrink.
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

/// Plans are a pure function of `(ws, graph)`, so an emit driver shares its
/// table; `None` builds one and is for standalone tools only.
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

    let projections: Vec<_> = {
        use bone_core::prelude::*;
        roots
            .iter()
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|entry| (entry, StageProjection::new(ws, g, entry)))
            .collect()
    };
    let mut helpers = BTreeSet::new();
    let mut builtins = BTreeSet::new();
    let mut by_decl = BTreeMap::new();
    for (entry, projection) in &projections {
        by_decl.entry(DeclKey(entry.func)).or_insert(projection);
        helpers.extend(
            projection
                .order
                .iter()
                .filter(|i| !roots.contains(*i))
                .cloned(),
        );
        builtins.extend(projection.builtins.iter().cloned());
    }
    let all_concrete_plans =
        plans.unwrap_or_else(|| skull_graph::concrete_body::build(ws, g).plans);
    let stage_of = |root, stage| {
        let key = DeclKey(skull_mir::lower::canonical_variant(ws, root));
        Some(by_decl.get(&key)?.view(stage))
    };

    let (program_views, checks): (Vec<_>, Vec<_>) = {
        use bone_core::prelude::*;
        req.programs
            .par_iter()
            .map(|program| {
                let stages: Vec<_> = program
                    .vert
                    .and_then(|entry| stage_of(entry, ShaderStage::Vertex))
                    .into_iter()
                    .chain(stage_of(program.frag, ShaderStage::Fragment))
                    .collect();
                let concrete_plans = Arc::new(skull_mir::concrete::select_concrete_plan_closure(
                    &all_concrete_plans,
                    stages.iter().flat_map(|stage| stage.retained.iter()),
                ));
                let program_view = ShaderProgramView {
                    program: *program,
                    stages,
                    concrete_plans,
                };
                let check = check_program(ws, g, scx, &program_view);
                (program_view, check)
            })
            .unzip()
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
    view: &ShaderProgramView,
) -> Option<Result<skull_codegen_glsl::ShaderPlan, ShaderRejection>> {
    let expected_stages = 1 + usize::from(view.program.vert.is_some());
    if view.stages.len() != expected_stages
        || view
            .stages
            .iter()
            .flat_map(|stage| &stage.retained)
            .any(|i| g.concreteness(i) == Concreteness::Missing)
    {
        return None;
    }
    let root = &view.stages.last()?.entry;
    Some(
        skull_codegen_glsl::plan_shader_with_view(ws, scx, view).map_err(|error| {
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

#[derive(Default)]
struct StageProjection {
    retained: BTreeSet<Instance>,
    builtins: BTreeSet<Instance>,
    order: Vec<Instance>,
}

impl StageProjection {
    fn new(ws: &Workspace, graph: &InstanceGraph, entry: &Instance) -> Self {
        let mut projection = Self::default();
        projection.visit(ws, graph, entry);
        projection
    }

    fn visit(&mut self, ws: &Workspace, graph: &InstanceGraph, caller: &Instance) {
        if !self.retained.insert(caller.clone()) {
            return;
        }
        for rec in graph
            .sites_of(caller)
            .into_iter()
            .flat_map(|sites| sites.values())
        {
            let SiteOutcome::Resolved(callee) = &rec.outcome else {
                continue;
            };
            let f = &ws.module(callee.func.module).functions[callee.func.func];
            if skull_codegen_glsl::renders_inline_instance(ws, f, callee, graph.interner()) {
                self.builtins.insert(callee.clone());
            } else if !skull_codegen_glsl::is_lowered_native_operator(
                ws,
                f,
                callee,
                graph.interner(),
            ) {
                self.visit(ws, graph, callee);
            }
        }
        self.order.push(caller.clone());
    }

    fn view(&self, stage: ShaderStage) -> ShaderStageView {
        let (entry, helpers) = self
            .order
            .split_last()
            .expect("stage always retains its entry");
        let mut seen = BTreeSet::new();
        let dependencies = self
            .order
            .iter()
            .filter(|instance| seen.insert(DeclKey(instance.func)))
            .map(|instance| instance.func)
            .collect();
        ShaderStageView {
            stage,
            entry: entry.clone(),
            retained: self.retained.clone(),
            helpers: helpers.iter().cloned().collect(),
            builtins: self.builtins.clone(),
            order: helpers.to_vec(),
            dependencies,
        }
    }
}
