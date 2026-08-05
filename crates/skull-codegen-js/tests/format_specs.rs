//! String-interpolation format-spec emission (T8d). The `{x:spec}` helper calls
//! (__rock_fmt_pad/_grouping/_binary/_percent) plus the `Format`/`FormatDyn` MIR
//! nodes (`string.format(spec, value)`) lowered through the single-value C printf
//! port `__rock_sprintf`. The node-executed checks are the correctness oracle
//! (byte-match incl. round-half-to-even); `tests/js_format_specs.rock`
//! cross-checks against luajit.

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;
use crate::common::ws_with;

fn node_available() -> bool {
    crate::common::node().arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let scx = ScalarCtx::new(ws);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef { module: mid, func: fid });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &scx, &[(r.unwrap(), root.to_string())])
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
    let out = crate::common::node().arg("-e").arg(&program).output().expect("spawn node");
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
    assert!(js.contains("__rock_sprintf"), "renders the printf port:\n{js}");
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
