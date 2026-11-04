//! String-interpolation format-spec emission (T8d). The `{x:spec}` helper calls
//! (__rock_fmt_pad/_grouping/_binary/_percent) plus the `Format`/`FormatDyn` MIR
//! nodes (`string.format(spec, value)`) lowered through the single-value C printf
//! port `__rock_sprintf`. The node-executed checks are the correctness oracle
//! (byte-match incl. round-half-to-even); `tests/js_format_specs.rock`
//! cross-checks against luajit.

use crate::common::ws_with;
use skull_mir::mir::FuncRef;

fn node_available() -> bool {
    crate::common::node()
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef {
                module: mid,
                func: fid,
            });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

fn eval_str(defs: &str, root: &str, expect: &str) {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let program = format!(
        "{}{}{}\n{defs}\nconsole.log({root}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_STRING_RUNTIME,
    );
    let out = crate::common::node()
        .arg("-e")
        .arg(&program)
        .output()
        .expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(s.trim_end_matches('\n'), expect, "wrong result\n{program}");
}

// The %f conversion rounds half-to-EVEN (C printf), NOT half-up (JS toFixed):
// 2.5 -> "2", 3.5 -> "4".
#[test]
fn fixed_point_half_even() {
    let src = "fn root() -> string {\n\
                   let a = 2.5\n\
                   let b = 3.5\n\
                   \"{a:.0f}|{b:.0f}\"\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("format emits");
    assert!(
        js.contains("__rock_sprintf"),
        "renders the printf port:\n{js}"
    );
    eval_str(&js, "root", "2|4");
}

// Padding / width / hex through the helper + Format nodes.
#[test]
fn pad_width_hex() {
    let src = "fn root() -> string {\n\
                   let x = 42\n\
                   let h = 255\n\
                   \"[{x:>5}]|{x:05d}|{h:x}\"\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("format emits");
    eval_str(&js, "root", "[   42]|00042|ff");
}

// Digit grouping composes with the %d Format node.
#[test]
fn grouping() {
    let src = "fn root() -> string { let x = 1234567; \"{x:,}\" }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("format emits");
    eval_str(&js, "root", "1,234,567");
}

// FormatDyn: format_float builds the spec at runtime.
#[test]
fn format_float_dynamic() {
    let src = "fn root() -> string { \"[{format_float(3.14159, 6, 2)}]\" }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("format_float emits");
    eval_str(&js, "root", "[  3.14]");
}

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
        let js = emit_closure(&ws, "compute").expect("emit ordering probe");
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
        let run = crate::common::node()
            .arg("-e")
            .arg(&program)
            .output()
            .expect("run ordering probe");
        assert!(
            run.status.success(),
            "{}\n{js}",
            String::from_utf8_lossy(&run.stderr)
        );
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
        let js = emit_closure(&ws, "compute").expect("emit numeric ordering probe");
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
        let run = crate::common::node()
            .arg("-e")
            .arg(&program)
            .output()
            .expect("run numeric ordering probe");
        assert!(
            run.status.success(),
            "{}\n{js}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
}
