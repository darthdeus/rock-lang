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
//! Needs `node` and `luajit`.

use crate::common::{run_program, ws_with};

fn run_both(src: &str) -> (String, String) {
    let ws = ws_with(src);
    let mid = crate::common::test_mid(&ws);
    let mut lua = skull::mir_emit::emit_workspace_mir(&ws, mid)
        .expect("lua emit")
        .lua;
    lua.push_str("\nmain()\n");
    let lua_out = run_program("luajit", "lua", &lua).trim().to_string();
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
    let js_out = run_program("node", "js", &js).trim().to_string();
    (lua_out, js_out)
}

fn check(src: &str, tag: &str, expect: &str) {
    let (lua_out, js_out) = run_both(src);
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
    if !crate::common::require_node_and_luajit() {
        return;
    }
    check(READ_SRC, "read", "-1\n-1\n42");
}

#[test]
fn safenav_assign_each_intermediate() {
    if !crate::common::require_node_and_luajit() {
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
    if !crate::common::require_node_and_luajit() {
        return;
    }
    check(
        COMPOUND_SRC,
        "compound",
        "a_null skipped\n-1\nbnull skipped\n-1\nfull wrote\n11",
    );
}
