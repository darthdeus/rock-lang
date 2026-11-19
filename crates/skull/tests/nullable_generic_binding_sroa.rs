#[test]
fn a_non_nullable_value_still_scalarizes_through_a_generic_let() {
    const SRC: &str = r#"
value BV2 { x: float, y: float }

fn thru<T>(a: [T], i: int) -> T { let v = a[i]  return v }

fn main() {
    let b: [BV2] = [BV2 { x: 1.0, y: 2.0 }, BV2 { x: 3.0, y: 4.0 }]
    print("{thru(b, 1).x}")
}
"#;
    let lua = crate::common::emit_module(SRC);
    let body = lua
        .split("function thru")
        .nth(1)
        .expect("the specialized `thru` must be emitted");
    let body = body.split("\nend").next().unwrap();
    assert!(!body.contains("BV2_ct"));
    assert_eq!(crate::common::run_main(&lua), "3");
}
