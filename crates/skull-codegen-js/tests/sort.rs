//! Vec.sort emission (T8d). The stdlib inline(lua) `table.sort(_data, |a,b|
//! key(a) < key(b))` maps to the `__vec_sort` runtime: sort the logical
//! `0..len` slice in place (0-based hole-free `_data`), key_fn through
//! `__rock_call`. The node-executed checks are the correctness oracle;
//! `tests/vectors/js_sort.rock` cross-checks against luajit (tie order is unspecified,
//! so only order-invariant properties are asserted).

use crate::common::ws_with;

crate::runtime_eval!(eval_str: JS_NB_CHECK_SHIMS, JS_VEC_CHECK_SHIMS, JS_CALL_HELPER, JS_STRING_RUNTIME);

// Ascending sort by the identity key over a 0-based hole-free `_data` (a stray
// index-0 `[null,` hole would corrupt the readback).
#[test]
fn sort_ascending_by_identity() {
    let src = "fn root() -> string {\n\
                   let xs = [5, 3, 8, 1, 2]\n\
                   xs.sort((x: int) => float(x))\n\
                   let parts: [string] = []\n\
                   for x in xs { parts.push(tostring(x)) }\n\
                   join(parts, \" \")\n\
               }";
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").expect("sort emits");
    assert!(
        js.contains("__vec_sort"),
        "renders the slice-sort runtime:\n{js}"
    );
    assert!(
        !js.contains("[null,"),
        "0-based hole-free Vec literal, no index-0 hole:\n{js}"
    );
    eval_str(&js, "root", "1 2 3 5 8");
}

// A derived key (negation) sorts descending -- the key_fn actually runs.
#[test]
fn sort_descending_by_derived_key() {
    let src = "fn root() -> string {\n\
                   let xs = [5, 3, 8, 1, 2]\n\
                   xs.sort((x: int) => float(-x))\n\
                   let parts: [string] = []\n\
                   for x in xs { parts.push(tostring(x)) }\n\
                   join(parts, \" \")\n\
               }";
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").expect("sort emits");
    eval_str(&js, "root", "8 5 3 2 1");
}
