use crate::common::{emit_closure, ws_with};

#[test]
fn foreign_callbacks_in_builtins_invalidate_vector_bounds() {
    for (param, call, empty, arg, calls) in [
        (
            "value: any",
            "__rock_get_type(value)",
            "null",
            "{get __ty() { shrink(); return 'foreign'; }}",
            "calls === 0",
        ),
        (
            "value: any",
            "ffi_string(value, null)",
            "\"\"",
            "{toString() { shrink(); return 'foreign'; }}",
            "calls !== 1",
        ),
        (
            "parts: [string]",
            "join(parts, \",\")",
            "\"\"",
            "{len: 1, _data: new Proxy(['a'], {get(target, key) {
                if (key === '0') shrink();
                return Reflect.get(target, key);
            }})}",
            "calls !== 1",
        ),
    ] {
        let ws = ws_with(&format!(
            "@nomangle fn probe(xs: [int], {param}) -> int {{\n\
                 if xs.len() <= 0 {{ return -1 }}\n\
                 let result = {call}\n\
                 if result == {empty} {{ return -2 }}\n\
                 return xs[0]\n\
             }}"
        ));
        let js = emit_closure(&ws, "probe").expect("emit callback probe");
        let output = crate::common::run_js(
            &[skull_codegen_js::JS_VEC_CHECK_SHIMS],
            &format!(
                r#"{js}
const xs = {{len: 1, _data: [7]}};
let calls = 0;
const shrink = () => {{ calls++; xs.len = 0; xs._data.length = 0; }};
let caught = false;
try {{ probe(xs, {arg}); }} catch (error) {{
    caught = String(error).includes('Vec index out of bounds');
}}
if (!caught || {calls}) throw new Error('{call}: callback left a stale bounds proof');
console.log('ok');
"#
            ),
        );
        assert_eq!(output.trim(), "ok");
    }
}
