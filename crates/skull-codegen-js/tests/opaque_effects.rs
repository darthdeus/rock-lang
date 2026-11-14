use crate::common::{emit_closure, require_node, run_node, ws_with};

#[test]
fn foreign_type_tag_getter_invalidates_vector_bounds() {
    if !require_node() {
        return;
    }
    let ws = ws_with(
        r#"
@nomangle fn after_type_tag(xs: [int], value: any) -> int {
    if xs.len() <= 0 { return -1 }
    let tag = __rock_get_type(value)
    if tag == null { return -2 }
    return xs[0]
}
"#,
    );
    let js = emit_closure(&ws, "after_type_tag").expect("emit type tag callback probe");
    let output = run_node(&format!(
        r#"{runtime}
{js}
const xs = {{len: 1, _data: [7]}};
let calls = 0;
const value = {{get __ty() {{ calls++; xs.len = 0; xs._data.length = 0; return 'foreign'; }}}};
let caught = false;
try {{ after_type_tag(xs, value); }} catch (error) {{
    caught = String(error).includes('Vec index out of bounds');
}}
if (!caught || calls === 0) throw new Error('callback left a stale bounds proof');
console.log('ok');
"#,
        runtime = skull_codegen_js::JS_VEC_CHECK_SHIMS,
    ));
    assert_eq!(output.trim(), "ok");
}

#[test]
fn foreign_string_conversion_invalidates_vector_bounds() {
    if !require_node() {
        return;
    }
    let ws = ws_with(
        r#"
@nomangle fn after_conversion(xs: [int], value: any) -> int {
    if xs.len() <= 0 { return -1 }
    let text = ffi_string(value, null)
    if text == "" { return -2 }
    return xs[0]
}
"#,
    );
    let js = emit_closure(&ws, "after_conversion").expect("emit string callback probe");
    let output = run_node(&format!(
        r#"{runtime}
{js}
const xs = {{len: 1, _data: [7]}};
let calls = 0;
const value = {{toString() {{ calls++; xs.len = 0; xs._data.length = 0; return 'foreign'; }}}};
let caught = false;
try {{ after_conversion(xs, value); }} catch (error) {{
    caught = String(error).includes('Vec index out of bounds');
}}
if (!caught || calls !== 1) throw new Error('callback left a stale bounds proof');
console.log('ok');
"#,
        runtime = skull_codegen_js::JS_VEC_CHECK_SHIMS,
    ));
    assert_eq!(output.trim(), "ok");
}

#[test]
fn foreign_join_access_invalidates_vector_bounds() {
    if !require_node() {
        return;
    }
    let ws = ws_with(
        r#"
@nomangle fn after_join(xs: [int], parts: [string]) -> int {
    if xs.len() <= 0 { return -1 }
    let text = join(parts, ",")
    if text == "" { return -2 }
    return xs[0]
}
"#,
    );
    let js = emit_closure(&ws, "after_join").expect("emit join callback probe");
    let output = run_node(&format!(
        r#"{runtime}
{js}
const xs = {{len: 1, _data: [7]}};
let calls = 0;
const parts = {{len: 1, _data: new Proxy(['a'], {{get(target, key) {{
    if (key === '0') {{ calls++; xs.len = 0; xs._data.length = 0; }}
    return Reflect.get(target, key);
}}}})}};
let caught = false;
try {{ after_join(xs, parts); }} catch (error) {{
    caught = String(error).includes('Vec index out of bounds');
}}
if (!caught || calls !== 1) throw new Error('callback left a stale bounds proof');
console.log('ok');
"#,
        runtime = skull_codegen_js::JS_VEC_CHECK_SHIMS,
    ));
    assert_eq!(output.trim(), "ok");
}
