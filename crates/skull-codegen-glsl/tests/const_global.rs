//! Const globals lower to GLSL as folded literals; mutable globals stay rejected.
use crate::support::emit_library as emit;

#[test]
fn const_values_inline_as_literals() {
    let src = r#"
        const K: float = 3.5
        const N: int = 7
        const NEGATIVE: float = -2.25
        const BASE: float = 1.5
        const ALIAS: float = BASE
        @pure fn plus_n(x: int) -> int { return x + N }
        @pure fn scaled(x: float) -> float { return x * K + NEGATIVE + ALIAS + cos(PI) }
    "#;
    let glsl = emit(src, &["plus_n", "scaled"]).expect("emit");
    for (name, literal) in [
        ("float", "3.5"),
        ("int", "x + 7"),
        ("negative float", "-2.25"),
        ("alias chain", "1.5"),
        ("stdlib PI", "3.14159"),
    ] {
        assert!(glsl.contains(literal), "{name}: missing {literal}:\n{glsl}");
    }
    for global in [" K", " N", " NEGATIVE", " BASE", " ALIAS", "PI"] {
        assert!(
            !glsl.contains(global),
            "global name {global} leaked into GLSL:\n{glsl}"
        );
    }
}

/// Purity would reject a mutable-global read first, so this stays a non-pure fn.
#[test]
fn mutable_global_still_rejects() {
    let src = r#"
        global G: float = 3.5
        fn scaled(x: float) -> float {
            return x * G
        }
    "#;
    let err = emit(src, &["scaled"]).expect_err("mutable global must reject");
    assert!(err.contains("global read"), "unexpected message: {err}");
}

/// A const with a non-literal initializer names that cause rather than a generic global read.
#[test]
fn non_foldable_const_rejects_with_specific_message() {
    let src = r#"
        @pure fn seed() -> float { return 2.0 }
        const C: float = seed()
        @pure fn scaled(x: float) -> float {
            return x * C
        }
    "#;
    let err = emit(src, &["scaled"]).expect_err("non-foldable const must reject");
    assert!(
        err.contains("constant expression") && err.contains('C'),
        "unexpected message: {err}"
    );
}
