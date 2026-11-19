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

#[test]
fn foreign_heap_record_callbacks_invalidate_vector_bounds() {
    let ws = ws_with(
        r#"
struct ExternalRecord { value: int? }

fn read_external_record(record: ExternalRecord) -> int? {
    return record.value
}

fn write_external_record(record: ExternalRecord) {
    record.value = 1
}

@nomangle fn after_record_callback(xs: [int], record: ExternalRecord, mode: int) -> int {
    if xs.len() <= 0 { return -1 }
    if mode == 0 {
        let value = record.value
        if value == null { return -2 }
    } else if mode == 1 {
        record.value = 1
    } else if mode == 2 {
        let value = read_external_record(record)
        if value == null { return -2 }
    } else if mode == 3 {
        write_external_record(record)
    } else if mode == 4 {
        let unused = record.value
    } else if mode == 5 {
        let first = record.value
        let second = record.value
        if first == null || second == null { return -2 }
        if first + second < 0 { return -3 }
    }
    return xs[0]
}
"#,
    );
    let js = emit_closure(&ws, "after_record_callback")
        .expect("emit foreign heap record callback probe");
    assert!(
        js.contains("function read_external_record")
            && js.contains("function write_external_record"),
        "record helper routes must remain non-inlined:\n{js}"
    );
    let output = crate::common::run_js(
        &[skull_codegen_js::JS_VEC_CHECK_SHIMS],
        &format!(
            r#"{js}
const cases = [
    {{mode: 0, kind: 'get', label: 'direct getter', expectedCalls: 1, shrinkAt: 1}},
    {{mode: 1, kind: 'set', label: 'direct setter', expectedCalls: 1, shrinkAt: 1}},
    {{mode: 2, kind: 'get', label: 'helper getter', expectedCalls: 1, shrinkAt: 1}},
    {{mode: 3, kind: 'set', label: 'helper setter', expectedCalls: 1, shrinkAt: 1}},
    {{mode: 4, kind: 'get', label: 'unused getter', expectedCalls: 1, shrinkAt: 1}},
    {{mode: 5, kind: 'get', label: 'repeated getter', expectedCalls: 2, shrinkAt: 2}},
];
for (const test of cases) {{
    const xs = {{len: 1, _data: [7]}};
    let calls = 0;
    const callback = () => {{
        calls++;
        if (calls === test.shrinkAt) {{ xs.len = 0; xs._data.length = 0; }}
        return 1;
    }};
    const record = test.kind === 'get'
        ? {{get value() {{ return callback(); }}}}
        : {{set value(_value) {{ callback(); }}}};
    let caught = false;
    try {{ after_record_callback(xs, record, test.mode); }} catch (error) {{
        caught = String(error).includes('Vec index out of bounds');
    }}
    if (!caught || calls !== test.expectedCalls) {{
        throw new Error(test.label + ' callback left a stale bounds proof or changed read count: ' + calls);
    }}
}}
console.log('ok');
"#
        ),
    );
    assert_eq!(output.trim(), "ok");
}
