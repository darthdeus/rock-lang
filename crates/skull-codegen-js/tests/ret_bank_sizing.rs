//! Pin bank declarations for MIR results and opaque inline publishers.
//! Sloppy-mode JS can write an undeclared slot without raising an error.

use crate::common::ws_with;

fn emit(src: &str, root: &str) -> String {
    let ws = ws_with(src);
    crate::common::emit_closure(&ws, root).unwrap_or_else(|e| panic!("emit: {e}"))
}

#[test]
fn each_published_lane_source_declares_the_bank() {
    let cases = [
        (
            "single inline publisher lane",
            r#"
@nomangle inline(js) fn publish(v: int) -> (bool, int) = "return __rock_publish(false, v);"
fn main() { let (ok, n) = publish(1) }
"#,
            "main",
            "var __rock_ret1",
        ),
        (
            "wide inline publisher result",
            r#"
nobox value P2 { x: float, y: float }
@nomangle @scalar_abi inline(js) fn publish_wide(p: P2) -> (bool, P2) =
    "return __rock_publish(false, p__x, p__y);"
fn main() { let (ok, p) = publish_wide(P2 { x: 1.0, y: 2.0 }) }
"#,
            "main",
            "var __rock_ret1, __rock_ret2",
        ),
        (
            "opaque-only publisher with no MIR call site",
            r#"
@nomangle inline(js) fn lone_publisher(v: int) -> (bool, int) = "return __rock_publish(false, v);"
@nomangle inline(js) fn seam() -> int = "lone_publisher(1); return 0;"
fn main() { let _ = seam() }
"#,
            "main",
            "var __rock_ret1",
        ),
        (
            "called owned MIR result plan",
            r#"
nobox value P2 { x: float, y: float }
@scalar_abi fn pair(p: P2) -> P2 { return p }
fn main() {
    let p = P2 { x: 1.0, y: 2.0 }
    let q = pair(p)
    print("{q.x}")
}
"#,
            "main",
            "var __rock_ret1",
        ),
        (
            "rooted owned MIR result without a call site",
            r#"
nobox value P2 { x: float, y: float }
@scalar_abi fn pair(p: P2) -> P2 { return p }
"#,
            "pair",
            "var __rock_ret1",
        ),
        (
            "hoisted nested MIR body",
            r#"
nobox value P2 { x: float, y: float }
fn outer(p: P2) -> P2 {
    fn pair(q: P2) -> P2 { return q }
    return pair(p)
}
fn main() {
    let p = P2 { x: 1.0, y: 2.0 }
    let q = outer(p)
    print("{q.x}")
}
"#,
            "main",
            "var __rock_ret1",
        ),
    ];

    for (label, source, root, marker) in cases {
        let js = emit(source, root);
        assert!(js.contains(marker), "{label}: expected `{marker}`:\n{js}");
    }
}

/// **The bank is not sized by the body TEXT.** A slot named in a comment widens
/// nothing -- and writing one directly rejects, which is the legacy mechanism
/// this sizing replaced.
#[test]
fn body_text_neither_widens_the_bank_nor_writes_it() {
    let js = emit(
        r#"
@nomangle inline(js) fn commented() -> int = "// not a lane: __rock_ret7
return 1;"
fn main() { let _ = commented() }
"#,
        "main",
    );
    assert!(
        !js.contains("var __rock_ret"),
        "a commented slot name sized a bank:\n{js}"
    );

    let ws = ws_with(
        r#"
@nomangle inline(js) fn hand_published() -> int = "__rock_ret1 = 7; return 1;"
fn main() { let _ = hand_published() }
"#,
    );
    let e =
        crate::common::emit_closure(&ws, "main").expect_err("a hand-published slot must reject");
    assert!(e.contains("scratch bank"), "unexpected rejection: {e}");
}

/// The control: no published lane and no multi-value wire means no bank at
/// all. Without it the assertions above would pass just as well if the emitter
/// had started declaring slots unconditionally.
#[test]
fn a_program_with_no_wire_declares_no_bank() {
    let js = emit(
        r#"
@nomangle inline(js) fn plain() -> int = "return 1;"
fn main() { let _ = plain() }
"#,
        "main",
    );
    assert!(
        !js.contains("var __rock_ret"),
        "no wire, so no bank was expected:\n{js}"
    );
}
