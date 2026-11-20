use bone_core::workspace::Workspace;
use skull_mir::mir::FuncRef;

use crate::common::run_file;

fn workspace(source: &str) -> Workspace {
    crate::common::ws_of_modules(vec![crate::common::package_module("app", "<test>", source)])
}

fn root(ws: &Workspace) -> FuncRef {
    let mid = crate::common::test_mid(ws);
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, function)| function.name.as_str() == "app::root")
        .expect("qualified root");
    FuncRef {
        module: mid,
        func: fid,
    }
}

fn js_runtime() -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\nvar __G = {{}};\n",
        skull_codegen_js::JS_VALUE_INTEGER_CHECK,
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_STRING_RUNTIME,
        skull_codegen_js::JS_INSPECT_RUNTIME,
        skull_codegen_js::JS_DEEPCOPY_RUNTIME,
    )
}

#[test]
fn qualified_programs_match_luajit() {
    for (source, expected) in [
        (
            "enum Shade { Dark, Light }\n\
             fn root() -> string { let shade = Shade::Light; \"{shade}\" }",
            "app::Shade::Light",
        ),
        (
            "enum Choice { Empty, Full(string) }\n\
             fn root() -> string { let choice = Choice::Empty; \"{choice}\" }",
            "app::Choice::Empty",
        ),
        (
            r#"fn root() -> string { __rock_quote_string("line\n\t\r\"\\") }"#,
            r#""line\n\t\r\"\\""#,
        ),
        (
            r#"
struct Counter { calls: int }
fn destination(counter: Counter) -> Map<string, int> {
    counter.calls = counter.calls + 1
    return map_new()
}
fn root() -> int {
    let counter = Counter { calls: 0 }
    let original = [1, 2]
    let copy = original.clone_into(map_storage(destination(counter)))
    original.set(0, 9)
    return counter.calls * 1000 + original[0] * 100 + copy[0]
}
"#,
            "1901",
        ),
        (
            "global BASE: int = 7\n\
             global DERIVED: int = BASE + 4\n\
             fn root() -> int { DERIVED = DERIVED + 1; return DERIVED }",
            "12",
        ),
        (
            "const BASE: int = 7\n\
             const DERIVED: int = BASE + 4\n\
             fn root() -> int { BASE + DERIVED }",
            "18",
        ),
        (
            "nobox value Pair { x: int, y: int }\n\
             const BASE: int = 7\n\
             const PAIR: Pair = Pair { x: BASE, y: BASE + 1 }\n\
             const DERIVED: int = PAIR.x + PAIR.y\n\
             fn root() -> int { DERIVED + BASE + PAIR.y }",
            "30",
        ),
    ] {
        differential(source, expected);
    }
}

fn lua_value(ws: &Workspace) -> String {
    let fr = root(ws);
    let lua_name = skull_mir::mangle::mangle_fn(ws, &ws.module(fr.module).functions[fr.func]);
    let lua = skull::mir_emit::emit_workspace_mir(ws, fr.module)
        .expect("Lua emission")
        .lua;
    run_file("luajit", &format!("{lua}\nprint({lua_name}())"))
}

fn differential(source: &str, expected: &str) {
    let ws = workspace(source);
    let fr = root(&ws);
    let lua_out = lua_value(&ws);
    assert_eq!(lua_out, expected, "LuaJIT oracle");
    let js = skull_codegen_js::emit::emit_with_closure(&ws, &[(fr, "root".to_string())])
        .expect("JS emission");
    let js_out = run_file(
        "node",
        &format!("{}{js}\nconsole.log(root());", js_runtime()),
    );
    assert_eq!(js_out, lua_out, "qualified globals must agree with LuaJIT");
}

/// A package decorator that shares its spelling with the stdlib's `on_reload`
/// is a different definition. Applications resolve by definition key, so only
/// the applied one is emitted, under the same symbol the Lua backend uses.
#[test]
fn decorator_applications_resolve_by_definition_key() {
    let ws = workspace(
        "global HITS: int = 0\n\
         deco init on_reload { HITS = HITS + 1 }\n\
         @on_reload fn tick() {}\n\
         @nomangle fn root() -> int { HITS }",
    );
    let fr = root(&ws);
    let program = skull_codegen_js::emit::emit_program(&ws, fr.module)
        .expect("JS program")
        .program;
    let symbol = format!(
        "__deco_{}",
        skull_mir::mangle::symbol_of_key(ustr::Ustr::from("app::on_reload"))
    );
    let definitions: Vec<&str> = program
        .lines()
        .filter(|line| line.starts_with("function __deco_"))
        .collect();
    assert_eq!(
        definitions,
        [format!("function {symbol}(c0) {{").as_str()],
        "{program}"
    );
    assert!(
        program.contains(&format!("{symbol}({{name: \"app::tick\"")),
        "{program}"
    );
}
