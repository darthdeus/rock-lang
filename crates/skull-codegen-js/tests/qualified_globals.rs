use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use skull_mir::mir::FuncRef;

fn workspace(source: &str) -> Workspace {
    let mut modules: Vec<_> = rock_stdlib::stdlib_files()
        .iter()
        .map(|file| {
            skull_parser::parse_module_clean(
                file.contents,
                ustr::Ustr::from(format!("<stdlib:{}>", file.name).as_str()),
            )
            .expect("stdlib parse")
        })
        .collect();
    let mut module = skull_parser::parse_module_clean_with_dialect(
        source,
        ustr::Ustr::from("<test>"),
        skull_parser::Dialect::V1,
    )
    .expect("fixture parse");
    module.package = Some(skull_parser::PackageTag {
        id: 0,
        self_name: "app".to_string(),
    });
    modules.push(module);
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic pass")
}

fn root(ws: &Workspace) -> FuncRef {
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
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

fn run(tool: &str, program: &str) -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "skull_qualified_globals_{}_{serial}_{tool}",
        std::process::id()
    ));
    std::fs::write(&path, program).expect("write program");
    let out = crate::common::tool(tool)
        .arg(&path)
        .output()
        .expect("run tool");
    let _ = std::fs::remove_file(path);
    assert!(
        out.status.success(),
        "{tool} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8 stdout")
        .trim()
        .to_string()
}

fn js_runtime() -> String {
    format!(
        "{}\n{}\nvar __G = {{}};\n",
        skull_codegen_js::JS_SESSION_RUNTIME,
        skull_codegen_js::JS_CALL_HELPER
    )
}

fn lua_value(ws: &Workspace) -> String {
    let fr = root(ws);
    let lua_name = skull_mir::mangle::mangle_fn(ws, &ws.module(fr.module).functions[fr.func]);
    let lua = skull::mir_emit::emit_workspace_mir(ws, fr.module)
        .expect("Lua emission")
        .lua;
    run("luajit", &format!("{lua}\nprint({lua_name}())"))
}

fn differential(source: &str, expected: &str) {
    let ws = workspace(source);
    let fr = root(&ws);
    let lua_out = lua_value(&ws);
    assert_eq!(lua_out, expected, "LuaJIT oracle");
    let js = skull_codegen_js::emit::emit_with_closure(&ws, &[(fr, "root".to_string())])
        .expect("JS emission");
    let js_out = run(
        "node",
        &format!("{}{js}\nconsole.log(root());", js_runtime()),
    );
    assert_eq!(js_out, lua_out, "qualified globals must agree with LuaJIT");
}

#[test]
fn qualified_mutable_global_initializers_match_luajit() {
    differential(
        "global BASE: int = 7\n\
         global DERIVED: int = BASE + 4\n\
         fn root() -> int { DERIVED = DERIVED + 1; return DERIVED }",
        "12",
    );
}

#[test]
fn qualified_const_globals_match_luajit() {
    differential(
        "const BASE: int = 7\n\
         const DERIVED: int = BASE + 4\n\
         fn root() -> int { BASE + DERIVED }",
        "18",
    );
}

#[test]
fn qualified_nobox_const_globals_match_luajit() {
    differential(
        "nobox value Pair { x: int, y: int }\n\
         const BASE: int = 7\n\
         const PAIR: Pair = Pair { x: BASE, y: BASE + 1 }\n\
         const DERIVED: int = PAIR.x + PAIR.y\n\
         fn root() -> int { DERIVED + BASE + PAIR.y }",
        "30",
    );
}

#[test]
fn qualified_const_overlay_reinitializes_dependents() {
    for (definition, read, expected) in [
        ("const DERIVED: int = BASE + 1", "DERIVED", "8\n11"),
        (
            "nobox value Pair { x: int, y: int }\n\
             const DERIVED: Pair = Pair { x: BASE + 1, y: BASE + 2 }",
            "DERIVED.x + DERIVED.y",
            "17\n23",
        ),
    ] {
        let source = |base| {
            format!(
                "deco keep {{}}\n\
             const BASE: int = {base}\n\
             {definition}\n\
             @keep @nomangle fn root() -> int {{ {read} }}"
            )
        };
        let before = workspace(&source(7));
        let after = workspace(&source(10));
        let fr = root(&before);
        let (program, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, fr.module)
            .expect("initial JS program");
        let (gid, _) = after
            .module(fr.module)
            .globals
            .iter()
            .find(|(_, global)| global.name.as_str() == "app::BASE")
            .expect("qualified global");
        let overlay = skull_codegen_js::emit::emit_overlay(
            &after,
            fr.module,
            &mut cache,
            &[(fr.module, BodyOwner::Global(gid))],
        )
        .expect("overlay")
        .expect_overlay();
        let name =
            skull_mir::mangle::mangle_fn(&before, &before.module(fr.module).functions[fr.func]);
        let output = run(
            "node",
            &format!(
                "{}{}\nconsole.log({name}());\n{overlay}\nconsole.log({name}());",
                js_runtime(),
                program.program,
            ),
        );
        let lua_out = format!("{}\n{}", lua_value(&before), lua_value(&after));
        assert_eq!(lua_out, expected, "LuaJIT oracle");
        assert_eq!(output, lua_out);
    }
}
