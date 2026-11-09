//! **Generated glue is a ROOT INPUT, not an emission-time bypass.**
//!
//! `sim_registration_lines` and `shader_registration_lines` write Lua TEXT that
//! names declarations by their mangled symbol -- `register_sim(...)`, and the
//! shader entry/helper declarations a registered program depends on. There is no
//! Rock call site for either, so the obligation cannot be DISCOVERED from a body;
//! it is an artifact root, which is exactly what `ArtifactRequest::roots` is for.
//!
//! Every assertion here reads `cache.lua_view()` -- the artifact view emission
//! decides definitions from -- so it stays red if the obligation is satisfied by
//! some second population input beside the view. The emitted-Lua arms are the
//! end-to-end control: a retained instance that reaches no definition would still
//! be a nil call at load.

use crate::common::{
    self, V1Fragment, emit_v1_package, fn_ref, ws_v1_package, ws_with_dependency_module,
    ws_with_stdlib_module,
};
use skull::mir_emit::emit_full;
use skull_mir::instance::Instance;
use ustr::Ustr;

/// `register_sim` and the control live in a LIBRARY module: `is_emission_root`
/// roots every own-project declaration unconditionally, so a fixture that puts
/// them in `<test>` cannot distinguish retention from "nothing is prunable here".
const SIM_LIB: &str = r#"
fn register_sim(name: string, body: () -> int) {
    print("{name}")
}

fn unrelated_library_body() -> int {
    return 7
}
"#;

const SIM_SRC: &str = r#"
@sim
fn probe_sim() -> int {
    return 1
}

fn main() {
    print("hi")
}
"#;

fn definition_symbols(lua: &str) -> Vec<String> {
    lua.match_indices("\nfunction ")
        .map(|(i, m)| {
            let rest = &lua[i + m.len()..];
            rest[..rest.find('(').expect("fn header has `(`")].to_string()
        })
        .collect()
}

/// The sim glue's `register_sim` obligation is carried by the Lua
/// `ArtifactView`, with its population stated: one `@sim` declaration, so the
/// glue is genuinely emitted rather than the arm passing on an empty registry.
#[test]
fn sim_glue_retains_register_sim_through_the_view() {
    let ws = ws_with_stdlib_module(SIM_LIB, SIM_SRC);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let sims = skull::mir_emit::collect_sim_fns(&ws);
    assert_eq!(
        sims.len(),
        1,
        "fixture must register exactly one sim, got {}",
        sims.len()
    );
    assert_eq!(
        sims[0].0.as_str(),
        "<test>",
        "sim collection retains its HIR module file"
    );

    let (emit, cache) = emit_full(&ws, mid).expect("emit");
    let view = cache.lua_view();

    let register = Instance::base(fn_ref(&ws, "register_sim"));
    assert!(
        view.retains(&register),
        "the Lua ArtifactView must retain `register_sim` -- the sim glue calls it \
         by mangled name from generated text"
    );

    // End-to-end control: retention must reach a real definition, since the
    // generated call is what fails at load if it does not.
    let symbol = view
        .symbols
        .get(&register)
        .expect("a retained instance has a symbol");
    assert!(
        definition_symbols(&emit.lua).iter().any(|s| s == symbol),
        "`register_sim` retained but not defined; symbol `{symbol}`"
    );
    assert!(
        emit.lua.contains(&format!("{symbol}(\"probe_sim\", ")),
        "generated sim registration must call the retained symbol `{symbol}`"
    );
}

/// The control. An unrelated LIBRARY body with no caller and no root status is
/// pruned, so the arm above is not passing because this fixture retains
/// everything.
#[test]
fn an_unrelated_library_body_stays_pruned() {
    let ws = ws_with_stdlib_module(SIM_LIB, SIM_SRC);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let (emit, cache) = emit_full(&ws, mid).expect("emit");
    let unrelated = Instance::base(fn_ref(&ws, "unrelated_library_body"));
    assert!(
        !cache.lua_view().retains(&unrelated),
        "an uncalled library body must not be retained"
    );
    assert!(
        !definition_symbols(&emit.lua)
            .iter()
            .any(|s| s.starts_with("unrelated_library_body")),
        "an uncalled library body must not be defined"
    );
}

#[test]
fn shared_prefix_dependency_retains_only_reached_bodies() {
    let ws = ws_with_dependency_module(
        "fn unused_dependency_body() -> int { return 7 }\nfn reached_dependency_body() -> int { return 9 }",
        "fn main() { print(reached_dependency_body()) }",
    );
    let mid = *ws
        .module_ids
        .get(&Ustr::from("/tmp/rock-mono-project/main.rock"))
        .expect("project module");
    let (emit, mut cache) = skull::mir_emit::FullEmit::program()
        .project_root(Some("/tmp/rock-mono-project"))
        .run(&ws, mid)
        .expect("pruned dependency bodies must not require lowering plans");
    let unused = Instance::base(fn_ref(&ws, "unused_dependency_body"));
    let reached = Instance::base(fn_ref(&ws, "reached_dependency_body"));
    assert!(
        !cache.lua_view().retains(&unused),
        "a sibling directory is not part of the target"
    );
    assert!(
        cache.lua_view().retains(&reached),
        "reachable dependency bodies must remain"
    );
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
    let removed = Instance::base(fn_ref(&edited, "reached_dependency_body"));
    assert!(
        !cache.lua_view().retains(&removed),
        "cached roots must not pin sibling bodies"
    );
    let fresh = skull::mir_emit::FullEmit::program()
        .project_root(Some("/tmp/rock-mono-project"))
        .run(&edited, main.module)
        .expect("fresh emission after removing the dependency call")
        .0;
    assert_eq!(incremental.lua, fresh.lua);
    assert_eq!(common::run_main(&incremental.lua), "0");
}

/// A registered `rock_shader` program's dependencies -- its stage entries and
/// GLSL helpers -- are the second generated-glue root class.
const SHADER_LIB: &str = r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value UiVertU  { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }
value UiVary   { position: Vec4, TexCoord: Vec2 }

@shader(vertex)
fn glue_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return UiVary { position: clip, TexCoord: a.aTexCoord }
}

@shader(fragment)
fn glue_frag(vary: UiVary) -> Vec4 {
    return Vec4 { x: 1.0, y: 0.0, z: 1.0, w: 1.0 }
}
"#;

const SHADER_SRC: &str = r#"
fn main() {
    print(rock_shader(glue_vert, glue_frag))
}
"#;

/// The DISCRIMINATING fixture. `shader_registration_lines` collects
/// `rock_shader` pairs over the whole workspace, so a registration is emitted
/// even when the body holding the call is itself pruned -- and then nothing in
/// the Rock call graph reaches the stage entries at all. This is the shape the
/// root obligation exists for; the reachable-call fixture above cannot see it.
const SHADER_UNREACHED_LIB: &str = r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value UiVertU  { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }
value UiVary   { position: Vec4, TexCoord: Vec2 }

@shader(vertex)
fn glue_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return UiVary { position: clip, TexCoord: a.aTexCoord }
}

@shader(fragment)
fn glue_frag(vary: UiVary) -> Vec4 {
    return Vec4 { x: 1.0, y: 0.0, z: 1.0, w: 1.0 }
}

fn nobody_calls_this() -> string {
    return rock_shader(glue_vert, glue_frag)
}
"#;

const SHADER_UNREACHED_SRC: &str = r#"
fn main() {
    print("hi")
}
"#;

#[test]
fn an_unreached_shader_registration_still_retains_its_dependencies() {
    let ws = ws_with_stdlib_module(SHADER_UNREACHED_LIB, SHADER_UNREACHED_SRC);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let (emit, cache) = emit_full(&ws, mid).expect("emit");
    let view = cache.lua_view();

    assert_eq!(
        emit.lua.matches("__rock_shader_register(").count(),
        1,
        "the registration is emitted even though its caller is pruned"
    );
    assert!(
        !view.retains(&Instance::base(fn_ref(&ws, "nobody_calls_this"))),
        "control: the body holding the rock_shader call is genuinely pruned"
    );

    for name in ["glue_vert", "glue_frag"] {
        let d = fn_ref(&ws, name);
        assert!(
            view.retains(&Instance::base(d)),
            "the Lua ArtifactView must retain shader dependency `{name}`"
        );
    }
}

#[test]
fn shader_glue_dependencies_are_retained_through_the_view() {
    let ws = ws_with_stdlib_module(SHADER_LIB, SHADER_SRC);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let (emit, cache) = emit_full(&ws, mid).expect("emit");
    let view = cache.lua_view();

    assert!(
        emit.lua.contains("__rock_shader_register("),
        "fixture must register a shader program"
    );

    let deps = [fn_ref(&ws, "glue_vert"), fn_ref(&ws, "glue_frag")];
    assert_eq!(deps.len(), 2, "stated population: two stage entries");
    for d in deps {
        let i = Instance::base(d);
        assert!(
            view.retains(&i),
            "the Lua ArtifactView must retain shader dependency {:?}",
            ws.module(d.module).functions[d.func].name.as_str()
        );
    }
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
    assert_eq!(
        registrations.len(),
        1,
        "v1 sim glue must register `probe_sim` once:\n{}",
        emit.lua
    );
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
    let mid = *ws
        .module_ids
        .get(&Ustr::from("main.rock"))
        .expect("module id");
    let error = match skull::mir_emit::emit_workspace_mir(&ws, mid) {
        Ok(_) => panic!("two registrars must not emit"),
        Err(error) => error,
    };
    assert!(
        error.message.contains("engine_a::register_sim")
            && error.message.contains("engine_b::register_sim"),
        "{}",
        error.message
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
    assert!(
        result.is_err(),
        "textual prefixes must not validate a nonexistent project root"
    );
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
    let mid = *ws.module_ids.get(&Ustr::from(file)).expect("nested module");
    let (emit, cache) = skull::mir_emit::FullEmit::program()
        .project_root(Some("/tmp/rock-root-contract/app"))
        .run(&ws, mid)
        .expect("a nested source validates its project root");
    let api = Instance::base(fn_ref(&ws, "root_contract::helpers::uncalled_api"));
    assert!(
        cache.lua_view().retains(&api),
        "uncalled project APIs are roots"
    );
    let symbol = &cache.lua_view().symbols[&api];
    assert!(definition_symbols(&emit.lua).contains(symbol));
}

#[test]
fn single_file_compilation_keeps_uncalled_functions() {
    let ws = common::ws_with_stdlib("fn uncalled_helper() -> int { return 7 }");
    let mid = *ws
        .module_ids
        .get(&Ustr::from("<test>"))
        .expect("single-file module");
    let (emit, cache) = emit_full(&ws, mid).expect("single-file emission");
    let helper = Instance::base(fn_ref(&ws, "uncalled_helper"));
    assert!(
        cache.lua_view().retains(&helper),
        "single-file helpers are roots"
    );
    let symbol = &cache.lua_view().symbols[&helper];
    assert!(definition_symbols(&emit.lua).contains(symbol));
}

#[test]
fn shared_prefix_dependency_keeps_bare_name_api_roots() {
    let ws = ws_with_dependency_module(
        "@nomangle fn external_api() -> int { return 7 }",
        "fn main() {}",
    );
    let mid = *ws
        .module_ids
        .get(&Ustr::from("/tmp/rock-mono-project/main.rock"))
        .expect("project module");
    let (emit, cache) = skull::mir_emit::FullEmit::program()
        .project_root(Some("/tmp/rock-mono-project"))
        .run(&ws, mid)
        .expect("dependency API emission");
    let api = Instance::base(fn_ref(&ws, "external_api"));
    assert!(
        cache.lua_view().retains(&api),
        "bare-name APIs remain roots in dependencies"
    );
    assert!(
        definition_symbols(&emit.lua)
            .iter()
            .any(|s| s == "external_api")
    );
}
