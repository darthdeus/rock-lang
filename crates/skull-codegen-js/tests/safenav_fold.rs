//! Safe-nav null guards must survive the short-circuit fold.
//!
//! `emit_branch` folds a nested branch into an enclosing `&&`/`||` and hoists
//! the folded block's projections before the `if`. A `Field` read faults on
//! null/undefined in JS, so hoisting it above the null check that guards it
//! miscompiles `a?.b?.c` chains (assign and compound-assign lower their
//! intermediate reads into such foldable blocks; reads pin the non-folded
//! shape). Each test runs the SAME program through Lua (oracle) and JS:
//! expectations below are the LuaJIT output via
//! `skull::mir_emit::emit_workspace_mir` + `luajit`, asserted on both sides.
//! Gated on `node` AND `luajit`; absent, skip WITH A LOG.

use crate::common::{tool, ws_with};

fn tool_available(tool_name: &str, arg: &str) -> bool {
    tool(tool_name)
        .arg(arg)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn both_backends_available(test: &str) -> bool {
    for (name, arg) in [("node", "--version"), ("luajit", "-v")] {
        if !tool_available(name, arg) {
            eprintln!("SKIP {test}: `{name}` not on PATH");
            return false;
        }
    }
    true
}

fn write_temp(tag: &str, ext: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "skull_js_safenav_{}_{tag}.{ext}",
        std::process::id()
    ));
    std::fs::write(&path, contents).expect("write temp program");
    path
}

fn run_tool(tool_name: &str, tag: &str, ext: &str, prog: &str) -> String {
    let path = write_temp(tag, ext, prog);
    let out = tool(tool_name)
        .arg(&path)
        .output()
        .unwrap_or_else(|e| panic!("spawn {tool_name}: {e}"));
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "{tool_name} exited non-zero.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn run_both(src: &str, tag: &str) -> (String, String) {
    let ws = ws_with(src);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let mut lua = skull::mir_emit::emit_workspace_mir(&ws, mid)
        .expect("lua emit")
        .lua;
    lua.push_str("\nmain()\n");
    let lua_out = run_tool("luajit", tag, "lua", &lua);
    let prog = skull_codegen_js::emit::emit_program(&ws, mid)
        .unwrap_or_else(|e| panic!("JS emit rejected: {}", e.message));
    let mut js = skull_codegen_js::assemble_prelude(
        &prog.program,
        prog.has_code_refs(),
        skull_codegen_js::JS_NODE_HOST,
        &[],
    );
    js.push_str(&prog.program);
    let main = prog.main_js_name.as_deref().expect("program has a main");
    js.push_str(&format!("\n{main}();\n"));
    let js_out = run_tool("node", tag, "js", &js);
    (lua_out, js_out)
}

fn check(src: &str, tag: &str, expect: &str) {
    let (lua_out, js_out) = run_both(src, tag);
    assert_eq!(lua_out, expect, "Lua oracle drifted for {tag}");
    assert_eq!(
        js_out, expect,
        "JS diverged from Lua for {tag}\nLua:\n{lua_out}"
    );
}

const READ_SRC: &str = r#"
struct B { c: int }
struct A { b: B? }
fn main() {
    let a_null: A? = null
    print(a_null?.b?.c ?? -1)
    let a_bnull: A? = A { b: null }
    print(a_bnull?.b?.c ?? -1)
    let a_full: A? = A { b: B { c: 42 } }
    print(a_full?.b?.c ?? -1)
}
"#;

const ASSIGN_SRC: &str = r#"
struct B { c: int }
struct A { b: B? }
fn main() {
    let a_null: A? = null
    a_null?.b?.c = 99
    print("a_null skipped")
    print(a_null?.b?.c ?? -1)
    let a_bnull: A? = A { b: null }
    a_bnull?.b?.c = 99
    print("bnull skipped")
    print(a_bnull?.b?.c ?? -1)
    let a_full: A? = A { b: B { c: 1 } }
    a_full?.b?.c = 99
    print("full wrote")
    print(a_full?.b?.c ?? -1)
}
"#;

const COMPOUND_SRC: &str = r#"
struct B { c: int }
struct A { b: B? }
fn main() {
    let a_null: A? = null
    a_null?.b?.c += 10
    print("a_null skipped")
    print(a_null?.b?.c ?? -1)
    let a_bnull: A? = A { b: null }
    a_bnull?.b?.c += 10
    print("bnull skipped")
    print(a_bnull?.b?.c ?? -1)
    let a_full: A? = A { b: B { c: 1 } }
    a_full?.b?.c += 10
    print("full wrote")
    print(a_full?.b?.c ?? -1)
}
"#;

#[test]
fn safenav_read_each_intermediate() {
    if !both_backends_available("safenav_read_each_intermediate") {
        return;
    }
    check(READ_SRC, "read", "-1\n-1\n42");
}

#[test]
fn safenav_assign_each_intermediate() {
    if !both_backends_available("safenav_assign_each_intermediate") {
        return;
    }
    check(
        ASSIGN_SRC,
        "assign",
        "a_null skipped\n-1\nbnull skipped\n-1\nfull wrote\n99",
    );
}

#[test]
fn safenav_compound_assign_each_intermediate() {
    if !both_backends_available("safenav_compound_assign_each_intermediate") {
        return;
    }
    check(
        COMPOUND_SRC,
        "compound",
        "a_null skipped\n-1\nbnull skipped\n-1\nfull wrote\n11",
    );
}
