//! Flat-enum inspect metadata on JS. Emitted programs only ever build exact
//! payload layouts (tests/patterns/js_enum_boxes.rock runs those on both
//! backends); a box from opaque JS can carry a stray slot, a colliding named key
//! or an inactive trailing descriptor, and the runtime must then keep every label
//! positional rather than partially rename it.

use crate::common::ws_with;

#[test]
fn inspect_flat_enum_payload_names_require_exact_leaf_count() {
    if !crate::common::require_node() {
        return;
    }
    let src = "nobox value Pos { x: int, y: int }\n\
               enum Event { At(Pos), Maybe(Pos, int?), Named { x: int }, Text(string) }\n\
               fn root() -> int { 0 }";
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").expect("flat enum metadata emits");
    let program = format!(
        "{}{}{}{}{}{}{}\n{js}\nconsole.log(inspect({{__ty: \"Event\", __variant: \"At\", 0: 1, 1: 2, 2: 3}}));\n\
         console.log(inspect({{__ty: \"Event\", __variant: \"Maybe\", 0: 1, 1: 2}}));\n\
         console.log(inspect({{__ty: \"Event\", __variant: \"At\", 0: 1, 1: 2, x: 99}}));",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_STRUCTURAL_EQ,
        skull_codegen_js::JS_ENUM_RUNTIME,
        skull_codegen_js::JS_INSPECT_RUNTIME,
        skull_codegen_js::JS_STRING_RUNTIME,
    );
    let out = crate::common::run_node(&program);
    assert_eq!(
        out.trim(),
        "Event::At {\n  [1] = 1,\n  [2] = 2,\n  [3] = 3\n}\nEvent::Maybe { [1] = 1, [2] = 2 }\nEvent::At {\n  [1] = 1,\n  [2] = 2,\n  x = 99\n}"
    );
}
