use crate::common::{bone_rejections, emit_module, function_body, run_main, try_emit_module};

const SOURCE: &str = r#"
struct Buffer { count: int, capacity: int, value: float }

fn prepare(b: Buffer) -> Buffer {
    if b.count == b.capacity { b.capacity += 1 }
    return b
}

fn tail(b: Buffer, value: float) {
    b.value = value
    b.count += 1
}
@preflight("prepare", "tail") fn append(b: Buffer, value: float) {
    tail(prepare(b), value)
}

fn call(b: Buffer, x: float) { append(b, x * 1.25) }
fn main() {
    let b = Buffer { count: 0, capacity: 0, value: 0.0 }
    call(b, 2.0)
    print("{b.value} {b.capacity} {b.count}")
}
"#;

#[test]
fn preflight_precedes_safe_float_argument_construction() {
    let lua = emit_module(SOURCE);
    let body = function_body(&lua, "call");
    let preflight = body
        .find("b0(c0)")
        .unwrap_or_else(|| panic!("preflight call missing:\n{body}"));
    let arithmetic = body.find("1.25").expect("float arithmetic");
    assert!(preflight < arithmetic, "{body}");
    assert_eq!(run_main(&lua), "2.5 1 1");
}

#[test]
fn malformed_annotation_and_helper_are_rejected() {
    let malformed = bone_rejections("@preflight(1) fn append() {} fn main() { append() }");
    assert!(
        malformed
            .iter()
            .any(|message| message.contains("requires prepare and tail function name strings")),
        "{malformed:?}"
    );

    let missing = try_emit_module(
        r#"
struct Buffer { capacity: int }
@preflight("absent", "tail") fn append(b: Buffer, x: int) { b.capacity = x }
fn main() { let b = Buffer { capacity: 0 }; append(b, 1) }
"#,
    )
    .unwrap_err();
    assert!(
        missing.message.contains("must name one function"),
        "{missing:?}"
    );

    let no_receiver = try_emit_module(
        r#"
fn prepare() -> int { return 0 }
@preflight("prepare", "tail") fn append() {}
fn main() { append() }
"#,
    )
    .unwrap_err();
    assert!(
        no_receiver
            .message
            .contains("requires a receiver parameter"),
        "{no_receiver:?}"
    );

    let ref_receiver = try_emit_module(
        r#"
struct Buffer { capacity: int }
fn prepare(b: ref Buffer) -> Buffer { return Buffer { capacity: b.capacity } }
fn tail(b: Buffer, x: int) { b.capacity = x }
@preflight("prepare", "tail") fn append(b: Buffer, x: int) { b.capacity = x }
fn main() { let b = Buffer { capacity: 0 }; append(b, 1) }
"#,
    )
    .unwrap_err();
    assert!(
        ref_receiver.message.contains("same receiver type"),
        "{ref_receiver:?}"
    );
}

#[test]
fn named_conditional_helper_arguments_follow_preflight() {
    let source = r#"
struct Buffer { capacity: int, count: int, value: float }
fn prepare(b: Buffer) -> Buffer { if b.count == b.capacity { b.capacity += 1 }; return b }
fn tail(b: Buffer, point: Vec2, scale: float) {
    b.value = point.x * scale
    b.count += 1
}
@preflight("prepare", "tail") fn append(b: Buffer, point: Vec2, scale: float) {
    tail(prepare(b), point, scale)
}
fn point(x: float) -> Vec2 { return v(x * 0.25, x * 0.5) }
fn call(b: Buffer, x: float, use_point: bool) {
    append(b, point: if use_point { point(x) } else { v(x, x) }, scale: 1.0)
}
fn main() {
    let b = Buffer { capacity: 0, count: 0, value: 0.0 }
    call(b, 8.0, true)
    print("{b.value} {b.capacity} {b.count}")
}
"#;
    let lua = emit_module(source);
    let body = function_body(&lua, "call");
    let preflight = body.find("= b1(c0)").unwrap_or_else(|| panic!("{body}"));
    let conditional = body.find("if c2 then").unwrap_or_else(|| panic!("{body}"));
    assert!(preflight < conditional, "{body}");
    assert_eq!(run_main(&lua), "2 1 1");
}
