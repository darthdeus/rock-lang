//! Phase-4 GLSL corpus oracle.
//!
//! The committed canonical rows are the independent expected side for the
//! graph/view driver. Each live row also compares the compatibility emission
//! against `emit_shader_*_with_view`; the row encoding is explicit and never
//! hashes a `Debug` rendering of a plan.

use rock_cli::glsl_test::validate_workspace;
use skull_codegen_glsl::{BlockWire, GlslProfile, ShaderAbiPlan};
use skull_mir::gpu_layout::{AttrKind, SamplerKind, WireKind};
use skull_mir::mir::FuncRef;
use skull_mir::shader_ir::{
    FragmentOutput, ShaderInterfacePlan, ShaderStage, ShaderStageIr, UniformRole, VertexRate,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PROFILES: &[(&str, GlslProfile)] = &[
    ("330core", GlslProfile::Core330),
    ("300es", GlslProfile::Es300),
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RowKey {
    project: String,
    shader: String,
    profile: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShaderRow {
    key: RowKey,
    stage_sections: usize,
    graph_roots: Vec<String>,
    graph_helpers: Vec<String>,
    entries: Vec<String>,
    stage_populations: Vec<Vec<String>>,
    helper_instances: Vec<String>,
    builtin_instances: Vec<String>,
    emission_order: Vec<Vec<String>>,
    glsl_fingerprint: u64,
    interface_fingerprint: u64,
    abi_fingerprint: u64,
    block_wire_fingerprint: u64,
    hot_reload_dependencies: Vec<String>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn discovered_bin_projects(root: &Path) -> Vec<(String, PathBuf)> {
    rock_cli::lint_union::bin_projects_under(root)
        .into_iter()
        .map(|path| {
            let project = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (project, path)
        })
        .collect()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

#[derive(Default)]
struct Canonical(Vec<u8>);

impl Canonical {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str) {
        self.u32(value.len() as u32);
        self.0.extend_from_slice(value.as_bytes());
    }

    fn strings(&mut self, values: &[String]) {
        self.u32(values.len() as u32);
        for value in values {
            self.string(value);
        }
    }

    fn nested_strings(&mut self, values: &[Vec<String>]) {
        self.u32(values.len() as u32);
        for value in values {
            self.strings(value);
        }
    }
}

fn stage_code(stage: ShaderStage) -> u8 {
    match stage {
        ShaderStage::Vertex => 0,
        ShaderStage::Fragment => 1,
        ShaderStage::Compute => 2,
    }
}

fn wire_code(kind: WireKind) -> u8 {
    kind.code() as u8
}

fn attr_code(kind: AttrKind) -> u8 {
    match kind {
        AttrKind::F32 => 0,
        AttrKind::F32x2 => 1,
        AttrKind::F32x3 => 2,
        AttrKind::F32x4 => 3,
        AttrKind::I32 => 4,
    }
}

fn sampler_code(kind: SamplerKind) -> u8 {
    match kind {
        SamplerKind::Float => 0,
        SamplerKind::Int => 1,
    }
}

fn uniform_role_code(role: UniformRole) -> u8 {
    match role {
        UniformRole::Engine => 0,
        UniformRole::Stage(stage) => 1 + stage_code(stage),
    }
}

fn rate_code(rate: VertexRate) -> u8 {
    match rate {
        VertexRate::PerVertex => 0,
        VertexRate::PerInstance => 1,
    }
}

fn interface_bytes(interface: &ShaderInterfacePlan) -> Vec<u8> {
    let mut out = Canonical::default();
    out.u32(interface.stages.len() as u32);
    for stage in &interface.stages {
        out.u8(stage_code(*stage));
    }
    out.u32(interface.vertex_inputs.len() as u32);
    for input in &interface.vertex_inputs {
        out.string(input.struct_name.key.as_str());
        out.u8(rate_code(input.rate));
        out.u32(input.attrs.len() as u32);
        for (name, kind) in &input.attrs {
            out.string(name.as_str());
            out.u8(attr_code(*kind));
        }
    }
    out.u32(interface.varyings.len() as u32);
    for varying in &interface.varyings {
        out.string(varying.name.as_str());
        out.u8(wire_code(varying.kind));
        out.u8(varying.flat as u8);
    }
    out.u32(interface.uniform_groups.len() as u32);
    for group in &interface.uniform_groups {
        out.u8(uniform_role_code(group.role));
        out.string(group.struct_name.key.as_str());
        out.u32(group.members.len() as u32);
        for (name, kind) in &group.members {
            out.string(name.as_str());
            out.u8(wire_code(*kind));
        }
    }
    out.u32(interface.array_groups.len() as u32);
    for group in &interface.array_groups {
        out.u8(stage_code(group.stage));
        out.string(group.owner.as_str());
        out.u32(group.members.len() as u32);
        for member in &group.members {
            out.string(member.name.as_str());
            out.u8(wire_code(member.elem));
            out.u32(member.capacity);
        }
    }
    match &interface.textures {
        None => out.u8(0),
        Some(group) => {
            out.u8(1);
            out.string(group.struct_name.map_or("", |name| name.key.as_str()));
            out.u32(group.samplers.len() as u32);
            for (name, kind) in &group.samplers {
                out.string(name.as_str());
                out.u8(sampler_code(*kind));
            }
            out.u32(group.stages.len() as u32);
            for stage in &group.stages {
                out.u8(stage_code(*stage));
            }
        }
    }
    out.u32(interface.bare_samplers.len() as u32);
    for sampler in &interface.bare_samplers {
        out.string(sampler.name.as_str());
        out.u8(sampler_code(sampler.kind));
        out.u8(stage_code(sampler.stage));
    }
    match &interface.fragment_output {
        FragmentOutput::None => out.u8(0),
        FragmentOutput::Single(kind) => {
            out.u8(1);
            out.u8(wire_code(*kind));
        }
        FragmentOutput::Mrt(fields) => {
            out.u8(2);
            out.u32(fields.len() as u32);
            for (name, kind) in fields {
                out.string(name.as_str());
                out.u8(wire_code(*kind));
            }
        }
    }
    out.0
}

fn abi_bytes(abi: &ShaderAbiPlan) -> Vec<u8> {
    let mut out = Canonical::default();
    out.u32(abi.uniform_blocks.len() as u32);
    for (binding, block) in &abi.uniform_blocks {
        let block = &block.plan;
        out.u32(*binding);
        out.string(block.struct_name.as_str());
        out.u32(block.size);
        out.u32(block.members.len() as u32);
        for member in &block.members {
            out.string(member.name.as_str());
            out.u8(wire_code(member.kind));
            out.u32(member.offset);
            match member.matrix_stride {
                Some(stride) => {
                    out.u8(1);
                    out.u32(stride);
                }
                None => out.u8(0),
            }
        }
    }
    out.u32(abi.uniform_array_blocks.len() as u32);
    for (binding, block) in &abi.uniform_array_blocks {
        out.u32(*binding);
        out.string(block.block_name.as_str());
        out.u32(block.size);
        out.u32(block.members.len() as u32);
        for member in &block.members {
            out.string(member.name.as_str());
            out.u8(wire_code(member.elem));
            out.u32(member.capacity);
            out.u32(member.offset);
            out.u32(member.stride);
        }
    }
    out.u32(abi.texture_units.len() as u32);
    for name in &abi.texture_units {
        out.string(name.as_str());
    }
    out.u32(abi.attribute_locations.len() as u32);
    for (name, location) in &abi.attribute_locations {
        out.string(name.as_str());
        out.u32(*location);
    }
    out.0
}

fn block_wire_bytes(abi: &ShaderAbiPlan) -> Vec<u8> {
    let wire = BlockWire::of(abi.uniform_blocks.iter(), abi.uniform_array_blocks.iter());
    let mut out = Canonical::default();
    out.u32(wire.names.len() as u32);
    for name in &wire.names {
        out.string(name);
    }
    out.u32(wire.bindings.len() as u32);
    for value in &wire.bindings {
        out.u64(*value as i64 as u64);
    }
    out.u32(wire.sizes.len() as u32);
    for value in &wire.sizes {
        out.u64(*value as i64 as u64);
    }
    out.u32(wire.fingerprints.len() as u32);
    for value in &wire.fingerprints {
        out.u64(*value as i64 as u64);
    }
    out.u32(wire.counts.len() as u32);
    for value in &wire.counts {
        out.u64(*value as i64 as u64);
    }
    out.u32(wire.member_names.len() as u32);
    for name in &wire.member_names {
        out.string(name);
    }
    for values in [
        &wire.member_offsets,
        &wire.member_kinds,
        &wire.member_capacities,
        &wire.member_strides,
    ] {
        out.u32(values.len() as u32);
        for value in values.iter() {
            out.u64(*value as i64 as u64);
        }
    }
    out.0
}

fn source_stages(source: &str) -> usize {
    let vertex = source.matches("#ifdef VERTEX").count();
    let fragment = source.matches("#ifdef PIXEL").count();
    assert_eq!(
        vertex, 1,
        "every emitted shader must have one VERTEX section"
    );
    assert_eq!(
        fragment, 1,
        "every emitted shader must have one PIXEL section"
    );
    vertex + fragment
}

fn fref_key(ws: &bone_semantic::inference::Workspace, fref: FuncRef) -> String {
    let f = &ws.module(fref.module).functions[fref.func];
    format!(
        "{}@{}.{}",
        f.name,
        fref.module.raw(),
        fref.func.into_raw().into_u32()
    )
}

fn instance_key(
    ws: &bone_semantic::inference::Workspace,
    graph: &skull::instance_graph::InstanceGraph,
    instance: &skull::instance_graph::Instance,
) -> String {
    graph.identity(ws, instance)
}

fn instance_keys<'a>(
    ws: &bone_semantic::inference::Workspace,
    graph: &skull::instance_graph::InstanceGraph,
    instances: impl IntoIterator<Item = &'a skull::instance_graph::Instance>,
) -> Vec<String> {
    instances
        .into_iter()
        .map(|i| instance_key(ws, graph, i))
        .collect()
}

fn sorted_instance_keys<'a>(
    ws: &bone_semantic::inference::Workspace,
    graph: &skull::instance_graph::InstanceGraph,
    instances: impl IntoIterator<Item = &'a skull::instance_graph::Instance>,
) -> Vec<String> {
    let mut out = instance_keys(ws, graph, instances);
    out.sort();
    out
}

fn emit_pair(
    ws: &bone_semantic::inference::Workspace,
    pair: &skull_mir::lower::ShaderPair,
    profile: GlslProfile,
) -> skull_codegen_glsl::ShaderEmission {
    let fragment = ws.module(pair.frag.module).functions[pair.frag.func].name;
    match pair.vert {
        Some(vertex) => {
            let vertex_name = ws.module(vertex.module).functions[vertex.func].name;
            skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
                ws,
                vertex.module,
                vertex_name.as_str(),
                fragment.as_str(),
                profile,
            )
        }
        None => skull_codegen_glsl::emit_shader_entry_with_dependencies(
            ws,
            pair.frag.module,
            fragment.as_str(),
            profile,
        ),
    }
    .unwrap_or_else(|e| panic!("`{}` failed to emit: {e}", pair.key))
}

fn emit_pair_with_view(
    ws: &bone_semantic::inference::Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    view: &skull::shader_artifact::ShaderProgramView,
    profile: GlslProfile,
) -> skull_codegen_glsl::ShaderEmission {
    let emission = match view.program.vert {
        Some(_) => skull_codegen_glsl::emit_shader_pipeline_with_view(ws, scx, view, profile),
        None => skull_codegen_glsl::emit_shader_entry_with_view(ws, scx, view, profile),
    };
    emission.unwrap_or_else(|e| panic!("view emission failed for {:?}: {e}", view.program))
}

fn target_for(profile: GlslProfile) -> skull::target_projection::TargetSpec {
    skull::target_projection::TargetSpec {
        shader: Some(skull::target_projection::ShaderTargetSpec::Glsl(profile)),
        ..skull::target_projection::LUAJIT_DESKTOP
    }
}

fn snapshot_project(project: &str, path: &Path) -> Vec<ShaderRow> {
    let ws = validate_workspace(path.to_str().unwrap()).expect("workspace");
    let pairs = skull_mir::lower::collect_shader_pairs(&ws).expect("shader pairs");
    let programs: Vec<skull::shader_artifact::ShaderProgram> = pairs
        .iter()
        .map(|pair| skull::shader_artifact::ShaderProgram {
            vert: pair.vert,
            frag: pair.frag,
        })
        .collect();
    let roots: Vec<FuncRef> = programs
        .iter()
        .flat_map(|program| {
            program
                .vert
                .into_iter()
                .chain(std::iter::once(program.frag))
        })
        .collect();
    let graph = skull::instance_graph::build(&ws, roots, false);
    let scx = skull_mir::lower::ScalarCtx::new_values_first_class(&ws)
        .sharing(Arc::clone(graph.interner()));
    let mut rows = Vec::new();

    for (profile_name, profile) in PROFILES {
        let view = skull::shader_artifact::realize_shader_with_context(
            &ws,
            &graph,
            &skull::shader_artifact::ShaderArtifactRequest {
                spec: target_for(*profile),
                programs: &programs,
            },
            &scx,
        )
        .expect("GLSL view");
        assert_eq!(
            view.program_views.len(),
            pairs.len(),
            "program-view denominator moved"
        );

        for (index, pair) in pairs.iter().enumerate() {
            let emission = emit_pair(&ws, pair, *profile);
            let program_view = &view.program_views[index];
            let key = RowKey {
                project: project.to_string(),
                shader: pair.key.clone(),
                profile: *profile_name,
            };
            assert_eq!(
                program_view.program, programs[index],
                "{project}: program view key changed for {} [{profile_name}]",
                pair.key
            );
            let view_emission = emit_pair_with_view(&ws, &scx, program_view, *profile);
            assert_emissions_match(&ws, &graph, &key, &emission, &view_emission, program_view);
            assert_view_matches_emission(&ws, &graph, &key, &emission, program_view);

            let mut stage_populations = Vec::new();
            let mut emission_order = Vec::new();
            let mut entries = Vec::new();
            let mut helpers = BTreeSet::new();
            let mut builtins = BTreeSet::new();
            let mut dependencies = BTreeSet::new();
            for stage in &program_view.stages {
                entries.push(instance_key(&ws, &graph, &stage.entry));
                stage_populations.push(sorted_instance_keys(&ws, &graph, &stage.retained));
                emission_order.push(instance_keys(&ws, &graph, stage.order.iter()));
                helpers.extend(stage.helpers.iter().map(|i| instance_key(&ws, &graph, i)));
                builtins.extend(stage.builtins.iter().map(|i| instance_key(&ws, &graph, i)));
                dependencies.extend(stage.dependencies.iter().map(|f| fref_key(&ws, *f)));
            }
            rows.push(ShaderRow {
                key,
                stage_sections: source_stages(&emission.source),
                graph_roots: sorted_instance_keys(&ws, &graph, view.roots.iter()),
                graph_helpers: sorted_instance_keys(&ws, &graph, view.helpers.iter()),
                entries,
                stage_populations,
                helper_instances: helpers.into_iter().collect(),
                builtin_instances: builtins.into_iter().collect(),
                emission_order,
                glsl_fingerprint: fnv1a(emission.source.as_bytes()),
                interface_fingerprint: fnv1a(&interface_bytes(&emission.interface)),
                abi_fingerprint: fnv1a(&abi_bytes(&emission.abi)),
                block_wire_fingerprint: fnv1a(&block_wire_bytes(&emission.abi)),
                hot_reload_dependencies: dependencies.into_iter().collect(),
            });
        }
    }
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    rows
}

fn sorted_dependency_keys(
    dependencies: impl IntoIterator<Item = FuncRef>,
) -> Vec<(u32, u32)> {
    let mut out: Vec<_> = dependencies
        .into_iter()
        .map(|f| (f.module.raw(), f.func.into_raw().into_u32()))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn legacy_builtin_instances(
    ws: &bone_semantic::inference::Workspace,
    graph: &skull::instance_graph::InstanceGraph,
    stage: &ShaderStageIr,
) -> BTreeSet<skull::instance_graph::Instance> {
    let callers = stage.order.iter().chain(std::iter::once(&stage.entry));
    let mut builtins = BTreeSet::new();
    for caller in callers {
        let Some(sites) = graph.sites_of(&caller.inst) else { continue };
        for record in sites.values() {
            let skull::instance_graph::SiteOutcome::Resolved(callee) = &record.outcome else {
                continue;
            };
            let f = &ws.module(callee.func.module).functions[callee.func.func];
            if skull_codegen_glsl::renders_inline_instance(ws, f, callee, graph.interner()) {
                builtins.insert(callee.clone());
            }
        }
    }
    builtins
}

fn assert_stage_ir_equal(
    key: &RowKey,
    expected: &ShaderStageIr,
    actual: &ShaderStageIr,
) {
    assert_eq!(actual.stage, expected.stage, "{key:?}: view stage IR stage differs");
    assert_eq!(actual.entry, expected.entry, "{key:?}: view stage IR entry differs");
    assert_eq!(actual.order, expected.order, "{key:?}: view stage emission order differs");
    let mut actual_helpers: Vec<_> = actual
        .bodies
        .keys()
        .map(|instance| format!("{instance:?}"))
        .collect();
    let mut expected_helpers: Vec<_> = expected
        .bodies
        .keys()
        .map(|instance| format!("{instance:?}"))
        .collect();
    actual_helpers.sort();
    expected_helpers.sort();
    assert_eq!(
        actual_helpers, expected_helpers,
        "{key:?}: view helper population differs"
    );
}

fn assert_emissions_match(
    ws: &bone_semantic::inference::Workspace,
    graph: &skull::instance_graph::InstanceGraph,
    key: &RowKey,
    expected: &skull_codegen_glsl::ShaderEmission,
    actual: &skull_codegen_glsl::ShaderEmission,
    program_view: &skull::shader_artifact::ShaderProgramView,
) {
    assert_eq!(actual.source, expected.source, "{key:?}: view GLSL differs");
    assert_eq!(actual.interface, expected.interface, "{key:?}: view interface differs");
    assert_eq!(
        abi_bytes(&actual.abi),
        abi_bytes(&expected.abi),
        "{key:?}: view ABI differs"
    );
    assert_eq!(
        block_wire_bytes(&actual.abi),
        block_wire_bytes(&expected.abi),
        "{key:?}: view BlockWire differs"
    );
    assert_eq!(
        sorted_dependency_keys(actual.dependencies.iter().copied()),
        sorted_dependency_keys(expected.dependencies.iter().copied()),
        "{key:?}: view dependencies differ"
    );
    assert_eq!(
        actual.ir.stages.len(),
        expected.ir.stages.len(),
        "{key:?}: view stage IR denominator differs"
    );
    assert_eq!(
        actual.ir.stages.len(),
        program_view.stages.len(),
        "{key:?}: view stage population denominator differs"
    );
    for ((expected_stage, actual_stage), view_stage) in expected
        .ir
        .stages
        .iter()
        .zip(&actual.ir.stages)
        .zip(&program_view.stages)
    {
        assert_stage_ir_equal(key, expected_stage, actual_stage);
        assert_eq!(
            view_stage.builtins,
            legacy_builtin_instances(ws, graph, expected_stage),
            "{key:?}: view builtin population differs"
        );
    }
}

fn assert_view_matches_emission(
    ws: &bone_semantic::inference::Workspace,
    graph: &skull::instance_graph::InstanceGraph,
    key: &RowKey,
    emission: &skull_codegen_glsl::ShaderEmission,
    program_view: &skull::shader_artifact::ShaderProgramView,
) {
    assert_eq!(
        program_view.stages.len(),
        emission.ir.stages.len(),
        "{key:?}: graph/emitter stage population denominator differs"
    );
    for (view_stage, emitted_stage) in program_view.stages.iter().zip(&emission.ir.stages) {
        assert_eq!(
            view_stage.stage, emitted_stage.stage,
            "{key:?}: graph/emitter stage key differs"
        );
        assert_eq!(
            view_stage.entry, emitted_stage.entry.inst,
            "{key:?}: graph/emitter entry instance differs"
        );
        let expected_retained: BTreeSet<_> = std::iter::once(&emitted_stage.entry.inst)
            .chain(emitted_stage.order.iter().map(|i| &i.inst))
            .cloned()
            .collect();
        let view_retained_keys = sorted_instance_keys(ws, graph, view_stage.retained.iter());
        let expected_retained_keys = sorted_instance_keys(ws, graph, expected_retained.iter());
        assert_eq!(
            view_retained_keys, expected_retained_keys,
            "{key:?}: graph/emitter retained stage population differs"
        );
        let expected_helpers: BTreeSet<_> =
            emitted_stage.order.iter().map(|i| i.inst.clone()).collect();
        assert_eq!(
            view_stage.helpers, expected_helpers,
            "{key:?}: graph/emitter helper population differs"
        );
        let expected_order: Vec<_> = emitted_stage.order.iter().map(|i| i.inst.clone()).collect();
        assert_eq!(
            view_stage.order, expected_order,
            "{key:?}: graph/emitter helper emission order differs"
        );
        assert_eq!(
            view_stage.dependencies,
            emitted_stage.declarations(),
            "{key:?}: graph/emitter hot-reload dependencies differ"
        );
        assert!(
            view_stage
                .retained
                .iter()
                .all(|instance| graph.concreteness(instance)
                    != skull::instance_graph::Concreteness::Missing),
            "{key:?}: retained shader population contains a missing graph substitution"
        );
    }
    assert!(
        !program_view.stages.is_empty(),
        "{key:?}: shader has no graph stages"
    );
}

fn changed_keys(before: &[ShaderRow], after: &[ShaderRow]) -> Vec<RowKey> {
    let before_keys: BTreeSet<_> = before.iter().map(|row| row.key.clone()).collect();
    let after_keys: BTreeSet<_> = after.iter().map(|row| row.key.clone()).collect();
    assert_eq!(
        before_keys, after_keys,
        "shader mutation added or removed a row"
    );
    before
        .iter()
        .zip(after)
        .filter(|(a, b)| *a != *b)
        .map(|(a, _)| a.key.clone())
        .collect()
}

fn canonical_fingerprint(row: &ShaderRow) -> u64 {
    let mut out = Canonical::default();
    out.string(&row.key.project);
    out.string(&row.key.shader);
    out.string(row.key.profile);
    out.u64(row.stage_sections as u64);
    out.strings(&row.graph_roots);
    out.strings(&row.graph_helpers);
    out.strings(&row.entries);
    out.nested_strings(&row.stage_populations);
    out.strings(&row.helper_instances);
    out.strings(&row.builtin_instances);
    out.nested_strings(&row.emission_order);
    out.u64(row.glsl_fingerprint);
    out.u64(row.interface_fingerprint);
    out.u64(row.abi_fingerprint);
    out.u64(row.block_wire_fingerprint);
    out.strings(&row.hot_reload_dependencies);
    fnv1a(&out.0)
}

fn canonical_baseline() -> BTreeMap<RowKey, u64> {
    let mut out = BTreeMap::new();
    for line in include_str!("shader_corpus_canonical.tsv").lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let project = fields.next().expect("canonical project").to_string();
        let shader = fields.next().expect("canonical shader").to_string();
        let profile = match fields.next().expect("canonical profile") {
            "330core" => "330core",
            "300es" => "300es",
            other => panic!("unknown canonical profile: {other}"),
        };
        let fingerprint = u64::from_str_radix(fields.next().expect("canonical fingerprint"), 16)
            .expect("canonical fingerprint is hex");
        assert!(
            fields.next().is_none(),
            "canonical row has extra fields: {line}"
        );
        assert!(out
            .insert(
                RowKey {
                    project,
                    shader,
                    profile
                },
                fingerprint
            )
            .is_none());
    }
    out
}

fn shader_fixture_source(helper_a_body: &str) -> String {
    format!(
        r#"
@shader(fragment)
fn oracle_shader_a(tc: Vec2) -> Vec4 {{
    let value = oracle_helper_a(tc.x)
    return Vec4 {{ x: value, y: 0.0, z: 0.0, w: 1.0 }}
}}

fn oracle_helper_a(value: float) -> float {{ {helper_a_body} }}

@shader(fragment)
fn oracle_shader_b(tc: Vec2) -> Vec4 {{
    let value = oracle_helper_b(tc.y)
    return Vec4 {{ x: 0.0, y: value, z: 0.0, w: 1.0 }}
}}

fn oracle_helper_b(value: float) -> float {{ return value + 3.0 }}

global oracle_shader_a_registration: string = rock_shader(oracle_shader_a)
global oracle_shader_b_registration: string = rock_shader(oracle_shader_b)
"#,
    )
}

#[test]
fn shader_source_mutation_names_only_the_changed_shader_profiles() {
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let source = temp.path().join("shaders.rock");
    fs::write(&source, shader_fixture_source("return value + 1.0")).expect("baseline source");
    let baseline = snapshot_project("fixture", temp.path());
    fs::write(&source, shader_fixture_source("return value + 2.0")).expect("candidate source");
    let candidate = snapshot_project("fixture", temp.path());

    let baseline_keys: BTreeSet<_> = baseline.iter().map(|row| row.key.clone()).collect();
    assert_eq!(
        baseline_keys.len(),
        4,
        "two shaders x two profiles must be compared"
    );
    assert_eq!(
        baseline_keys,
        candidate.iter().map(|row| row.key.clone()).collect(),
        "helper mutation must not add or remove a project/shader/profile row"
    );
    let changed = changed_keys(&baseline, &candidate);
    let expected = BTreeSet::from([
        RowKey {
            project: "fixture".into(),
            shader: "oracle_shader_a".into(),
            profile: "330core",
        },
        RowKey {
            project: "fixture".into(),
            shader: "oracle_shader_a".into(),
            profile: "300es",
        },
    ]);
    assert_eq!(
        changed.iter().cloned().collect::<BTreeSet<_>>(),
        expected,
        "helper A source mutation changed the wrong exact project/shader/profile rows: {changed:?}"
    );
    for row in &candidate {
        if row.key.shader == "oracle_shader_b" {
            let old = baseline
                .iter()
                .find(|before| before.key == row.key)
                .unwrap();
            assert_eq!(
                old, row,
                "shader B changed under a helper A-only mutation: {:?}",
                row.key
            );
        }
    }
    for row in candidate
        .iter()
        .filter(|row| row.key.shader == "oracle_shader_a")
    {
        let old = baseline
            .iter()
            .find(|before| before.key == row.key)
            .unwrap();
        assert_ne!(
            old.glsl_fingerprint, row.glsl_fingerprint,
            "shader A source did not change: {:?}",
            row.key
        );
        assert_eq!(
            old.interface_fingerprint, row.interface_fingerprint,
            "shader A interface moved: {:?}",
            row.key
        );
        assert_eq!(
            old.abi_fingerprint, row.abi_fingerprint,
            "shader A ABI moved: {:?}",
            row.key
        );
        assert_eq!(
            old.block_wire_fingerprint, row.block_wire_fingerprint,
            "shader A block wire moved: {:?}",
            row.key
        );
        assert_eq!(
            old.graph_roots, row.graph_roots,
            "shader A graph roots moved: {:?}",
            row.key
        );
        assert_eq!(
            old.stage_populations, row.stage_populations,
            "shader A population moved: {:?}",
            row.key
        );
    }
}

#[test]
fn shader_project_mutation_names_only_the_changed_project_profiles() {
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let chesslings = temp.path().join("chesslings");
    let wgpu_renderer = temp.path().join("wgpu_renderer");
    fs::create_dir_all(&chesslings).expect("chesslings fixture");
    fs::create_dir_all(&wgpu_renderer).expect("wgpu renderer fixture");
    fs::write(
        chesslings.join("shaders.rock"),
        shader_fixture_source("return value + 1.0"),
    )
    .expect("chesslings source");
    fs::write(
        wgpu_renderer.join("shaders.rock"),
        shader_fixture_source("return value + 1.0"),
    )
    .expect("wgpu renderer source");

    let mut baseline = snapshot_project("games/chesslings", &chesslings);
    baseline.extend(snapshot_project("games/wgpu_renderer", &wgpu_renderer));
    baseline.sort_by(|a, b| a.key.cmp(&b.key));

    fs::write(
        wgpu_renderer.join("shaders.rock"),
        shader_fixture_source("return value + 2.0"),
    )
    .expect("mutated wgpu renderer source");
    let mut candidate = snapshot_project("games/chesslings", &chesslings);
    candidate.extend(snapshot_project("games/wgpu_renderer", &wgpu_renderer));
    candidate.sort_by(|a, b| a.key.cmp(&b.key));

    let changed = changed_keys(&baseline, &candidate);
    let expected = BTreeSet::from([
        RowKey {
            project: "games/wgpu_renderer".into(),
            shader: "oracle_shader_a".into(),
            profile: "300es",
        },
        RowKey {
            project: "games/wgpu_renderer".into(),
            shader: "oracle_shader_a".into(),
            profile: "330core",
        },
    ]);
    assert_eq!(
        changed.iter().cloned().collect::<BTreeSet<_>>(),
        expected,
        "a project-local source mutation changed the wrong project/shader/profile rows: {changed:?}"
    );
    for row in baseline
        .iter()
        .filter(|row| row.key.project == "games/chesslings")
    {
        let candidate_row = candidate
            .iter()
            .find(|candidate| candidate.key == row.key)
            .unwrap();
        assert_eq!(row, candidate_row, "unmutated project changed: {:?}", row.key);
    }
}

#[test]
fn shader_oracle_rejects_a_missing_stage_population() {
    let temp = tempfile::tempdir().expect("fixture tempdir");
    fs::write(
        temp.path().join("shaders.rock"),
        shader_fixture_source("return value + 1.0"),
    )
    .expect("fixture source");
    let expected = snapshot_project("fixture", temp.path());
    let mut candidate = expected.clone();
    let target = candidate
        .iter_mut()
        .find(|row| row.key.shader == "oracle_shader_a" && row.key.profile == "330core")
        .expect("stage-population mutation target");
    assert!(
        target.stage_populations[0].len() > 1,
        "fixture needs a retained helper control"
    );
    target.stage_populations[0].pop();
    let changed = changed_keys(&expected, &candidate);
    assert_eq!(
        changed,
        vec![RowKey {
            project: "fixture".into(),
            shader: "oracle_shader_a".into(),
            profile: "330core",
        }],
        "a missing stage instance must name its exact project/shader/profile row"
    );
}

#[test]
fn shader_oracle_rejects_a_view_population_missing_from_emission() {
    let temp = tempfile::tempdir().expect("fixture tempdir");
    fs::write(
        temp.path().join("shaders.rock"),
        shader_fixture_source("return value + 1.0"),
    )
    .expect("fixture source");
    let ws = validate_workspace(temp.path().to_str().unwrap()).expect("workspace");
    let pairs = skull_mir::lower::collect_shader_pairs(&ws).expect("shader pairs");
    let programs: Vec<skull::shader_artifact::ShaderProgram> = pairs
        .iter()
        .map(|pair| skull::shader_artifact::ShaderProgram {
            vert: pair.vert,
            frag: pair.frag,
        })
        .collect();
    let roots: Vec<FuncRef> = programs
        .iter()
        .flat_map(|program| {
            program
                .vert
                .into_iter()
                .chain(std::iter::once(program.frag))
        })
        .collect();
    let graph = skull::instance_graph::build(&ws, roots, false);
    let profile = GlslProfile::Core330;
    let view = skull::shader_artifact::realize_shader(
        &ws,
        &graph,
        &skull::shader_artifact::ShaderArtifactRequest {
            spec: target_for(profile),
            programs: &programs,
        },
    )
    .expect("GLSL view");
    let index = pairs
        .iter()
        .position(|pair| pair.key == "oracle_shader_a")
        .expect("fixture shader A");
    let emission = emit_pair(&ws, &pairs[index], profile);
    let key = RowKey {
        project: "fixture".into(),
        shader: "oracle_shader_a".into(),
        profile: "330core",
    };
    let mut broken = view.program_views[index].clone();
    let entry = broken.stages[0].entry.clone();
    let removed = broken.stages[0]
        .retained
        .iter()
        .find(|instance| *instance != &entry)
        .cloned()
        .expect("fixture helper population");
    assert!(broken.stages[0].retained.remove(&removed));
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_view_matches_emission(&ws, &graph, &key, &emission, &broken);
    }))
    .expect_err("a missing stage instance must make the graph/view oracle red");
    let failure = failure
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| failure.downcast_ref::<&str>().map(ToString::to_string))
        .unwrap_or_default();
    assert!(
        failure.contains(&format!("{key:?}")),
        "the rejection must name its exact project/shader/profile key: {failure}"
    );
}

#[test]
fn snapshot_project_resolves_shader_sim_helpers_in_marker_free_test_files() {
    let outer = tempfile::tempdir().expect("fixture tempdir");
    let project = outer.path().join("sim-helper-project");
    let tests_dir = project.join("tests");
    std::fs::create_dir_all(&tests_dir).expect("fixture tests dir");
    std::fs::write(
        project.join("stick.toml"),
        "[package]\nname = \"sim-helper-project\"\nversion = \"0.1.0\"\ntype = \"lib\"\n",
    )
    .expect("fixture manifest");
    std::fs::write(project.join("main.rock"), "fn main() {}\n")
        .expect("fixture production source");
    std::fs::write(
        tests_dir.join("helper.rock"),
        "fn oracle_cross_file_helper(value: float) -> float { return value + 1.0 }\n",
    )
    .expect("fixture helper");
    std::fs::write(
        tests_dir.join("sim.rock"),
        r#"
@shader(fragment)
fn oracle_sim_shader(tc: Vec2) -> Vec4 {
    let value = oracle_cross_file_helper(tc.x)
    return Vec4 { x: value, y: 0.0, z: 0.0, w: 1.0 }
}

global oracle_sim_shader_registration: string = rock_shader(oracle_sim_shader)
"#,
    )
    .expect("fixture sim");
    let rows = snapshot_project("fixture", &project);
    assert_eq!(
        rows.iter().map(|row| row.key.clone()).collect::<BTreeSet<_>>(),
        BTreeSet::from([
            RowKey {
                project: "fixture".into(),
                shader: "oracle_sim_shader".into(),
                profile: "330core",
            },
            RowKey {
                project: "fixture".into(),
                shader: "oracle_sim_shader".into(),
                profile: "300es",
            },
        ]),
        "one sim-registered shader x two profiles"
    );
}

#[test]
#[ignore = "real 57-project Phase-4 corpus oracle"]
fn shader_corpus_has_per_shader_profile_and_interface_evidence() {
    let root = repo_root();
    let projects = discovered_bin_projects(&root);
    let examined = projects.len();
    let mut rows = Vec::new();
    let mut shader_bearing = 0;
    let mut shaderless = 0;
    let mut observed_projects = BTreeMap::new();
    for (project, path) in &projects {
        let project_rows = snapshot_project(project, path);
        if project_rows.is_empty() {
            shaderless += 1;
        } else {
            shader_bearing += 1;
        }
        observed_projects.insert(project.as_str(), project_rows.len());
        rows.extend(project_rows);
    }
    rows.sort_by(|a, b| a.key.cmp(&b.key));

    assert_eq!(examined, 57, "bin-project population moved");
    assert_eq!(shader_bearing, 47, "shader-bearing project population moved");
    assert_eq!(shaderless, 10, "shaderless project population moved");
    assert_eq!(examined, shader_bearing + shaderless, "project denominators do not partition");
    let shader_keys: BTreeSet<(&str, &str)> = rows
        .iter()
        .map(|row| (row.key.project.as_str(), row.key.shader.as_str()))
        .collect();
    assert_eq!(shader_keys.len(), 1137, "shader population moved");
    assert_eq!(rows.len(), 2274, "profile row population moved");
    assert_eq!(
        rows.iter().map(|row| row.stage_sections).sum::<usize>(),
        4548,
        "stage-section denominator moved"
    );
    assert!(
        rows.iter()
            .map(|row| row.graph_helpers.len())
            .sum::<usize>()
            > 0,
        "graph helper population is empty"
    );
    assert!(
        rows.iter()
            .map(|row| row.helper_instances.len())
            .sum::<usize>()
            > 0,
        "helper instance population is empty"
    );
    assert!(
        rows.iter()
            .flat_map(|row| row.emission_order.iter())
            .map(Vec::len)
            .sum::<usize>()
            > 0,
        "emission-order population is empty"
    );
    assert_eq!(
        rows.iter()
            .map(|row| row.key.profile)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["300es", "330core"]),
        "both GLSL profiles must be represented"
    );

    for row in &rows {
        assert!(
            !row.entries.is_empty(),
            "empty concrete entry population: {:?}",
            row.key
        );
        assert!(
            row.stage_populations
                .iter()
                .all(|population| !population.is_empty()),
            "empty per-stage retained population: {:?}",
            row.key
        );
        assert_ne!(
            row.glsl_fingerprint, 0,
            "empty GLSL fingerprint: {:?}",
            row.key
        );
        assert_ne!(
            row.interface_fingerprint, 0,
            "empty interface fingerprint: {:?}",
            row.key
        );
        assert_ne!(
            row.abi_fingerprint, 0,
            "empty ABI fingerprint: {:?}",
            row.key
        );
        assert_ne!(
            row.block_wire_fingerprint, 0,
            "empty BlockWire fingerprint: {:?}",
            row.key
        );
        assert!(
            !row.hot_reload_dependencies.is_empty(),
            "empty hot-reload dependencies: {:?}",
            row.key
        );
    }
    assert_eq!(
        observed_projects
            .values()
            .filter(|count| **count > 0)
            .count(),
        shader_bearing
    );
    assert_eq!(
        observed_projects
            .values()
            .filter(|count| **count == 0)
            .count(),
        shaderless
    );

    let actual: BTreeMap<_, _> = rows
        .iter()
        .map(|row| (row.key.clone(), canonical_fingerprint(row)))
        .collect();
    let expected = canonical_baseline();
    assert_eq!(
        actual.keys().collect::<BTreeSet<_>>(),
        expected.keys().collect::<BTreeSet<_>>(),
        "canonical row keys moved"
    );
    for row in &rows {
        assert_eq!(
            actual[&row.key], expected[&row.key],
            "canonical shader row changed for exact project/shader/profile key {:?}",
            row.key
        );
    }
}
