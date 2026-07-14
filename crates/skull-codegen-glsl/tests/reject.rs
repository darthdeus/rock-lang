//! Profile-pass rejection tests: one per error class. The GLSL profile is strict
//! (reject, never skip-and-continue), so each of these must return an Err whose
//! message names the offending construct.

use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn emit(src: &str, roots: &[&str]) -> Result<String, String> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let roots: Vec<String> = roots.iter().map(|s| s.to_string()).collect();
    skull_codegen_glsl::emit_shader_library(&ws, mid, &roots).map_err(|e| e.message)
}

fn reject(src: &str, roots: &[&str], needle: &str) {
    match emit(src, roots) {
        Ok(glsl) => panic!("expected rejection ({needle}) but emitted:\n{glsl}"),
        Err(msg) => assert!(
            msg.contains(needle),
            "rejection message should mention `{needle}`, got: {msg}"
        ),
    }
}

#[test]
fn reject_recursion() {
    let src = r#"
        @pure fn fact(n: float) -> float {
            return fact(n - 1.0)
        }
    "#;
    reject(src, &["fact"], "recursion");
}

#[test]
fn reject_string_param() {
    let src = r#"
        @pure fn takes_string(s: string) -> float { return 1.0 }
    "#;
    reject(src, &["takes_string"], "not tier 0");
}

#[test]
fn reject_vec_return() {
    let src = r#"
        fn makes_vec(n: int) -> [int] { return [n] }
    "#;
    reject(src, &["makes_vec"], "not tier 0");
}

#[test]
fn reject_string_body() {
    // A tier-0 signature but a string literal in the body.
    let src = r#"
        fn has_string(x: float) -> float {
            let s = "hello"
            return x
        }
    "#;
    reject(src, &["has_string"], "string literal");
}

#[test]
fn reject_unmapped_inline_lua_call() {
    let src = r#"
        @pure @inline inline(lua) fn myprim(x: float) -> float = "math.log(x)"
        @pure fn uses_prim(x: float) -> float { return myprim(x) }
    "#;
    reject(src, &["uses_prim"], "no GLSL intrinsic mapping");
}

// SENTINEL: milestone B reloops straight-line + counted-for only; a conditional
// branch (`if` / `&&`) is rejected, not miscompiled. (The current reject reason is
// incidental -- an `if` lowers through a nil, caught by the profile pass before
// emit_seq's explicit conditional-branch reject; either way it rejects.) Flip this
// to a golden test when the if-diamond / short-circuit reloop lands
// (docs/glsl-fn-emission.md).
#[test]
fn reject_if_branch_sentinel() {
    let src = r#"
        @pure fn pick(a: float, b: float) -> float {
            if a > b { return a } else { return b }
        }
    "#;
    reject(src, &["pick"], "shader profile:");
}

#[test]
fn reject_unknown_root() {
    let src = r#"
        @pure fn present(x: float) -> float { return x }
    "#;
    reject(src, &["absent"], "not found");
}
