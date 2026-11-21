use crate::common::{emit_module, function_body};

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
