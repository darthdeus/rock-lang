//! StringBuilder emission (T8d). The three `@intrinsic inline(lua)` accumulator
//! ops map to a JS array: `sb_new` -> `[]`, `sb_push` -> `.push`, `sb_build` ->
//! `.join("")`. `join` (stdlib/strings.rock) is pure Rock over them and
//! co-emits. The node-executed check is the correctness oracle; the corpus
//! `tests/strings/js_string_builder.rock` cross-checks against luajit.

use crate::common::ws_with;

crate::runtime_eval!(eval_str: JS_NB_CHECK_SHIMS, JS_VEC_CHECK_SHIMS);

// Direct StringBuilder: push several fragments, build the concatenation.
#[test]
fn sb_direct_concat() {
    let src = "fn root() -> string {\n\
                   let sb = sb_new()\n\
                   sb_push(sb, \"foo\")\n\
                   sb_push(sb, \"bar\")\n\
                   sb_build(sb)\n\
               }";
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").expect("sb emits");
    assert!(
        js.contains(".push(") && js.contains(".join("),
        "renders array ops:\n{js}"
    );
    eval_str(&js, "root", "foobar");
}

// An empty builder builds the empty string.
#[test]
fn sb_empty_builds_empty() {
    let src = "fn root() -> string {\n\
                   let sb = sb_new()\n\
                   sb_build(sb)\n\
               }";
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").expect("sb emits");
    eval_str(&js, "root", "");
}

// `join` co-emits the sb_* ops (it is pure Rock over them).
#[test]
fn join_co_emits_over_sb() {
    let src = "fn root() -> string {\n\
                   let parts = [\"a\", \"b\", \"c\"]\n\
                   join(parts, \", \")\n\
               }";
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").expect("join emits");
    eval_str(&js, "root", "a, b, c");
}
