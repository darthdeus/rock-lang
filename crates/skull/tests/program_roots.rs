//! A program target roots its entry point, not every function of its own
//! sources; a library target (no `main`) keeps all of them.
use crate::common::{self, definition_symbols, fn_ref, ws_with_dependency_module};
use skull_mir::instance::Instance;

const LIB: &str = "fn engine_body() -> int { return 1 }";
const ROOT: &str = "/tmp/rock-mono-project";

fn program(
    src: &str,
) -> (
    skull::mir_emit::MirEmit,
    skull::mir_emit::SkullEmitCache,
    bone_semantic::inference::Workspace,
) {
    let ws = ws_with_dependency_module(LIB, src);
    let main = fn_ref(&ws, "main");
    let (emit, cache) = skull::mir_emit::FullEmit::program()
        .project_root(Some(ROOT))
        .run(&ws, main.module)
        .expect("emit");
    (emit, cache, ws)
}

fn defines(lua: &str, prefix: &str) -> bool {
    definition_symbols(lua)
        .iter()
        .any(|s| s.starts_with(prefix))
}

#[test]
fn a_program_does_not_define_own_functions_nothing_calls() {
    let (emit, cache, ws) = program(
        "fn own_unused() -> int { return 7 }\nfn own_used() -> int { return 9 }\nfn main() { print(own_used()) }",
    );
    assert!(
        !cache
            .lua_view()
            .retains(&Instance::base(fn_ref(&ws, "own_unused")))
    );
    assert!(!defines(&emit.lua, "own_unused"), "{}", emit.lua);
    common::require_luajit();
    assert_eq!(common::run_main(&emit.lua), "9");
}

#[test]
fn a_program_keeps_tests_sims_and_nomangle_functions() {
    let (emit, _, _) = program(
        "@test fn own_test() { }\n@nomangle fn own_host_entry() -> int { return 2 }\nfn main() { print(1) }",
    );
    assert!(defines(&emit.lua, "own_test"), "{}", emit.lua);
    assert!(defines(&emit.lua, "own_host_entry"), "{}", emit.lua);
}

#[test]
fn an_edit_that_starts_calling_an_own_function_defines_it() {
    let (_, mut cache, _) = program("fn own_later() -> int { return 5 }\nfn main() { print(0) }");
    let edited = ws_with_dependency_module(
        LIB,
        "fn own_later() -> int { return 5 }\nfn main() { print(own_later()) }",
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
    .expect("incremental");
    assert!(
        defines(&incremental.lua, "own_later"),
        "{}",
        incremental.lua
    );
    common::require_luajit();
    assert_eq!(common::run_main(&incremental.lua), "5");
}

#[test]
fn a_library_target_keeps_every_own_function() {
    let ws = ws_with_dependency_module(LIB, "fn own_export() -> int { return 3 }");
    let m = fn_ref(&ws, "own_export").module;
    let (emit, _) = skull::mir_emit::FullEmit::program()
        .project_root(Some(ROOT))
        .run(&ws, m)
        .expect("emit");
    assert!(defines(&emit.lua, "own_export"), "{}", emit.lua);
}
