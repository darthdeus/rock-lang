use bone_semantic::hir::Function;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use rock_host::stdlib_loader::stdlib_files;
use skull_codegen_shader::{ShaderPlannerPolicy, plan_fullscreen_fragment, resolve_root};
use skull_mir::intrinsics::is_intrinsic;
use skull_parser::{parse_module_clean, parse_module_clean_with_dialect, Dialect, PackageTag};
use ustr::Ustr;

static BLIT: &str = include_str!("../../../stone/engine/shaders/post_processing/blit_shader.rock");

struct NeutralPolicy;

impl ShaderPlannerPolicy for NeutralPolicy {
    fn inline_call(&self, function: &Function) -> bool {
        is_intrinsic(function)
            || (function.name.as_str().starts_with("__op_") && function.param_decls.len() == 2)
    }

    fn backend_name(&self) -> &'static str {
        "neutral-test"
    }
}

fn workspace(source: &str) -> (Workspace, ModuleId) {
    let mut modules = stdlib_files()
        .iter()
        .map(|file| {
            parse_module_clean(
                file.contents,
                Ustr::from(format!("<stdlib:{}>", file.name).as_str()),
            )
            .expect("stdlib parse")
        })
        .collect::<Vec<_>>();
    // The shader sources are Stone's, so they parse in the dialect Stone
    // declares and, under v1, carry its package tag as the host would stamp.
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../stone/stick.toml");
    let dialect = rock_host::stick::StickConfig::from_path(&manifest).expect("stone manifest").dialect();
    let mut shader = parse_module_clean_with_dialect(source, Ustr::from("<test>"), dialect).expect("shader parse");
    if dialect == Dialect::V1 {
        shader.package = Some(PackageTag { id: 0, self_name: "stone".to_string() });
    }
    modules.push(shader);
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic pass");
    let mid = *ws
        .module_ids
        .get(&Ustr::from("<test>"))
        .expect("test module");
    (ws, mid)
}

#[test]
fn fullscreen_artifact_is_planned_without_a_glsl_backend() {
    let source = format!(
        "{}\n{}",
        include_str!("../../../stone/engine/shaders/common/pass_textures.rock"),
        BLIT
    );
    let (ws, mid) = workspace(&source);
    // A v1 root is named by its definition key (`rock shader` takes the same).
    let key = ws
        .module(mid)
        .functions
        .iter()
        .map(|(_, f)| f.name)
        .find(|name| name.as_str().rsplit("::").next() == Some("blit_frag"))
        .expect("blit_frag is defined");
    let root = resolve_root(&ws, mid, key.as_str()).expect("root");
    let artifact = plan_fullscreen_fragment(&ws, root, &NeutralPolicy).expect("neutral artifact");

    assert_eq!(artifact.interface.stages.len(), 1);
    assert_eq!(artifact.interface.stages[0].name(), "fragment");
    assert_eq!(artifact.interface.sampler_names().len(), 1);
    assert_eq!(artifact.ir.stages.len(), 1);
    assert_eq!(artifact.dependencies, artifact.ir.declarations());
    assert_eq!(artifact.dependencies.len(), 1);
}
