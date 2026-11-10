//! A failed `unwrap()` raises through one out-of-line helper instead of an
//! inlined traceback-and-raise block at every site.
use crate::common::{emit_module, function_body};

const SRC: &str = r#"
@nomangle fn get(x: int?) -> int { return x.unwrap() + 1 }
@nomangle fn need(x: int?) -> int { return x.expect("x is required") + 1 }
"#;

#[test]
fn unwrap_sites_call_one_failure_helper() {
    let lua = emit_module(SRC);
    for name in ["get", "need"] {
        let body = function_body(&lua, name);
        assert!(!body.contains("debug.traceback"), "{body}");
        assert!(!body.contains("error("), "{body}");
    }
    let driver = format!(
        r#"{lua}
assert(get(2) == 3 and need(4) == 5)
local function failing_caller() local v = get(nil); return v end
local ok, message = pcall(failing_caller)
assert(not ok)
assert(message:find("unwrap() called on null value", 1, true), message)
assert(message:find("in function 'get'", 1, true), message)
local ok, message = pcall(need, nil)
assert(not ok)
assert(message:find("expect() failed: x is required", 1, true), message)
print("ok")
"#
    );
    assert_eq!(crate::common::run_luajit(&driver), "ok");
}
