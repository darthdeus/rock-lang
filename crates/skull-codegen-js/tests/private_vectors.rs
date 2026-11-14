use crate::common::{emit_closure, require_node, run_node, ws_with};

#[test]
fn fresh_js_arrays_can_inherit_index_setters() {
    if !require_node() {
        return;
    }
    let ws = ws_with(
        r#"
@nomangle fn collect_private(xs: [int]) -> [int] {
    let out: [int] = []
    let value = xs[0]
    out.push(value)
    xs[0] = value
    return out
}
"#,
    );
    let js = emit_closure(&ws, "collect_private").unwrap();
    let output = run_node(&format!(
        r#"{runtime}
{js}
const xs = {{len:3, _data:[0,0,0]}};
let hits=0, caught=false;
Object.defineProperty(Array.prototype, '0', {{configurable:true, set(value) {{
    hits++; xs.len=0;
    Object.defineProperty(this, '0', {{value, writable:true, configurable:true, enumerable:true}});
}}}});
try {{ collect_private(xs); }} catch (error) {{
    caught=String(error).includes('Vec index out of bounds');
}} finally {{ delete Array.prototype[0]; }}
if (!caught || hits !== 1) throw new Error('inherited setter bypassed bounds checking');
console.log('ok');
"#,
        runtime = skull_codegen_js::JS_VEC_CHECK_SHIMS
    ));
    assert_eq!(output.trim(), "ok");
}
