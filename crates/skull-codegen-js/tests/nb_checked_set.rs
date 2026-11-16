//! tests/native_buffers/element_types.rock runs typed buffer stores and reads
//! on both backends; this is the JS store check LuaJIT has no counterpart for.

use skull_codegen_js::JS_NB_CHECK_SHIMS;

// The pinned NaN store edge: NaN into an INTEGER buffer is a programmer error -- the JS checked-set
// shim rejects it (debug check; currently unconditional, the JS harness has no
// release mode). NaN into a FLOAT buffer stores normally. JS-side only: on
// luajit the store is backend-native, so there is nothing to compare.
#[test]
fn nb_js_checked_set_rejects_nan_into_int_buffer() {
    if !crate::common::require_node() {
        return;
    }
    let prog = format!(
        "{JS_NB_CHECK_SHIMS}\n\
         var ib = {{ ptr: new Int32Array(1), len: 1 }};\n\
         var thrown = false;\n\
         try {{ __nb_checked_set(ib, 0, NaN); }} catch (e) {{ thrown = String(e).indexOf('NaN') >= 0; }}\n\
         var fb = {{ ptr: new Float32Array(1), len: 1 }};\n\
         __nb_checked_set(fb, 0, NaN);\n\
         console.log(thrown && (fb.ptr[0] !== fb.ptr[0]));"
    );
    let path = crate::common::write_temp("js", &prog);
    let out = crate::common::run_file("node", &path);
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "node failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true");
}
