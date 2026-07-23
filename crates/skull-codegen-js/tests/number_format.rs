//! Number-formatting parity (docs/multi-backend-testing-plan.md T2, Stage 3):
//! the JS `lua_tostring` runtime (`JS_STRING_RUNTIME`, C `%.14g` reconstructed
//! with round-half-to-even) must byte-match Lua's `tostring` for the battery in
//! `fixtures/number_format.rock`. Runs the SAME fn through the full skull
//! pipeline under luajit and through the JS emitter under node, then
//! byte-compares stdout line by line -- luajit is the oracle, no hand-pinned
//! expectations to rot. Gated on `node` AND `luajit`; absent, skip WITH A LOG.

use std::process::Command;

use bone_semantic::inference::Workspace;
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

const FIXTURE: &str = include_str!("fixtures/number_format.rock");
const FIXTURE_MOD: &str = "<number_format>";

fn tool_available(tool: &str, arg: &str) -> bool {
    Command::new(tool).arg(arg).output().map(|o| o.status.success()).unwrap_or(false)
}

fn both_backends_available(test: &str) -> bool {
    for (tool, arg) in [("node", "--version"), ("luajit", "-v")] {
        if !tool_available(tool, arg) {
            eprintln!("SKIP {test}: `{tool}` not on PATH");
            return false;
        }
    }
    true
}

fn fixture_ws() -> Workspace {
    let mut modules: Vec<skull_parser::Ast> = rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect();
    modules.push(
        skull_parser::parse_module(FIXTURE, ustr::Ustr::from(FIXTURE_MOD)).expect("fixture parse"),
    );
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

fn find_fn(ws: &Workspace, name: &str) -> FuncRef {
    for (_, &mid) in ws.module_ids.iter() {
        for (fid, f) in ws.module(mid).functions.iter() {
            if f.name.as_str() == name && f.inline_body.is_none() {
                return FuncRef { module: mid, func: fid };
            }
        }
    }
    panic!("no fn `{name}` in workspace");
}

fn write_temp(tag: &str, ext: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir()
        .join(format!("skull_js_numfmt_{}_{tag}.{ext}", std::process::id()));
    std::fs::write(&path, contents).expect("write temp program");
    path
}

fn run_lines(tool: &str, tag: &str, ext: &str, prog: &str) -> Vec<String> {
    let path = write_temp(tag, ext, prog);
    let out = Command::new(tool).arg(&path).output().unwrap_or_else(|e| panic!("spawn {tool}: {e}"));
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "{tool} exited non-zero.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Faithful byte compare: keep every line verbatim (only strip the final
    // trailing newline), no per-line trim -- a stray space WOULD be a divergence.
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.strip_suffix('\n').unwrap_or(&s).split('\n').map(|l| l.to_string()).collect()
}

#[test]
fn number_tostring_parity_luajit_vs_node() {
    if !both_backends_available("number_tostring_parity_luajit_vs_node") {
        return;
    }
    let ws = fixture_ws();
    let name = "test_number_battery";

    let mid = *ws.module_ids.get(&ustr::Ustr::from(FIXTURE_MOD)).expect("fixture module id");
    let mut lua = skull::mir_emit::emit_workspace_mir(&ws, mid).expect("lua emit").lua;
    lua.push_str(&format!("\n{name}()\n"));
    let lua_lines = run_lines("luajit", "battery", "lua", &lua);

    let scx = ScalarCtx::new(&ws);
    let roots = [(find_fn(&ws, name), name.to_string())];
    let defs = skull_codegen_js::emit::emit_with_closure(&ws, &scx, &roots)
        .unwrap_or_else(|e| panic!("closure emit: {}", e.message));
    // A stdout-writing `print` (Lua-parity: args lua_tostring-joined by TAB, one
    // line per call) -- the same runtime `lua_tostring` the harness uses.
    let js = format!(
        "{}{}\nfunction print() {{ var p = []; for (var i = 0; i < arguments.length; i++) \
         p.push(lua_tostring(arguments[i])); console.log(p.join(\"\\t\")); }}\n{defs}\n{name}();\n",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_STRING_RUNTIME,
    );
    let js_lines = run_lines("node", "battery", "js", &js);

    assert_eq!(
        lua_lines, js_lines,
        "number formatting diverged between luajit and node"
    );
}
