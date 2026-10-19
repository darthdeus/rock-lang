//! The JS NativeBuffer checked-store shim (`__nb_checked_set`), run under
//! node. Gated on `node` being present -- absent, skip WITH A LOG.

use skull_codegen_js::JS_NB_CHECK_SHIMS;

fn tool_available(tool: &str, arg: &str) -> bool {
    crate::common::tool(tool).arg(arg).output().map(|o| o.status.success()).unwrap_or(false)
}

fn run_tool(tool: &str, path: &std::path::Path) -> std::process::Output {
    crate::common::tool(tool).arg(path).output().unwrap_or_else(|e| panic!("spawn {tool}: {e}"))
}

fn write_temp(tag: &str, ext: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "skull_js_nbdiff_{}_{tag}.{ext}",
        std::process::id()
    ));
    std::fs::write(&path, contents).expect("write temp program");
    path
}

// The pinned NaN store edge: NaN into an INTEGER buffer is a programmer error -- the JS checked-set
// shim rejects it (debug check; currently unconditional, the JS harness has no
// release mode). NaN into a FLOAT buffer stores normally. JS-side only: on
// luajit the store is backend-native, so there is nothing to compare.
#[test]
fn nb_js_checked_set_rejects_nan_into_int_buffer() {
    if !tool_available("node", "--version") {
        eprintln!("SKIP nb_js_checked_set_rejects_nan_into_int_buffer: `node` not on PATH");
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
    let path = write_temp("nan_check", "js", &prog);
    let out = run_tool("node", &path);
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "node failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true");
}
