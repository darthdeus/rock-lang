//! Changing-instance recursion is rejected on the JS backend too.
//!
//! The Lua sibling is `crates/skull/tests/user_fn_emit_rejection.rs`. This file
//! exists because that one could not see JS at all: the depth bound landed in
//! Lua reachability first, and the JS specialization queue went on draining a
//! recursion whose every demand names a strictly deeper instantiation. Each node
//! is distinct, so dedup never closes the loop -- JS did not miscompile, it
//! HUNG (`rock run --backend js` timed out at 90s with zero output while Lua
//! rejected in under a second).
//!
//! Both now read one oracle, `skull_mir::monoty::too_deep_to_instantiate`, so a
//! future change to the bound cannot reach one backend and miss the other.

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

use crate::common::ws_with;

fn emit_res(src: &str, root: &str) -> Result<String, String> {
    let ws = ws_with(src);
    let scx = ScalarCtx::new(&ws).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == root)
        .map(|(fid, _)| fid)
        .expect("root fn");
    skull_codegen_js::emit::emit_with_closure(
        &ws,
        &scx,
        &[(FuncRef { module: mid, func: fid }, root.to_string())],
    )
    .map_err(|e| e.message)
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

/// Recursion whose type arguments do NOT change. The control that keeps the
/// rejection a statement about GROWTH rather than about recursion -- and the
/// arm that would catch a bound so tight it refused ordinary generic recursion.
const SAME_INSTANCE_RECURSION: &str = r#"
fn countdown<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return 1 + countdown(x, n - 1)
}

fn main() {
    print("{countdown(1, 5)}")
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

#[test]
fn same_instance_recursion_still_emits_on_js() {
    if let Err(e) = emit_res(SAME_INSTANCE_RECURSION, "main") {
        panic!("recursion whose type arguments do not change must still emit on JS: {e}");
    }
}
