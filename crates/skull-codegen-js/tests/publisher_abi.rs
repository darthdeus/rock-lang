//! Inline JS publishers must match the concrete result ABI and may access
//! secondary lanes only through the publisher contract.

use skull_mir::lower::ScalarCtx;

use crate::common::ws_with;

fn emit_res(src: &str, root: &str) -> Result<String, String> {
    let ws = ws_with(src);
    crate::common::emit_closure(&ws, root)
}

fn emit(src: &str, root: &str) -> String {
    emit_res(src, root).unwrap_or_else(|e| panic!("emit: {e}"))
}

fn reject(src: &str, root: &str, needle: &str) {
    match emit_res(src, root) {
        Ok(js) => panic!("expected a rejection mentioning `{needle}`, emitted:\n{js}"),
        Err(m) => assert!(
            m.contains(needle),
            "rejection should mention `{needle}`, got: {m}"
        ),
    }
}

/// The lanes of a declared return, from the ABI. The fixture set covers every
/// shape the real web stubs use.
fn lanes(src: &str, name: &str) -> usize {
    let ws = ws_with(src);
    let scx = ScalarCtx::new(&ws).with_target(bone_core::hir::FnBackend::Js);
    skull_mir::lower::BackendFnAbiPlan::for_boundary_declaration(
        &ws,
        &scx,
        crate::common::test_fn_ref(&ws, name),
    )
    .result_lanes()
}

const SHAPES: &str = r#"
nobox value Vec2P { x: float, y: float }

@scalar_abi inline(js) fn pub_value() -> Vec2P = "return __rock_publish(0.0, 0.0);"
inline(js) fn pub_pair(v: float) -> (bool, float) = "return __rock_publish(false, v);"
@scalar_abi inline(js) fn pub_pair_value(v: Vec2P) -> (bool, Vec2P) = "return __rock_publish(false, v__x, v__y);"
inline(js) fn pub_scalar(v: float) -> float = "return v;"

fn main() {
    let a = pub_value()
    let (ok, f) = pub_pair(1.0)
    let (ok2, p) = pub_pair_value(a)
    let s = pub_scalar(2.0)
}
"#;

/// The compiler ABI owns the publisher width for scalar, tuple, and value lanes.
#[test]
fn result_lanes_match_the_concrete_abi() {
    for (name, expected) in [
        ("pub_value", 2),
        ("pub_pair", 2),
        ("pub_pair_value", 3),
        ("pub_scalar", 1),
    ] {
        assert_eq!(lanes(SHAPES, name), expected, "ABI lanes for `{name}`");
    }
}

/// The publisher LOWERS to the bank: leaf 0 on the ordinary return, the rest
/// into the slots -- and the bank is declared for exactly those lanes.
#[test]
fn a_publisher_lowers_to_the_bank_and_sizes_it() {
    let js = emit(SHAPES, "main");
    assert!(
        js.contains("__rock_ret1 = ") && js.contains("__rock_ret2 = "),
        "the publisher did not lower to the scratch bank:\n{js}"
    );
    assert!(
        js.contains("var __rock_ret1, __rock_ret2"),
        "the bank was not declared for the ABI's lanes:\n{js}"
    );
    assert!(
        !js.contains("__rock_publish"),
        "the publisher pseudo-call survived into the emitted program:\n{js}"
    );
}

/// **Not a text scan.** A slot named inside a comment sizes nothing: the bank
/// covers the lanes the ABI says exist, and a body cannot widen it by talking
/// about one.
#[test]
fn a_slot_named_in_a_comment_does_not_size_the_bank() {
    let js = emit(
        r#"
inline(js) fn commented(v: float) -> float = "// __rock_ret4 is not a lane here
return v;"
fn main() { let x = commented(1.0) }
"#,
        "main",
    );
    assert!(
        !js.contains("var __rock_ret"),
        "a commented slot name still sized a bank:\n{js}"
    );
}

/// Every disagreeing shape rejects, one per arm. Each is a body that compiled
/// silently under the substring scan.
#[test]
fn a_publisher_that_disagrees_with_the_abi_rejects() {
    let with = |body: &str| {
        format!(
            r#"
inline(js) fn probe(v: float) -> (bool, float) = "{body}"
fn main() {{ let (a, b) = probe(1.0) }}
"#
        )
    };
    // Too few lanes, too many lanes.
    reject(
        &with("return __rock_publish(false);"),
        "main",
        "2 result lanes",
    );
    reject(
        &with("return __rock_publish(false, v, v);"),
        "main",
        "2 result lanes",
    );
    // No publisher at all in a multi-lane body.
    reject(&with("return false;"), "main", "__rock_publish");
    // A second return.
    reject(
        &with("if (v > 0) { return __rock_publish(true, v); } return __rock_publish(false, v);"),
        "main",
        "one final",
    );
    // Direct bank access -- the legacy mechanism, now refused.
    reject(
        &with("__rock_ret1 = v; return false;"),
        "main",
        "scratch bank",
    );
}

/// A single-lane body uses an ordinary `return`, and may use neither the
/// publisher nor the bank. Without this arm the rejections above would be
/// satisfied by requiring a publisher everywhere.
#[test]
fn a_single_lane_body_returns_ordinarily() {
    let js = emit(
        r#"
inline(js) fn scalar_probe(v: float) -> float = "return v * 2;"
fn main() { let x = scalar_probe(1.0) }
"#,
        "main",
    );
    assert!(
        js.contains("return v * 2;"),
        "the body was rewritten:\n{js}"
    );

    reject(
        r#"
inline(js) fn over_publish(v: float) -> float = "return __rock_publish(v, v);"
fn main() { let x = over_publish(1.0) }
"#,
        "main",
        "1 result lane",
    );
    reject(
        r#"
inline(js) fn scalar_bank(v: float) -> float = "__rock_ret1 = v; return v;"
fn main() { let x = scalar_bank(1.0) }
"#,
        "main",
        "scratch bank",
    );
}

/// **The publisher may not reorder its arguments.** `__rock_publish(f(), g())`
/// is an ordinary JS call expression in source order, so lowering it to
/// `__rock_ret1 = g(); return f();` runs `g` first -- a visible change of
/// effects -- and writes a lane before every argument has finished, so a throw
/// in `f()` leaves a published lane behind.
///
/// Both properties are one shape: every argument is evaluated, in order, into
/// reserved temps BEFORE any lane is written. The effects half is executed in
/// `tests/nobox/js_return_abi.rock`; the throw half is a shape assertion,
/// because a caller cannot observe an unwritten lane without reading the bank,
/// which hand-written JS may not do.
#[test]
fn a_publisher_evaluates_its_arguments_in_order_before_writing_a_lane() {
    let js = emit(
        r#"
inline(js) fn ordered(v: float) -> (bool, float) = "return __rock_publish(first(v), second(v));"
fn main() { let (a, b) = ordered(1.0) }
"#,
        "main",
    );
    let body = js
        .lines()
        .find(|l| l.contains("first(v)"))
        .unwrap_or_else(|| panic!("the publisher body is missing:\n{js}"));
    let first_at = body.find("first(v)").expect("first");
    let second_at = body.find("second(v)").expect("second");
    assert!(
        first_at < second_at,
        "the publisher swapped its arguments' evaluation order:\n{body}"
    );
    let lane_at = body.find("__rock_ret1 =").expect("lane 1 write");
    assert!(
        second_at < lane_at,
        "a lane was written before every argument had been evaluated, so a throwing \
         argument leaves a published lane behind:\n{body}"
    );
    let return_at = body.rfind("return ").expect("return");
    assert!(
        lane_at < return_at,
        "the lane write must precede the return:\n{body}"
    );
}

/// The temps are compiler-owned, so a body naming one rejects rather than
/// silently colliding with the lowering.
#[test]
fn a_body_may_not_name_the_publisher_temps() {
    reject(
        r#"
inline(js) fn collides(v: float) -> (bool, float) = "var __rock_lane0 = v; return __rock_publish(false, v);"
fn main() { let (a, b) = collides(1.0) }
"#,
        "main",
        "compiler-owned",
    );
}

#[test]
fn selected_js_sibling_owns_the_inline_abi() {
    let js = emit(
        r#"
value Pair { x: float, y: float }

inline(lua) fn probe<T>(value: T) -> int = "return 0"
@scalar_abi inline(js) fn probe<T>(value: T) -> int = "return 0;"

fn main() {
    let p = Pair { x: 1.0, y: 2.0 }
    let _ = probe(p)
}
"#,
        "main",
    );
    let definitions: Vec<&str> = js
        .lines()
        .filter(|line| line.starts_with("function probe_"))
        .collect();
    assert_eq!(
        definitions.len(),
        1,
        "the fixture needs one selected JS body:\n{js}"
    );
    assert!(
        definitions[0].contains("value__x, value__y"),
        "the selected @scalar_abi JS sibling did not own its concrete parameter ABI:\n{js}"
    );
    assert!(
        js.lines()
            .any(|line| line.contains("probe_") && line.contains("1, 2")),
        "the call site did not use the selected sibling's scalar parameter ABI:\n{js}"
    );
}

#[test]
fn selected_js_sibling_owns_result_lanes_and_the_return_bank() {
    let js = emit(
        r#"
value Pair { x: float, y: float }

inline(lua) fn publish<T>(value: T) -> Pair = "return 0"
@scalar_abi inline(js) fn publish<T>(value: T) -> Pair = "return __rock_publish(0.0, 0.0);"

fn main() {
    let p = Pair { x: 1.0, y: 2.0 }
    let _ = publish(p)
}
"#,
        "main",
    );
    assert!(
        js.contains("var __rock_ret1"),
        "the selected sibling's concrete result ABI did not size the return bank:\n{js}"
    );
    assert!(
        js.contains("__rock_ret1 = __rock_lane1"),
        "the selected sibling's publisher did not lower to its concrete result lane:\n{js}"
    );
}
