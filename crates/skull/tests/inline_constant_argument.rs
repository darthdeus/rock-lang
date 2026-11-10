//! A constant argument reaches the reads of an inlined branching callee, so the
//! callee's test on it folds and only the taken arm is emitted.
use crate::common::{compile_and_run, emit_module, function_body};

const SRC: &str = r#"
fn side(x: int) -> int { return x * 7 + 1 }

@inline fn pick(flag: bool, x: int) -> int {
    if flag { return side(x) }
    return x + 2
}

fn caller(x: int) -> int { return pick(false, x) }

fn main() { print("{caller(5)}") }
"#;

#[test]
fn inlined_branch_on_constant_argument_folds() {
    let lua = emit_module(SRC);
    let body = function_body(&lua, "caller");
    assert!(
        !body.contains("if "),
        "the arm a constant `false` never takes must fold away; got:\n{body}"
    );
    assert_eq!(compile_and_run(SRC).trim(), "7");
}
