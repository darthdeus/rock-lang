//! tests/library/js_try_call.rock runs try_call and unused checked index
//! reads on both backends. Here an unused read through a foreign Map or
//! Vec must still enter its host getter.

use crate::common::ws_with;

const TRY_CALL: &str = "@intrinsic @nomangle inline(lua) fn try_call(f: fn()) -> bool = \"\"\"\n    local ok, err = pcall(f)\n    return not ok\n\"\"\"\n";

#[test]
fn unused_foreign_map_and_nullable_vec_reads_keep_getters() {
    let src = format!(
        r#"{TRY_CALL}
inline(js) fn foreign_map(xs: [int]) -> Map<string, int> = """
    return new Proxy(new Map([['present', 1]]), {{ get(target, key) {{
        if (key === 'get') return function(k) {{
            globalThis.__seen_map_getter = true;
            xs.len = 0; xs._data[0] = undefined; return 1;
        }};
        return Reflect.get(target, key);
    }} }});
"""
inline(js) fn foreign_vec(xs: [int]) -> [int?] = """
    return {{ len: 1, _data: new Proxy([undefined], {{ get(target, key) {{
        if (key === '0') {{
            globalThis.__seen_vec_getter = true;
            xs.len = 0; xs._data[0] = undefined;
        }}
        return Reflect.get(target, key);
    }} }}) }};
"""
inline(js) fn getters_seen() -> bool = "return !!globalThis.__seen_map_getter && !!globalThis.__seen_vec_getter;"
fn map_read() -> bool {{
    let xs = [7]
    let m = foreign_map(xs)
    return try_call(() => {{ let _ = m["present"]; let _ = xs[0] }})
}}
fn vec_read() -> bool {{
    let xs = [7]
    let ys = foreign_vec(xs)
    return try_call(() => {{ let _ = ys[0]; let _ = xs[0] }})
}}
fn root() -> int {{
    let map_failed = map_read()
    let vec_failed = vec_read()
    if map_failed && vec_failed && getters_seen() {{ 1 }} else {{ 0 }}
}}
"#
    );
    let ws = ws_with(&src);
    let js = crate::common::emit_closure(&ws, "root").expect("foreign getters emit");
    crate::common::eval_int(
        &[
            skull_codegen_js::JS_NB_CHECK_SHIMS,
            skull_codegen_js::JS_VEC_CHECK_SHIMS,
            skull_codegen_js::JS_CALL_HELPER,
            skull_codegen_js::JS_TABLE_PRIM,
        ],
        &js,
        "root",
        1,
    );
}
