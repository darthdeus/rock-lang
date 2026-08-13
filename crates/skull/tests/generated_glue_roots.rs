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

mod common;
use common::{fn_ref, ws_with_dependency_module, ws_with_stdlib_module};
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
    assert_eq!(sims.len(), 1, "fixture must register exactly one sim, got {}", sims.len());

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
    let symbol = view.symbols.get(&register).expect("a retained instance has a symbol");
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
        !definition_symbols(&emit.lua).iter().any(|s| s.starts_with("unrelated_library_body")),
        "an uncalled library body must not be defined"
    );
}

#[test]
fn an_unretained_dependency_body_needs_no_concrete_plan() {
    let ws = ws_with_dependency_module(
        "fn unused_dependency_body() -> int { return 7 }",
        "fn main() { print(\"hi\") }",
    );
    let mid = *ws
        .module_ids
        .get(&Ustr::from("/tmp/rock-mono-project/main.rock"))
        .expect("project module");
    skull::mir_emit::emit_full_in(&ws, mid, Some("/tmp/rock-mono-project"))
        .expect("pruned dependency bodies must not require lowering plans");
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
