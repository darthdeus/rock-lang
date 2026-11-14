use crate::common::{emit_closure, require_node, run_node, ws_with};

#[test]
fn bounded_affine_native_buffer_loop_matches_js_boundaries() {
    if !require_node() {
        return;
    }
    let ws = ws_with(include_str!("../../../benchmarks/guard_proofs.rock"));
    let js = emit_closure(&ws, "guard_stride_proven").unwrap();
    let js = format!("{}\n{js}", skull_codegen_js::JS_NB_CHECK_SHIMS);
    let output = run_node(&format!(
        r#"{js}
const buf = {{ptr: Int32Array.from({{length:16}}, (_, i) => i+1), len:16}};
for (const count of [-1, 0, 0.5, 1, 1.9, 4, 5, 100001, Infinity, -Infinity, NaN]) {{
    const n = Math.floor(count);
    const expected = n >= 0 && n <= 100000 && buf.len >= n * 4
        ? n * 4 * (n * 4 + 1) / 2 : -1;
    if (guard_stride_proven(buf, count) !== expected) throw new Error('wrong affine result');
}}
buf.len = 3;
if (guard_stride_proven(buf, 1) !== -1) throw new Error('short buffer');
console.log('ok');
"#
    ));
    assert_eq!(output.trim(), "ok");
}
