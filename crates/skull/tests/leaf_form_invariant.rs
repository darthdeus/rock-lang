//! A flattened value-struct ctor must carry EXACTLY its type's canonical leaf
//! sequence. The generated `_lct` is positional, so a partial or reordered
//! flattening writes one leaf's value into another with no diagnostic -- the
//! silent-wrong-values class.
//!
//! The check REJECTS rather than panics: it runs on every emit, and a panic
//! takes `rock lsp serve` and the host watch command's watch loop down with a backtrace
//! and no diagnostic. Nothing in the emit path can return a `Result`, so the
//! rejection goes through `CodegenCtx`'s sink and surfaces at the emit boundary.

use crate::common::{key, ws_with_stdlib};

const NESTED_NOBOX: &str = r#"
nobox value NV2 { x: float, y: float }
value E5 { pos: NV2, a: float, b: float, c: float }
fn main() {
    let e = E5 { pos: NV2 { x: 1.0, y: 2.0 }, a: 3.0, b: 4.0, c: 5.0 }
    print("{e.a}")
}
"#;

#[test]
fn a_non_canonical_leaf_sequence_rejects() {
    let ws = ws_with_stdlib(NESTED_NOBOX);
    let name = key(&ws, "E5");
    let arena = skull_mir::lower::RepArena::default();
    let plan_id = arena
        .plan_nominal_value(&ws, bone_semantic::hir::FnBackend::Lua, name)
        .expect("E5 has a representation plan");
    let plan = arena.get(plan_id);
    let canonical = plan.leaf_paths().expect("E5 has a leaf layout");
    assert!(canonical.len() > 2);

    assert!(skull_codegen_lua::leaf_form_violation(plan, &canonical).is_none());

    let err = skull_codegen_lua::leaf_form_violation(plan, &canonical[..canonical.len() - 1])
        .expect("a truncated flattening must reject");
    assert!(err.message.contains("canonical leaf"));

    let mut swapped = canonical.clone();
    swapped.swap(0, 1);
    assert!(skull_codegen_lua::leaf_form_violation(plan, &swapped).is_some());
}
