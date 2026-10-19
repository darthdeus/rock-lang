//! `const` globals lower to GLSL as their folded literal; mutable `global`s
//! stay rejected (they are runtime state with no GLSL equivalent).

use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn emit(src: &str, roots: &[&str]) -> Result<String, String> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let errs = skull_mir::verdict::body_errors(&ws);
    if !errs.is_empty() {
        return Err(errs.iter().map(|e| e.message.clone()).collect::<Vec<_>>().join("; "));
    }
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let roots: Vec<String> = roots.iter().map(|s| s.to_string()).collect();
    let root_refs = roots
        .iter()
        .map(|root| skull_codegen_glsl::resolve_root(&ws, mid, root))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.message)?;
    let session = skull_codegen_glsl::shader_library_session(&ws, root_refs)
        .map_err(|e| e.message)?;
    skull_codegen_glsl::emit_shader_library_with_view(&ws, &session.scx, &session.view)
        .map_err(|e| e.message)
}

#[test]
fn const_float_inlines_as_literal() {
    let src = r#"
        const K: float = 3.5
        @pure fn scaled(x: float) -> float {
            return x * K
        }
    "#;
    let glsl = emit(src, &["scaled"]).expect("emit");
    assert!(glsl.contains("3.5"), "const value not inlined:\n{glsl}");
    // The Rock-level name has no GLSL declaration -- it folds at the use site.
    assert!(!glsl.contains(" K"), "const name leaked into GLSL:\n{glsl}");
}

#[test]
fn const_int_inlines_as_literal() {
    let src = r#"
        const N: int = 7
        @pure fn plus_n(x: int) -> int {
            return x + N
        }
    "#;
    let glsl = emit(src, &["plus_n"]).expect("emit");
    assert!(glsl.contains('7'), "const value not inlined:\n{glsl}");
}

#[test]
fn const_negative_float_inlines() {
    let src = r#"
        const K: float = -2.25
        @pure fn shift(x: float) -> float {
            return x + K
        }
    "#;
    let glsl = emit(src, &["shift"]).expect("emit");
    assert!(glsl.contains("-2.25"), "negative const not inlined:\n{glsl}");
}

/// A const initialized from another const chains through.
#[test]
fn const_alias_chain_inlines() {
    let src = r#"
        const BASE: float = 1.5
        const ALIAS: float = BASE
        @pure fn scaled(x: float) -> float {
            return x * ALIAS
        }
    "#;
    let glsl = emit(src, &["scaled"]).expect("emit");
    assert!(glsl.contains("1.5"), "aliased const not inlined:\n{glsl}");
}

/// The stdlib's own PI is the motivating case (docs/shaders.md).
#[test]
fn stdlib_pi_is_usable_in_a_shader_fn() {
    let src = r#"
        @pure fn half_turn(x: float) -> float {
            return cos(x * PI)
        }
    "#;
    let glsl = emit(src, &["half_turn"]).expect("emit");
    assert!(glsl.contains("3.14159"), "PI not inlined:\n{glsl}");
}

/// Not `@pure`: bone rejects a mutable-global read in a `@pure` fn before the
/// GLSL profile runs, so purity would mask what this pins.
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

/// A `const` whose initializer is not a foldable literal must reject with a
/// message that names the real cause, not the generic "global read".
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
