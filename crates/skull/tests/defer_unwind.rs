//! DEFER ON ERROR: a Lua function whose body
//! registers an ordinary-fn `defer` enters itself through `__rock_defer_call`,
//! which unwinds the registered cleanups when an error escapes and re-raises
//! it. A function without `defer` carries none of that machinery.
use crate::common::{emit_module, function_body, mangled_name, run_main};

const FIXTURE: &str = r#"
global LOG: string = ""

fn plain(x: int) -> int {
    if x > 0 {
        let y = x * 2
        return y
    }
    return 0
}

fn guarded(x: int) -> int {
    defer { LOG = LOG + "g;" }
    if x > 100 {
        assert(false, "too big")
    }
    return plain(x) + 1
}

inline(lua) fn raised_with(f: fn(int) -> int, x: int) -> string = """
    local ok, err = pcall(f, x)
    if ok then return "ok" end
    return "raised"
"""

fn main() {
    let v = guarded(1)
    let r = raised_with(guarded, 1000)
    print("{v} {r} {LOG}")
}
"#;

#[test]
fn only_a_defer_fn_enters_through_the_unwind_trampoline() {
    let lua = emit_module(FIXTURE);
    let name = mangled_name(&lua, "guarded");
    let body = function_body(&lua, "guarded");
    assert!(
        body.contains(&format!("return __rock_defer_call({name}, ")),
        "a fn registering a defer must re-enter itself under the unwind trampoline:\n{body}"
    );
    assert!(
        body.contains("__rock_defer_push") && body.contains("__rock_defer_pop"),
        "the defer must register its cleanup and pop it at scope exit:\n{body}"
    );
    let plain = function_body(&lua, "plain");
    assert!(
        !plain.contains("__rock_defer"),
        "a fn without defer must not pay for the protected call:\n{plain}"
    );
    assert_eq!(
        run_main(&lua),
        "3 raised g;g;",
        "defer runs on normal exit and on error"
    );
}
