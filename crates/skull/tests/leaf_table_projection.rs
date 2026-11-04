use crate::common::{emit_module, function_body, run_luajit};

const SOURCE: &str = r#"
nobox value LeafPoint { x: int, y: int }
value LeafState { armed: bool, point: LeafPoint, anchor: LeafPoint?, age: float }
global STATE: LeafState = LeafState {
    armed: false, point: LeafPoint { x: 7, y: 9 },
    anchor: LeafPoint { x: 11, y: 13 }, age: 2.0
}
@nomangle fn flag(p: ref LeafState) -> bool { return p.armed }
fn replace(p: ref LeafState) {
    p.armed = true
    p.point = LeafPoint { x: 17, y: 19 }
    p.anchor = null
}
fn check(p: ref LeafState) {
    let before = p.armed
    let point = p.point
    let anchor = p.anchor
    replace(p)
    print("{before} {point.x} {point.y} {anchor?.x ?? -1} {anchor?.y ?? -1}")
    print("{p.armed} {p.point.x} {p.point.y} {p.anchor?.x ?? -1}")
}
fn main() { check(STATE) }
"#;

#[test]
fn leaf_table_ref_field_emits_only_the_selected_read() {
    let lua = emit_module(SOURCE);
    let body = function_body(&lua, "flag");
    assert!(body.contains(".armed"), "{body}");
    for sibling in ["point", "anchor", "age"] {
        assert!(!body.contains(sibling), "unread sibling {sibling}: {body}");
    }
}

#[test]
fn leaf_table_ref_projection_captures_values_before_mutation() {
    let lua = emit_module(SOURCE);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "leaf_table_projection"),
        "false 7 9 11 13\ntrue 17 19 -1"
    );
}
