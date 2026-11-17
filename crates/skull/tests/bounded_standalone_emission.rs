use crate::common::{emit_module, inline_off_guard, run_main};

const BOUNDED: &str = r#"
fn total<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for v in it { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs.iter())}")
}
"#;

const NESTED: &str = r#"
struct Item { id: int, p: float }

fn main() {
    let xs = [
        Item { id: 1, p: 3.0 },
        Item { id: 2, p: 2.0 },
    ]
    let ids = xs.map(_.id).collect()
    print("{ids[0]}")
}
"#;

#[test]
fn bounded_bodies_and_nested_witnesses_run_without_inlining() {
    let _inline = inline_off_guard();
    for (source, expected) in [
        (BOUNDED.to_string(), "6"),
        (
            BOUNDED
                .replace("<I: Iterator<int>>(it: I)", "(it: [int])")
                .replace("xs.iter()", "xs"),
            "6",
        ),
        (NESTED.to_string(), "1"),
    ] {
        assert_eq!(run_main(&emit_module(&source)), expected);
    }
}
