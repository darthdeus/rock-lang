use crate::common::{
    self, V1Fragment, definition_symbols, emit_v1_package, fn_ref, module_id, ws_v1_package,
    ws_with_dependency_module, ws_with_stdlib_module,
};
use skull::mir_emit::emit_full;
use ustr::Ustr;

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
    common::require_luajit();
    assert_eq!(common::run_main(&emit.lua), "9");

    let edited = ws_with_dependency_module(
        "fn unused_dependency_body() -> int { return 7 }\nfn reached_dependency_body() -> int { return 9 }",
        "fn main() { print(0) }",
    );
    let main = fn_ref(&edited, "main");
    let incremental = skull::mir_emit::emit_incremental(
        &edited,
        &mut cache,
        &[(
            main.module,
            bone_semantic::hir::BodyOwner::Function(main.func),
        )],
    )
    .expect("remove the dependency call incrementally");
    let fresh = skull::mir_emit::FullEmit::program()
        .project_root(Some("/tmp/rock-mono-project"))
        .run(&edited, main.module)
        .expect("fresh emission after removing the dependency call")
        .0;
    assert_eq!(incremental.lua, fresh.lua);
    assert_eq!(common::run_main(&incremental.lua), "0");
}

/// Under modules v1 a function's name is its DefKey (`<pkg>::<module>::<name>`),
/// so the sim glue must find the registrar by its declared name: once host engine is
/// v1, a bare-name lookup fails every sim build.
#[test]
fn v1_sim_glue_finds_register_sim_by_declared_name() {
    let fragments = [
        V1Fragment {
            file: "main.rock",
            src: SIM_SRC,
        },
        V1Fragment {
            file: "engine.rock",
            src: &format!("mod engine;\npub {}", SIM_LIB.trim_start()),
        },
    ];
    let emit = emit_v1_package("v1sims", &fragments);
    let registrations: Vec<&str> = emit
        .lua
        .lines()
        .filter(|l| l.starts_with("register_sim_") && l.contains("probe_sim\""))
        .collect();
    assert_eq!(registrations.len(), 1);
}

/// Two registrars in one v1 build are ambiguous; the glue must name both
/// instead of silently calling whichever module came first.
#[test]
fn v1_sim_glue_rejects_two_register_sim_definitions() {
    let lib = |module: &str| {
        format!(
            "mod {module};\npub fn register_sim(name: string, body: () -> int) {{ print(\"{{name}}\") }}\n"
        )
    };
    let (a, b) = (lib("engine_a"), lib("engine_b"));
    let fragments = [
        V1Fragment {
            file: "main.rock",
            src: SIM_SRC,
        },
        V1Fragment {
            file: "engine_a.rock",
            src: &a,
        },
        V1Fragment {
            file: "engine_b.rock",
            src: &b,
        },
    ];
    let ws = ws_v1_package("v1sims_twice", &fragments);
    let mid = module_id(&ws, "main.rock");
    let error = match skull::mir_emit::emit_workspace_mir(&ws, mid) {
        Ok(_) => panic!("two registrars must not emit"),
        Err(error) => error,
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
    let mid = *ws
        .module_ids
        .get(&Ustr::from("/tmp/rock-mono-project/main.rock"))
        .unwrap();
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
        &[V1Fragment {
            file,
            src: "mod helpers;\npub fn uncalled_api() -> int { return 7 }",
        }],
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
fn single_file_compilation_keeps_uncalled_functions() {
    let ws = common::ws_with_stdlib("fn uncalled_helper() -> int { return 7 }");
    let mid = module_id(&ws, "<test>");
    let (emit, _) = emit_full(&ws, mid).expect("single-file emission");
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
