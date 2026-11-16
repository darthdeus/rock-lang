use crate::common::{emit_closure, run_node, ws_with};

#[test]
fn foreign_type_tag_getter_invalidates_vector_bounds() {
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
    let output = run_node(&format!(
        r#"{runtime}
{js}
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
"#,
        runtime = skull_codegen_js::JS_VEC_CHECK_SHIMS,
    ));
    assert_eq!(output.trim(), "ok");
}
