use crate::common::{
    self, body_id, definition_symbols, emit_v1_package, module_id, ws_v1_package,
    ws_with_dependency_module, ws_with_stdlib_module,
};
use skull::mir_emit::emit_full;

const SIM_LIB: &str = r#"
fn register_sim(name: string, body: () -> int) { print("{name}:{body()}") }
fn unrelated_library_body() -> int { return 7 }
"#;
const SIM_SRC: &str = "@sim fn probe_sim() -> int { return 1 }\nfn main() { print(\"hi\") }";

#[test]
fn generated_sim_registration_loads_and_prunes_unrelated_library_code() {
    let ws = ws_with_stdlib_module(SIM_LIB, SIM_SRC);
    let (emit, _) = emit_full(&ws, module_id(&ws, "<test>")).unwrap();
    assert_eq!(common::run_main(&emit.lua), "probe_sim:1\nhi");
    assert!(
        !definition_symbols(&emit.lua)
            .iter()
            .any(|s| s.starts_with("unrelated_library_body"))
    );
}

#[test]
fn shared_prefix_dependency_retains_only_reached_bodies() {
    let ws = ws_with_dependency_module(
        "fn unused_dependency_body() -> int { return 7 }\nfn reached_dependency_body() -> int { return 9 }",
        "fn main() { print(reached_dependency_body()) }",
    );
    let mid = module_id(&ws, "/tmp/rock-mono-project/main.rock");
    let (emit, mut cache) = skull::mir_emit::FullEmit::program()
        .project_root(Some("/tmp/rock-mono-project"))
        .run(&ws, mid)
        .expect("pruned dependency bodies must not require lowering plans");
    assert!(
        !definition_symbols(&emit.lua)
            .iter()
            .any(|s| s.starts_with("unused_dependency_body"))
    );
    assert!(
        definition_symbols(&emit.lua)
            .iter()
            .any(|s| s.starts_with("reached_dependency_body"))
    );

    assert_eq!(common::run_main(&emit.lua), "9");

    let edited = ws_with_dependency_module(
        "fn unused_dependency_body() -> int { return 7 }\nfn reached_dependency_body() -> int { return 9 }",
        "fn main() { print(0) }",
    );
    let incremental =
        skull::mir_emit::emit_incremental(&edited, &mut cache, &[body_id(&edited, "main")])
            .expect("remove the dependency call incrementally");
    let fresh = skull::mir_emit::FullEmit::program()
        .project_root(Some("/tmp/rock-mono-project"))
        .run(&edited, mid)
        .expect("fresh emission after removing the dependency call")
        .0;
    assert_eq!(incremental.lua, fresh.lua);
    assert_eq!(common::run_main(&incremental.lua), "0");
}

#[test]
fn v1_sim_glue_finds_register_sim_by_declared_name() {
    let fragments = [
        ("main.rock", SIM_SRC),
        (
            "engine.rock",
            &format!("mod engine;\npub {}", SIM_LIB.trim_start()),
        ),
    ];
    let emit = emit_v1_package("v1sims", &fragments);
    let registrations: Vec<&str> = emit
        .lua
        .lines()
        .filter(|l| l.starts_with("register_sim_") && l.contains("probe_sim\""))
        .collect();
    assert_eq!(registrations.len(), 1);
}

#[test]
fn v1_sim_glue_rejects_two_register_sim_definitions() {
    let lib = |module: &str| {
        format!(
            "mod {module};\npub fn register_sim(name: string, body: () -> int) {{ print(\"{{name}}\") }}\n"
        )
    };
    let (a, b) = (lib("engine_a"), lib("engine_b"));
    let fragments = [
        ("main.rock", SIM_SRC),
        ("engine_a.rock", &a),
        ("engine_b.rock", &b),
    ];
    let ws = ws_v1_package("v1sims_twice", &fragments);
    let mid = module_id(&ws, "main.rock");
    let Err(error) = skull::mir_emit::emit_workspace_mir(&ws, mid) else {
        panic!("two registrars must not emit");
    };
    assert!(
        error.message.contains("engine_a::register_sim")
            && error.message.contains("engine_b::register_sim")
    );
}

#[test]
fn a_shared_prefix_alone_does_not_validate_the_project_root() {
    let ws = ws_with_dependency_module(
        "fn unused_dependency_body() -> int { return 7 }",
        "fn main() { print(\"hi\") }",
    );
    let mid = module_id(&ws, "/tmp/rock-mono-project/main.rock");
    let result = skull::mir_emit::FullEmit::program()
        .project_root(Some("/tmp/rock-mono"))
        .run(&ws, mid);
    assert!(result.is_err());
}

#[test]
fn nested_project_sources_keep_uncalled_public_functions() {
    let file = "/tmp/rock-root-contract/app/nested/helpers.rock";
    let ws = ws_v1_package(
        "root_contract",
        &[(
            file,
            "mod helpers;\npub fn uncalled_api() -> int { return 7 }",
        )],
    );
    let mid = module_id(&ws, file);
    let (emit, _) = skull::mir_emit::FullEmit::program()
        .project_root(Some("/tmp/rock-root-contract/app"))
        .run(&ws, mid)
        .expect("a nested source validates its project root");
    assert!(
        definition_symbols(&emit.lua)
            .iter()
            .any(|s| s.contains("uncalled"))
    );
}

#[test]
fn shared_prefix_dependency_keeps_bare_name_api_roots() {
    let ws = ws_with_dependency_module(
        "@nomangle fn external_api() -> int { return 7 }",
        "fn main() {}",
    );
    let mid = module_id(&ws, "/tmp/rock-mono-project/main.rock");
    let (emit, _) = skull::mir_emit::FullEmit::program()
        .project_root(Some("/tmp/rock-mono-project"))
        .run(&ws, mid)
        .expect("dependency API emission");
    assert!(
        definition_symbols(&emit.lua)
            .iter()
            .any(|s| s == "external_api")
    );
}
