use crate::common::{emit_module, function_body};

const HANDLE_EQUALITY_FIXTURE: &str = r#"
struct EqTag { n: int }

fn entity_matches(xs: [Entity<EqTag>], target: Entity<EqTag>) -> int {
    let matches = 0
    for e in xs {
        if e == target { matches += 1 }
        if e != target { matches += 2 }
    }
    return matches
}
"#;

#[test]
fn vector_handle_equality_does_not_materialize_carriers() {
    let lua = emit_module(HANDLE_EQUALITY_FIXTURE);
    let body = function_body(&lua, "entity_matches");
    assert!(body.contains("==") && (body.contains("~=") || body.contains("not")));
    assert!(!body.contains("Entity_vt("));
}

#[test]
fn only_types_without_a_native_eq_take_the_structural_walk() {
    for (src, case, walks) in [
        (
            "value Wrap { tag: int, name: string }\n\
             fn compare_for_dispatch(a: Wrap, b: Wrap) -> bool { return a == b }\n",
            "gc-carrying value",
            false,
        ),
        (
            "value V2 { x: float, y: float }\n\
             fn compare_for_dispatch(a: V2?, b: V2?) -> bool { return a == b }\n",
            "nullable value",
            false,
        ),
        (
            "struct Plain { x: int, y: int }\n\
             fn compare_for_dispatch(a: Plain, b: Plain) -> bool { return a == b }\n",
            "plain struct",
            true,
        ),
        (
            "fn compare_for_dispatch(a: [int], b: [int]) -> bool { return a == b }\n",
            "Vec",
            true,
        ),
    ] {
        let lua = emit_module(src);
        let body = function_body(&lua, "compare_for_dispatch");
        assert_eq!(body.contains("__op_eq"), walks, "{case}");
    }
}

const SCALAR_LEAF_EQUALITY: &str = r#"
value V2 { x: float, y: float }
value Handle<T> { slot: int, generation: int }

fn eq_v2(a: V2, b: V2) -> bool { return a == b }
fn ne_v2(a: V2, b: V2) -> bool { return a != b }
fn eq_handle(a: Handle<int>, b: Handle<int>) -> bool { return a == b }
fn ne_handle(a: Handle<int>, b: Handle<int>) -> bool { return a != b }

value Tagged { tag: int, handle: Handle<int> }
struct Slot { last: Tagged? = null }

fn stored_matches(slot: Slot, x: Handle<int>) -> bool {
    if let last = slot.last { return last.handle == x }
    return false
}
"#;

#[test]
fn scalar_value_equality_compares_leaves_without_materializing() {
    let lua = emit_module(SCALAR_LEAF_EQUALITY);
    for name in ["eq_v2", "ne_v2", "eq_handle", "ne_handle", "stored_matches"] {
        let body = function_body(&lua, name);
        assert!(!body.contains("_vt(") && !body.contains("__op_eq") && !body.contains("__op_ne"));
    }
}
