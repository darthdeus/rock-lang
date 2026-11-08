//! try_call emission (T8d). The test-local `pcall(f); return not ok` maps to
//! `__try_call` (try/catch over `__rock_call`). Also pins the checked-index
//! side-effect rule: an UNUSED Vec/NativeBuffer index read is emitted as a
//! statement (its bounds check throws), never dropped as a pure value -- so
//! `try_call(|| { let _ = xs[oob] })` reports the OOB error luajit raises.
//! `tests/library/js_try_call.rock` cross-checks against luajit.

use crate::common::ws_with;

fn eval_int(defs: &str, root: &str, expect: i64) {
    crate::common::eval_int(
        &[
            skull_codegen_js::JS_NB_CHECK_SHIMS,
            skull_codegen_js::JS_VEC_CHECK_SHIMS,
            skull_codegen_js::JS_CALL_HELPER,
        ],
        defs,
        root,
        expect,
    );
}

const TRY_CALL: &str = "@intrinsic @nomangle inline(lua) fn try_call(f: fn()) -> bool = \"\"\"\n    local ok, err = pcall(f)\n    return not ok\n\"\"\"\n";

// A thrown error is caught and reported as `true`; success as `false`.
#[test]
fn catches_and_reports() {
    let src = format!(
        "{TRY_CALL}\n\
         fn root() -> int {{\n\
             let a = if try_call(() => {{ error(\"x\") }}) {{ 1 }} else {{ 0 }}\n\
             let b = if try_call(() => {{ let z = 1 + 1 }}) {{ 1 }} else {{ 0 }}\n\
             a * 10 + b\n\
         }}"
    );
    let ws = ws_with(&src);
    let js = crate::common::emit_closure(&ws, "root").expect("try_call emits");
    assert!(
        js.contains("__try_call"),
        "renders the try/catch runtime:\n{js}"
    );
    eval_int(&js, "root", 10); // errored -> 1, ok -> 0
}

// An UNUSED out-of-bounds Vec read is emitted as a statement (its check throws),
// so try_call catches it -- the checked-index side-effect rule.
#[test]
fn unused_oob_index_still_throws() {
    let src = format!(
        "{TRY_CALL}\n\
         fn root() -> int {{\n\
             let xs = [1, 2, 3]\n\
             if try_call(() => {{ let _ = xs[10] }}) {{ 1 }} else {{ 0 }}\n\
         }}"
    );
    let ws = ws_with(&src);
    let js = crate::common::emit_closure(&ws, "root").expect("try_call emits");
    assert!(
        js.contains("__vec_checked_get"),
        "the unused checked read must be emitted, not dropped:\n{js}"
    );
    eval_int(&js, "root", 1);
}

// An in-bounds unused read does not error.
#[test]
fn unused_inbounds_index_ok() {
    let src = format!(
        "{TRY_CALL}\n\
         fn root() -> int {{\n\
             let xs = [1, 2, 3]\n\
             if try_call(() => {{ let _ = xs[0] }}) {{ 1 }} else {{ 0 }}\n\
         }}"
    );
    let ws = ws_with(&src);
    let js = crate::common::emit_closure(&ws, "root").expect("try_call emits");
    eval_int(&js, "root", 0);
}
