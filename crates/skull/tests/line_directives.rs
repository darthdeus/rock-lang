use crate::common;
use skull::mir_emit::ShapeOverrides;

const FILE: &str = "/src/game.rock";

#[test]
fn a_call_keeps_its_own_numbered_statement() {
    let src = "fn twice(n: int) -> int { return n * 2 }
fn split(n: int) -> int {
    let a = twice(n)
    return a + 1
}
fn main() { print(split(1)) }
";
    let emit = |line_directives| {
        common::emit_module_shaped(
            src,
            FILE,
            ShapeOverrides {
                line_directives: Some(line_directives),
                ..Default::default()
            },
        )
    };
    let assigned = |lua: &str| {
        common::function_body(lua, "split")
            .lines()
            .any(|line| line.starts_with(" r0 = "))
    };
    let plain = emit(false);
    assert!(!assigned(&plain));
    let numbered = emit(true);
    assert!(assigned(&numbered));
}
