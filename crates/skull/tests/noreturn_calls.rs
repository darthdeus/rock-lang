//! A call to a raising intrinsic ends its block: nothing after it is emitted.
use crate::common::{emit_module, function_body, run_main};

#[test]
fn statements_after_error_are_not_emitted() {
    let lua = emit_module(
        r#"
@nomangle fn check(n: int) -> int {
    if n < 0 {
        error("negative")
        print("after")
    }
    return n * 2
}
fn main() { print(check(3)) }
"#,
    );
    let body = function_body(&lua, "check");
    assert!(body.contains("error("), "{body}");
    assert!(!body.contains("after"), "{body}");
    assert!(!body.contains("unreachable block"), "{body}");
    assert_eq!(run_main(&lua), "6");
}

#[test]
fn a_failed_unwrap_ends_its_branch() {
    let lua = emit_module(
        r#"
@nomangle fn get(x: int?) -> int {
    let v = x.unwrap()
    return v + 1
}
fn main() { print(get(4)) }
"#,
    );
    let body = function_body(&lua, "get");
    let lines: Vec<&str> = body.lines().collect();
    let raise = lines
        .iter()
        .position(|l| l.contains("__unwrap_failed") || l.contains("error("))
        .unwrap_or_else(|| panic!("{body}"));
    assert!(
        lines[raise + 1..]
            .iter()
            .take_while(|l| !l.trim_start().starts_with("end"))
            .all(|l| l.trim().is_empty()),
        "{body}"
    );
    assert_eq!(run_main(&lua), "5");
}

#[test]
fn a_path_that_only_raises_kills_no_locals() {
    let lua = emit_module(
        r#"
struct W { n: int }
@nomangle fn live(w: W) -> bool { return w.n > 0 }
@nomangle fn check(a: int, b: int, c: int, w: W?) -> bool {
    let ok = a > 0
    if ok {
        ok = w != null
    }
    if ok {
        ok = live(w.unwrap())
    }
    return ok && b > c
}
fn main() { print(check(1, 2, 1, W { n: 1 })) }
"#,
    );
    let body = function_body(&lua, "check");
    let lines: Vec<&str> = body.lines().collect();
    let raise = lines
        .iter()
        .position(|l| l.contains("__unwrap_failed"))
        .unwrap_or_else(|| panic!("{body}"));
    assert!(!lines[raise - 1].contains("= nil"), "{body}");
    assert_eq!(run_main(&lua), "true");
}
