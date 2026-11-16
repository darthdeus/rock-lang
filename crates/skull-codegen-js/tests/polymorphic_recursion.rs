//! Changing-instance recursion is rejected on the JS backend too.
//!
//! The Lua sibling is `rock-lang/crates/skull/tests/user_fn_emit_rejection.rs`. This file
//! exists because that one could not see JS at all: the depth bound landed in
//! Lua reachability first, and the JS specialization queue went on draining a
//! recursion whose every demand names a strictly deeper instantiation. Each node
//! is distinct, so dedup never closes the loop -- JS did not miscompile, it
//! HUNG (`rock run --backend js` timed out at 90s with zero output while Lua
//! rejected in under a second).
//!
//! Both now read one oracle, `skull_mir::monoty::too_deep_to_instantiate`, so a
//! future change to the bound cannot reach one backend and miss the other.
//! functions/generic_self_recursion.rock checks that same-instance recursion
//! still runs on both.

use crate::common::ws_with;

fn emit_res(src: &str, root: &str) -> Result<String, String> {
    let ws = ws_with(src);
    crate::common::emit_closure(&ws, root)
}

/// `rec<T>` calls itself at `[T]`, so every call needs a body for a strictly
/// deeper type. Unemittable by DECISION (plan S 0h), which is why it is a safe
/// permanent fixture rather than one that goes green when some gap closes.
const POLYMORPHIC_RECURSION: &str = r#"
fn rec<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return rec([x], n - 1)
}

fn main() {
    print("{rec(1, 3)}")
}
"#;

#[test]
fn changing_instance_recursion_rejects_on_js() {
    let err = emit_res(POLYMORPHIC_RECURSION, "main")
        .expect_err("JS must reject changing-instance recursion, not drain it forever");
    assert!(
        err.contains("polymorphic recursion"),
        "the rejection must name its cause, so it is distinguishable from any \
         other JS-subset refusal: {err}"
    );
}
