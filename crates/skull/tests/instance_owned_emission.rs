use crate::common::{emit_module, run_main};

#[test]
fn iterator_witnesses_deliver_their_standalone_bodies() {
    let _inline = crate::common::InlineOffGuard::on();
    let lua = emit_module(
        r#"
fn main() {
    let xs = [1, 2]
    let sum = 0
    for v in xs.iter().map(_ * 2) { sum += v }
    print("{xs.map(_ * 2).sum()} {sum}")
}
"#,
    );
    assert_eq!(run_main(&lua), "6 6");
}

#[test]
fn specializations_emit_in_a_stable_order() {
    let mut src = String::from(
        "inline(lua) fn tag<V>(value: V) -> int = \"return 1\"\n\
         fn count<V>(items: [V]) -> int {\n    let n = 0\n    for _x in items { n += tag(_x) }\n    return n\n}\n",
    );
    let mut body = String::new();
    for i in 0..32 {
        src.push_str(&format!(
            "struct Item{i} {{ v: int }}\n\
             fn pick{i}() -> int {{ return count([Item{i} {{ v: {i} }}]) }}\n"
        ));
        body.push_str(&format!("    n += pick{i}()\n"));
    }
    src.push_str(&format!(
        "fn main() {{\n    let n = 0\n{body}    print(\"{{n}}\")\n}}\n"
    ));
    let first = emit_module(&src);
    let specs = crate::common::definition_names(&first)
        .into_iter()
        .filter(|name| name.starts_with("count_"))
        .count();
    assert_eq!(specs, 32);
    for _ in 0..16 {
        assert_eq!(emit_module(&src), first);
    }
}
