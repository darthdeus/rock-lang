use crate::common::{emit_mir, try_emit_module};

const POLYMORPHIC_RECURSION: &str = r#"
fn rec<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return rec([x], n - 1)
}

fn main() {
    print("{rec(1, 3)}")
}
"#;

const SAME_INSTANCE_RECURSION: &str = r#"
fn countdown<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return 1 + countdown(x, n - 1)
}

fn main() {
    print("{countdown(1, 5)}")
}
"#;

#[test]
fn user_fn_emit_failure_is_a_hard_error() {
    let err = try_emit_module(POLYMORPHIC_RECURSION)
        .expect_err("an unemittable user fn must reject, not silently skip");
    assert!(err.message.contains("rec"));
    assert!(err.message.contains("polymorphic recursion"));
}

#[test]
fn same_instance_recursion_still_emits() {
    if let Err(e) = try_emit_module(SAME_INSTANCE_RECURSION) {
        panic!(
            "recursion whose type arguments do not change must still emit: {}",
            e.message
        );
    }
}

#[test]
fn stdlib_lowers_for_a_plain_program() {
    let emit = emit_mir("fn main() { print(1) }");
    assert!(emit.lua.contains("main"));
}

#[test]
fn an_ordinary_generic_call_still_emits() {
    let src = r#"
fn ident<T>(x: T) -> T { return x }

fn main() {
    print("{ident(1)} {ident(\"s\")}")
}
"#;
    if let Err(e) = try_emit_module(src) {
        panic!(
            "a generic call that DELIVERS its instantiation must emit -- the \
             declaration-body gate must not refuse generics outright: {}",
            e.message
        );
    }
}
