use crate::common::{emit_module, function_body, run_luajit};

const SOURCE: &str = r#"
value FreshPair { x: int, y: int }
struct FreshEnvelope { pair: FreshPair, spare: int }
struct FreshScalar { selected: int, discarded: int }
global marks: int = 0
fn mark(n: int) -> int { marks = marks * 10 + n; return n }
fn read(p: FreshPair) -> int { return p.x }
@nomangle fn fresh_scalar() -> int {
    return FreshScalar { selected: 7, discarded: mark(2) }.selected
}
@nomangle fn fresh_call() -> int {
    return read(FreshEnvelope { pair: FreshPair { x: 7, y: mark(2) }, spare: mark(3) }.pair)
}
@nomangle fn fresh_nested() -> int {
    return FreshEnvelope { pair: FreshPair { x: 7, y: mark(2) }, spare: mark(3) }.pair.x
}
fn change(h: FreshScalar) { h.selected = 19 }
fn retained() -> int {
    let h = FreshScalar { selected: 7, discarded: mark(2) }
    let old = h.selected
    change(h)
    return old + h.selected
}
fn main() {
    marks = 0
    print("{fresh_scalar()} {marks}")
    marks = 0
    print("{fresh_call()} {marks}")
    marks = 0
    print("{fresh_nested()} {marks}")
    marks = 0
    print("{retained()} {marks}")
}
"#;

#[test]
fn fresh_struct_projection_eliminates_the_unobservable_container() {
    let lua = emit_module(SOURCE);
    for name in ["fresh_scalar", "fresh_call", "fresh_nested"] {
        let body = function_body(&lua, name);
        assert!(!body.contains("__ty ="), "{body}");
        assert!(!body.contains("FreshPair_ct("), "{body}");
    }
}

#[test]
fn fresh_struct_projection_keeps_operand_effects_and_escaping_mutation() {
    let lua = emit_module(SOURCE);
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "fresh_struct_projection"),
        "7 2\n7 23\n7 23\n26 2");
}
