//! Code sinking must not move a side effect ahead of a format or checked
//! numeric operation that can throw: each probe counts the marks that ran
//! before the error and compares with the unsunk order. Format-spec output is
//! tests/strings/format_specs.rock.

use crate::common::ws_with;

#[test]
fn sinking_preserves_dynamic_format_error_order() {
    for result in ["s", "\"[{s}]\""] {
        let ws = ws_with(&format!(
            r#"
fn compute(w: int, mark: fn()) -> string {{
    let x = 3.25
    let s = "{{x:{{w}}.2f}}"
    let result = {result}
    mark()
    return result
}}
"#
        ));
        let js = crate::common::emit_closure(&ws, "compute").expect("emit ordering probe");
        let program = format!(
            r#"{runtime}
{calls}
{js}
for (const width of [8, 1000, 1000000000]) {{
    let actualMarks = 0, expectedMarks = 0, actual, expected;
    let ok = true, expectedOk = true;
    try {{ actual = compute(width, () => {{ actualMarks++; }}); }} catch (_) {{ ok = false; }}
    try {{
        const s = __rock_sprintf('%' + width + '.2f', 3.25);
        expected = {control};
        expectedMarks++;
    }} catch (_) {{ expectedOk = false; }}
    if (ok !== expectedOk || actualMarks !== expectedMarks || (ok && actual !== expected))
        throw new Error('side effects before error: ' + actualMarks + ' vs ' + expectedMarks);
    if (width === 1000000000 && (ok || actualMarks !== 0)) throw new Error('expected format error');
}}
"#,
            runtime = skull_codegen_js::JS_STRING_RUNTIME,
            calls = skull_codegen_js::JS_CALL_HELPER,
            control = if result == "s" { "s" } else { "'[' + s + ']'" }
        );
        crate::common::run_node(&program);
    }
}

#[test]
fn sinking_preserves_checked_numeric_error_order() {
    for (ty, expr, bad) in [
        ("float", "x as uint", "-1"),
        ("uint", "(1 as uint) << x", "32"),
    ] {
        let ws = ws_with(&format!(
            "fn compute(x: {ty}, mark: fn()) -> uint {{ let v = {expr}; let result = v + (1 as uint); mark(); return result }}"
        ));
        let js = crate::common::emit_closure(&ws, "compute").expect("emit numeric ordering probe");
        let program = format!(
            r#"{calls}
{js}
for (const input of [2, {bad}]) {{
    let marks = 0, ok = true;
    try {{ compute(input, () => {{ marks++; }}); }} catch (_) {{ ok = false; }}
    if (ok !== (input === 2) || marks !== (ok ? 1 : 0))
        throw new Error('side effects before checked numeric error: ' + marks);
}}
"#,
            calls = skull_codegen_js::JS_CALL_HELPER
        );
        crate::common::run_node(&program);
    }
}
