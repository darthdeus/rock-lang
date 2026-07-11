// Structural tests: assertions on workspace/HIR/inference internals that a
// `.rock` fixture marker can't express (arena layouts, per-body bookkeeping,
// cross-module wiring). Type-level behavior belongs in tests/fixtures/*.rock
// instead.

use bone_semantic::prelude::*;
use bone_semantic::test_support::ExpectPretty;

/// Parse a source string into a raw (uncooked) skull `Ast`. Shims rock's old
/// `parse_rock`/`parse_rock_with_file` so call sites need no rewriting.
fn parse_rock(src: &str) -> Result<skull_parser::Ast, CompilerError> {
    skull_parser::parse_module(src, Ustr::from("<parse_rock>"))
}
#[allow(dead_code)]
fn parse_rock_with_file(src: &str, file: &str) -> Result<skull_parser::Ast, CompilerError> {
    skull_parser::parse_module(src, Ustr::from(file))
}

/// Parse + type-check one source as a standalone module (no stdlib) — the
/// `parse_rock(src)? -> semantic_pass(&mut [module])` path nearly every
/// structural test shares. Callers add their own `.unwrap()`/`.expect(..)`/
/// `.is_err()`. One place to touch when the front-end entry signature changes.
fn check(src: &str) -> Result<bone_semantic::inference::Workspace, CompilerError> {
    let mut module = [parse_rock(src)?];
    semantic_pass(&mut module)
}

/// Measurement harness, not a guard: runs `unified.rock` (or the file in
/// `BONE_STDLIB_PROBE`) through `semantic_pass` and panics with the first
/// strict-lowering wall. Run with `--ignored` to see the next stdlib gap.
#[test]
#[ignore]
fn probe_stdlib_first_wall() {
    let bundled = include_str!("../../../stdlib/unified.rock");
    let owned = std::env::var("BONE_STDLIB_PROBE")
        .ok()
        .map(|p| std::fs::read_to_string(p).expect("read BONE_STDLIB_PROBE"));
    let src: &str = owned.as_deref().unwrap_or(bundled);
    let srcs: &[(&str, &str)] = &[("unified.rock", src)];
    let module = parse_rock(src).expect_pretty("parse_rock", srcs);
    match semantic_pass(&mut [module]) {
        Ok(_) => println!("PROBE: stdlib loaded clean"),
        Err(e) => panic!(
            "PROBE first wall:\n{}",
            bone_semantic::test_support::render_err(srcs, &e)
        ),
    }
}

#[test]
#[ignore]
fn lower_and_check_add() {
    let src = "fn add(x: int, y: int) -> int { return x + y }";
    let srcs: &[(&str, &str)] = &[("<parse_rock>", src)];
    let module = parse_rock(src).expect_pretty("parse_rock", srcs);
    semantic_pass(&mut [module]).expect_pretty("semantic_pass", srcs);
}

#[test]
#[ignore]
fn check_add_mismatch_returns_error() {
    let src = "fn add(x: int, y: int) -> bool { return x + y }";
    let is_err = check(src).is_err();
    assert!(is_err);
}

// The per-body errors recorded for `fn main` in a self-contained source
// (semantic_pass returns Ok; type errors live per-body).
fn main_body_errors(src: &str) -> Vec<CompilerError> {
    let ws = check(src).expect("semantic_pass");
    let mid = ws
        .modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    let (main_idx, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "main")
        .expect("fn main exists");
    let inf = ws.inference(mid).expect("user module inferred");
    inf.errors
        .get(&BodyOwner::Function(main_idx))
        .cloned()
        .unwrap_or_default()
}

fn main_body_error_messages(src: &str) -> Vec<String> {
    main_body_errors(src).iter().map(|e| e.message.clone()).collect()
}

// P1a: a leaf type mismatch reads "expected `X`, found `Y`", not the bare
// jargon "Failed to unify Y and X" (docs/error-quality-plan.md).
#[test]
fn leaf_type_mismatch_says_expected_found() {
    let msgs = main_body_error_messages(r#"fn main() { let a: int = "hello" }"#);
    assert!(
        msgs.iter().any(|m| m.contains("expected `int`, found `string`")),
        "expected an expected/found message, got: {msgs:?}"
    );
}

// P1b: the mismatch carries a second label at the expected type's origin (the
// `: Foo` annotation), so the reader sees WHERE the expectation came from.
// Works for span-carrying types (nominal/compound); builtins (`int`) lower to a
// shared interned TyId with no per-site span -- documented gap in
// docs/error-quality-plan.md.
#[test]
fn type_mismatch_labels_the_expected_type_origin() {
    let src = "struct Foo { x: int }\nfn main() { let a: Foo = \"hello\" }";
    let err = main_body_errors(src)
        .into_iter()
        .find(|e| e.message.contains("expected `Foo`"))
        .expect("a type-mismatch error");
    let has_origin = err.labels.iter().any(|l| {
        let (lo, hi) = l.span.offset_range;
        src.get(lo..hi) == Some("Foo")
    });
    assert!(
        has_origin,
        "expected a label at the `Foo` annotation; labels: {:?}",
        err.labels.iter().map(|l| (l.span.offset_range, l.message.clone())).collect::<Vec<_>>()
    );
}

// `/` is float division (docs/float-division-plan.md); its `float` result in
// an int-shaped hole is rejected with a hint to use `./`. Self-contained (the
// float operators are declared inline) so it needs no stdlib.
#[test]
fn float_div_into_int_hole_hints_idiv() {
    let src = r#"
@pure @inline operator(lua) /(a: float, b: float) -> float = "return a / b"
fn foo(x: int) {}
fn main() { foo(3 / 4) }
"#;
    let msgs = main_body_error_messages(src);
    assert!(
        msgs.iter().any(|m| m.contains("./")),
        "expected a `./` hint, got: {msgs:?}"
    );
}

// The `./` hint also fires when the `/` is buried in the mismatched expression,
// reached through arithmetic operators (`__op_mul`) rather than being the
// top-level node (docs/float-division-plan.md Phase 4).
#[test]
fn float_div_deep_in_mul_subtree_hints_idiv() {
    let src = r#"
@pure @inline operator(lua) /(a: float, b: float) -> float = "return a / b"
@pure @inline operator(lua) *(a: float, b: float) -> float = "return a * b"
fn main() { let x: int = 2.0 * (3 / 4) }
"#;
    let msgs = main_body_error_messages(src);
    assert!(
        msgs.iter().any(|m| m.contains("./")),
        "expected a `./` hint for a `/` nested under `*`, got: {msgs:?}"
    );
}

// ...and through unary minus (`-(a / b)`), the other float-propagating shape.
#[test]
fn float_div_under_unary_minus_hints_idiv() {
    let src = r#"
@pure @inline operator(lua) /(a: float, b: float) -> float = "return a / b"
fn main() { let x: int = -(3 / 4) }
"#;
    let msgs = main_body_error_messages(src);
    assert!(
        msgs.iter().any(|m| m.contains("./")),
        "expected a `./` hint for a `/` under unary minus, got: {msgs:?}"
    );
}

// The hint's label points at the offending `/` subexpression, not the whole
// mismatched expression, so the caret lands on `(3 / 4)` rather than the outer
// `2.0 * (3 / 4)` (docs/float-division-plan.md Phase 4).
#[test]
fn float_div_hint_span_points_at_the_division() {
    let src = r#"
@pure @inline operator(lua) /(a: float, b: float) -> float = "return a / b"
@pure @inline operator(lua) *(a: float, b: float) -> float = "return a * b"
fn main() { let x: int = 2.0 * (3 / 4) }
"#;
    let ws = check(src).expect("semantic_pass");
    let mid = ws
        .modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    let (main_idx, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "main")
        .expect("fn main exists");
    let inf = ws.inference(mid).expect("user module inferred");
    let err = inf
        .errors
        .get(&BodyOwner::Function(main_idx))
        .and_then(|errs| errs.iter().find(|e| e.message.contains("./")))
        .expect("a `./` hint error");
    let (lo, hi) = err
        .labels
        .first()
        .expect("hint carries a label span")
        .span
        .offset_range;
    assert_eq!(&src[lo..hi], "(3 / 4)", "label should cover just the division");
}

// The walk follows a local binding back to its `let` initializer: a `float`
// that entered via `let h = a / b` and only mismatches an `int` later
// (`2 * h`) still points at the `/`. Regression for mag/projectiles.rock
// `let frame_h = dims.y / N ... p.dir * frame_h` into an int field.
#[test]
fn float_div_traced_through_let_binding_hints_idiv() {
    let src = r#"
@pure @inline operator(lua) /(a: float, b: float) -> float = "return a / b"
@pure @inline operator(lua) *(a: float, b: float) -> float = "return a * b"
fn main() {
    let h = 8 / 3
    let x: int = 2 * h
}
"#;
    let msgs = main_body_error_messages(src);
    assert!(
        msgs.iter().any(|m| m.contains("./")),
        "expected a `./` hint traced through the `let` binding, got: {msgs:?}"
    );
}

// A `/` flowing into a re-bindable numeric var (`min(1, ...)`, where `1`
// provisionally fixes `T=int` and the float arg widens `T` back to float) is
// NOT a mismatch -- coerce widens it. The hint must defer to that: no error at
// all. Regression for mag/particles.rock `min(1, (a/b)*1.6)` (a first cut fired
// the hint before coerce got to widen). docs/float-division-plan.md Phase 4.
#[test]
fn float_div_into_widening_numeric_var_is_not_an_error() {
    let src = r#"
@pure @inline operator(lua) /(a: float, b: float) -> float = "return a / b"
@pure @inline operator(lua) *(a: float, b: float) -> float = "return a * b"
fn min<T>(a: T, b: T) -> T { return a }
fn main() { let env = min(1, (3.0 / 4.0) * 1.6) }
"#;
    let msgs = main_body_error_messages(src);
    assert!(msgs.is_empty(), "expected no error (coerce widens), got: {msgs:?}");
}

// Hand-builds tuple nodes in a module arena (lowering doesn't emit them yet)
// to pin `render`'s Tuple arm. Elements live in the tuple's own arena, so
// `render` reattaches each child to the tuple's module id.
#[test]
fn render_tuple_flat() {
    use bone_semantic::hir::{BuiltinTy, Module, TyId, TypeExpr};
    let mut ws = Workspace::default();
    let mut m = Module {
        file: Ustr::from("test"),
        ..Default::default()
    };
    let a = m.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Int));
    let b = m.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Bool));
    let t = m.ty_arena.alloc(TypeExpr::Tuple {
        elements: IdxRange::new_inclusive(a..=b),
    });
    m.id = ws.intern_module(m.file);
    let mid = ws.insert_module(m);
    assert_eq!(ws.render(None, TyId::Static(mid, t)), "(int, bool)");
}

#[test]
fn render_tuple_nested() {
    use bone_semantic::hir::{BuiltinTy, Module, TyId, TypeExpr};
    let mut ws = Workspace::default();
    let mut m = Module {
        file: Ustr::from("test"),
        ..Default::default()
    };
    // Outer (int, <inner>) where inner = (int, bool). Each compound's
    // elements must be contiguous and in order, so the outer pair
    // (outer_a, inner) is allocated last and adjacently; inner's own
    // children come first.
    let inner_a = m.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Int));
    let inner_b = m.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Bool));
    let outer_a = m.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Int));
    let inner = m.ty_arena.alloc(TypeExpr::Tuple {
        elements: IdxRange::new_inclusive(inner_a..=inner_b),
    });
    let outer = m.ty_arena.alloc(TypeExpr::Tuple {
        elements: IdxRange::new_inclusive(outer_a..=inner),
    });
    m.id = ws.intern_module(m.file);
    let mid = ws.insert_module(m);
    assert_eq!(
        ws.render(None, TyId::Static(mid, outer)),
        "(int, (int, bool))"
    );
}

// Structural test (the bug is type-invisible, so a `.rock` type marker can't
// catch it): `let c`'s inner block defines `let b`, and lowering must not let
// that inner statement leak into the OUTER block's statement range via the
// `first..=last` contiguity trap. The outer block has exactly 2 statements
// (`let a`, `let c`); the tail `c` is separate.
#[test]
fn nested_block_does_not_leak_into_outer_statements() {
    use bone_semantic::hir::Expr;
    let src = "fn f() -> int {\n    let a = 1\n    let c = {\n        let b = 2\n        b\n    }\n    c\n}";
    let ws = check(src).unwrap();

    let mid = ws.modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    let hir = ws.module(mid);

    let (f_idx, _) = hir.functions.iter().next().expect("one function");
    let fbody = &hir.bodies[&BodyOwner::Function(f_idx)];
    let body = fbody.expr_arena[fbody.root.expect("rock function has a body")];
    let Expr::Block { statements, .. } = body else {
        panic!("function body should be a block, got {:?}", body);
    };

    assert_eq!(
        statements.len(),
        2,
        "inner block's `let b` leaked into the outer block's statement range"
    );
}

// Phase 0 of docs/bone-incremental-plan.md: a type error in one body must
// not abort checking of the others -- pass 4 collects errors per BodyOwner
// instead of fail-fasting through `?`. A watch/LSP session has to keep
// healthy bodies' inference warm while some bodies are broken.
#[test]
fn body_type_error_does_not_abort_other_bodies() {
    let src = "\
fn broken_one() -> int { return true }
fn good() -> int { return 3 }
fn broken_two() -> bool { return 42 }";
    let ws = check(src).expect("body-level type errors must not abort semantic_pass");

    let mid = ws.modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    let hir = ws.module(mid);

    let fn_owner = |name: &str| {
        let (idx, _) = hir
            .functions
            .iter()
            .find(|(_, f)| f.name.as_str() == name)
            .unwrap_or_else(|| panic!("fn {name} exists"));
        BodyOwner::Function(idx)
    };

    let inf = ws.inference(mid).expect("user module inferred");
    assert!(
        inf.errors.contains_key(&fn_owner("broken_one")),
        "broken_one must have a recorded error: {:?}",
        inf.errors
    );
    assert!(
        inf.errors.contains_key(&fn_owner("broken_two")),
        "broken_two must have a recorded error: {:?}",
        inf.errors
    );
    assert_eq!(
        inf.errors.len(),
        2,
        "only the two broken bodies should error: {:?}",
        inf.errors
    );
    assert!(
        inf.per_fn.contains_key(&fn_owner("good")),
        "healthy body must have inference even when sibling bodies are broken"
    );
}

// --- Return-type mismatch diagnostics: span + message quality. ---
//
// A fn whose body doesn't satisfy the declared return type must NOT underline
// the whole `fn { .. }` span. When the body has a tail expression, blame the
// tail; when it ends in a statement (no value at all), say so plainly instead
// of leaking the internal `()` type into "type mismatch: expected `string`, found `()`".

/// The source slice a byte-offset span covers.
fn text<'a>(src: &'a str, span: &Span) -> &'a str {
    let (s, e) = span.offset_range;
    &src[s..e]
}

/// All errors recorded against the named function's body.
fn recorded_errors<'a>(
    ws: &'a bone_semantic::inference::Workspace,
    fn_name: &str,
) -> &'a [CompilerError] {
    let mid = ws
        .modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    let hir = ws.module(mid);
    let (idx, _) = hir
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == fn_name)
        .unwrap_or_else(|| panic!("fn {fn_name} exists"));
    let inf = ws.inference(mid).expect("user module inferred");
    inf.errors
        .get(&BodyOwner::Function(idx))
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

#[test]
fn return_mismatch_underlines_tail_expr_not_whole_fn() {
    let src = "fn f() -> string {\n    let y = 1\n    y\n}";
    let ws = check(src).expect("recovered, not aborted");
    let errs = recorded_errors(&ws, "f");
    let err = errs.first().expect("a recorded return-type error");
    let span = err.labels.first().expect("labeled error").span;
    assert_eq!(
        text(src, &span),
        "y",
        "return mismatch must underline the tail expr `y`, not the whole fn"
    );
}

#[test]
fn missing_return_value_says_so_instead_of_unifying_unit() {
    let src = "fn f() -> string {\n    let y = 1\n    let z = 2\n}";
    let ws = check(src).expect("recovered, not aborted");
    let errs = recorded_errors(&ws, "f");
    let err = errs.first().expect("a recorded return-type error");
    assert!(
        err.message.contains("no return value"),
        "a statement-tailed body must report a missing return value, got: {:?}",
        err.message
    );
    assert!(
        !err.message.contains("()"),
        "the internal unit type must not leak into the message, got: {:?}",
        err.message
    );
    // ...anchored somewhere narrow, never the whole 4-line fn.
    let span = err.labels.first().expect("labeled error").span;
    assert!(
        text(src, &span).len() < src.len(),
        "must not underline the entire function body"
    );
}

#[test]
fn return_mismatch_does_not_mask_genuine_body_error() {
    let src = "fn f() -> string {\n    let w = undefined_symbol_here\n    let z = 1\n}";
    let ws = check(src).expect("recovered, not aborted");
    let errs = recorded_errors(&ws, "f");
    assert!(
        errs.iter().any(|e| e.message.contains("undefined_symbol_here")),
        "the real `Undefined global` error must survive, not be masked by the \
         return-type error; got: {:?}",
        errs.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

// Companion to body_type_error_does_not_abort_other_bodies for the
// non-function BodyOwners: check_module has a separate `?` loop per owner
// kind (decorators, globals, struct defaults), and the early global-init
// check in semantic_pass `?`s before pass 4 even runs. All of them must
// collect instead of aborting, or a broken global/default/decorator body
// still kills the healthy fn's inference.
#[test]
fn non_function_body_errors_do_not_abort_other_bodies() {
    let src = r#"struct __DecoTarget { name: string, params: [string], func: any }
struct Cfg { scale: int = "nope" }
global LIMIT: int = "too many"
deco init broken_deco {
    let s: int = target.name
}
fn good() -> int { return 3 }"#;
    let ws = check(src)
        .expect("global/struct-default/decorator body errors must not abort semantic_pass");

    let mid = ws.modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    let hir = ws.module(mid);

    let (good_idx, _) = hir
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "good")
        .expect("fn good exists");
    let (global_idx, _) = hir
        .globals
        .iter()
        .find(|(_, g)| g.name.as_str() == "LIMIT")
        .expect("global LIMIT exists");
    let (struct_idx, _) = hir
        .structs
        .iter()
        .find(|(_, s)| s.name.as_str() == "Cfg")
        .expect("struct Cfg exists");
    let (deco_idx, _) = hir
        .decorators
        .iter()
        .find(|(_, d)| d.name.as_str() == "broken_deco")
        .expect("deco broken_deco exists");

    let inf = ws.inference(mid).expect("user module inferred");
    for owner in [
        BodyOwner::Global(global_idx),
        BodyOwner::StructDefaults(struct_idx),
        BodyOwner::Decorator(deco_idx),
    ] {
        assert!(
            inf.errors.contains_key(&owner),
            "{owner:?} must have a recorded error: {:?}",
            inf.errors
        );
    }
    assert_eq!(
        inf.errors.len(),
        3,
        "exactly the three broken non-function bodies should error: {:?}",
        inf.errors
    );
    assert!(
        inf.per_fn.contains_key(&BodyOwner::Function(good_idx)),
        "healthy body must have inference even when non-function bodies are broken"
    );
}

// bone-inc phase 1 (docs/bone-incremental-plan.md): every item owns a Body
// -- checkable content or not -- and Module.bodies must cover every
// BodyOwner that pass 4 produces a result (or error) for. Since B7 the rule
// is uniform: "body exists <=> item exists"; owners with nothing to check
// (lua/extern fns, valueless globals, enums, newtypes, default-less structs)
// hold position-only bodies (node_index for hover), and check_* skip-guards
// decide checkability, not body existence.
#[test]
fn every_checked_body_owner_has_a_bodies_entry() {
    let src = r#"struct __DecoTarget { name: string, params: [string], func: any }
struct WithDefault { scale: int = 2 }
struct NoDefault { x: int }
global WITH_VALUE: int = 100
global NO_VALUE: int
enum Color { Red }
newtype Id(int)
inline(lua) fn lua_fn() -> int = "return 1"
deco init d {
    let n = target.name
}
fn rock_fn() -> int { return 3 }"#;
    let ws = check(src).unwrap();

    // superset rule, across every module in the workspace
    for (mid, inf) in ws.inferences_iter() {
        let hir = ws.module(mid);
        for owner in inf.per_fn.keys().chain(inf.errors.keys()) {
            assert!(
                hir.bodies.contains_key(owner),
                "{mid:?} {owner:?} was checked but has no Module.bodies entry"
            );
        }
    }

    // per-kind pins on the user module
    let mid = ws.modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    let hir = ws.module(mid);

    let fn_owner = |name: &str| {
        let (idx, _) = hir
            .functions
            .iter()
            .find(|(_, f)| f.name.as_str() == name)
            .unwrap_or_else(|| panic!("fn {name} exists"));
        BodyOwner::Function(idx)
    };
    let struct_owner = |name: &str| BodyOwner::StructDefaults(hir.struct_by_name[&name.into()]);

    let (deco_idx, _) = hir
        .decorators
        .iter()
        .find(|(_, d)| d.name.as_str() == "d")
        .expect("deco d exists");
    let (global_idx, _) = hir
        .globals
        .iter()
        .find(|(_, g)| g.name.as_str() == "WITH_VALUE")
        .expect("global WITH_VALUE exists");

    let (no_value_idx, _) = hir
        .globals
        .iter()
        .find(|(_, g)| g.name.as_str() == "NO_VALUE")
        .expect("global NO_VALUE exists");
    let (enum_idx, _) = hir
        .enums
        .iter()
        .find(|(_, e)| e.name.as_str() == "Color")
        .expect("enum Color exists");
    let (newtype_idx, _) = hir
        .newtypes
        .iter()
        .find(|(_, n)| n.name.as_str() == "Id")
        .expect("newtype Id exists");

    for owner in [
        fn_owner("rock_fn"),
        fn_owner("lua_fn"),
        BodyOwner::Decorator(deco_idx),
        BodyOwner::Global(global_idx),
        // position-only bodies: nothing checkable, but hover needs their
        // node_index, so the item still owns a Body
        BodyOwner::Global(no_value_idx),
        BodyOwner::Enum(enum_idx),
        BodyOwner::Newtype(newtype_idx),
        struct_owner("WithDefault"),
        struct_owner("NoDefault"),
    ] {
        assert!(
            hir.bodies.contains_key(&owner),
            "{owner:?} must have a bodies entry; got {:?}",
            hir.bodies.keys().collect::<Vec<_>>()
        );
    }

    // __DecoTarget also gets a StructDefaults body, so the count pins that
    // nothing beyond the rules above sneaks in. The injected operator
    // prelude (__op_add, ...) adds one Function body each -- count those
    // from the module rather than hardcoding the prelude's size.
    let prelude_fns = hir
        .functions
        .iter()
        .filter(|(_, f)| f.name.as_str().starts_with("__op_"))
        .count();
    assert_eq!(
        hir.bodies.len(),
        10 + prelude_fns,
        "{:?}",
        hir.bodies.keys().collect::<Vec<_>>()
    );
}

// bone-inc phase 1 step B2: scope storage is per-body. Module.scope_arena no
// longer exists (compiler-enforced), so what's left to pin is that lowering
// routes each owner's scopes into its OWN Body -- the nested fn's inner-block
// scope must not land in a sibling's arena. Inequalities, not exact counts:
// how many scopes a fn allocates (default/root/per-block) is lowering detail.
#[test]
fn scopes_land_in_their_own_body() {
    let src = "\
fn nested() -> int {
    let a = {
        let b = 2
        b
    }
    a
}
fn flat() -> int { return 1 }";
    let ws = check(src).unwrap();

    let mid = ws.modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    let hir = ws.module(mid);

    let scopes_of = |name: &str| {
        let (idx, _) = hir
            .functions
            .iter()
            .find(|(_, f)| f.name.as_str() == name)
            .unwrap_or_else(|| panic!("fn {name} exists"));
        hir.bodies[&BodyOwner::Function(idx)].scope_arena.len()
    };

    let flat = scopes_of("flat");
    let nested = scopes_of("nested");
    assert!(
        flat >= 2,
        "every fn body owns at least its default + root scopes, got {flat}"
    );
    assert!(
        nested > flat,
        "the inner block's scope must land in nested's own Body (nested {nested} vs flat {flat})"
    );
}

// Companion content pin for the non-function owners: an existing-but-EMPTY
// Body means a lowering entry point built its local Body and then inserted a
// fresh default instead (exactly the B4-era lower_global bug -- existence
// checks alone let it sit latent from B2 until an inference read tripped it).
// Each valued owner lowers at least one scope for its expression(s), so a
// non-empty scope_arena is the cheapest "the local actually got inserted" pin.
#[test]
fn non_function_bodies_carry_their_lowered_storage() {
    let src = r#"struct __DecoTarget { name: string, params: [string], func: any }
struct Cfg { scale: int = 2 }
global LIMIT: int = 100
deco init d {
    let n = target.name
}
fn f() -> int { return 3 }"#;
    let ws = check(src).unwrap();

    let mid = ws.modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    let hir = ws.module(mid);

    let (global_idx, _) = hir
        .globals
        .iter()
        .find(|(_, g)| g.name.as_str() == "LIMIT")
        .expect("global LIMIT exists");
    let (deco_idx, _) = hir
        .decorators
        .iter()
        .find(|(_, d)| d.name.as_str() == "d")
        .expect("deco d exists");
    let cfg_idx = hir.struct_by_name[&"Cfg".into()];

    for (what, owner) in [
        ("valued global", BodyOwner::Global(global_idx)),
        ("decorator", BodyOwner::Decorator(deco_idx)),
        ("struct with defaults", BodyOwner::StructDefaults(cfg_idx)),
    ] {
        assert!(
            !hir.bodies[&owner].scope_arena.is_empty(),
            "{what} ({owner:?}) has an empty Body -- its lowering entry point \
             built a local Body but inserted a default instead"
        );
    }
}

// The generation-staleness guard at the `query::type_at` chokepoint moved with
// the test that exercises it to `bone/tests/query_generation.rs`.

// The module ty_arena is interface-only: `TyId::Static` is what other
// modules' cached inference holds, so it may only ever contain
// signature/field/global types. An in-body `let x: P` annotation lowers to
// body-local storage (`Body.lowered_tys`, `TyId::Local`), so the module
// arena must be IDENTICAL whether or not a body is annotated. This is the
// property body-edit incremental updates stand on: a body edit cannot perturb
// interface storage (docs/bone-incremental-plan.md step C).
#[test]
fn body_annotations_do_not_touch_module_ty_arena() {
    let ty_arena_len = |src: &str| {
        let ws = check(src).unwrap();
        let mid = ws.modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
        ws.module(mid).ty_arena.len()
    };

    let plain = "\
struct P { a: int }
fn f() -> int {
    let x = P { a: 1 }
    return x.a
}";
    let annotated = "\
struct P { a: int }
fn f() -> int {
    let x: P = P { a: 1 }
    return x.a
}";
    assert_eq!(
        ty_arena_len(annotated),
        ty_arena_len(plain),
        "an in-body annotation leaked into the module (interface) ty_arena \
         -- some lower_type_expr path in body position still allocates \
         Static (check TyPos threading)"
    );
}

// The "inferred twice" guard in `assign_expr_type` must compare types
// STRUCTURALLY, not by `TyId` identity. Two `(int, bool)` tuples reified at
// different arena indices are distinct `TyId`s but the same type; flagging
// them as a conflict is a false positive. This is type-invisible (a `.rock`
// marker can't catch it -- an expr is inferred once in the real walk), so we
// drive `assign_expr_type` directly with two equal-but-distinct tuples.
#[test]
fn structurally_equal_compounds_reassign_is_ok() {
    use bone_semantic::hir::{BuiltinTy, Expr, Function, Module, TyId, TypeExpr};

    let mut ws = Workspace::default();
    let mut m = Module {
        file: Ustr::from("test"),
        ..Default::default()
    };

    let mut body = Body::default();
    let e = body.expr_arena.alloc(Expr::Int(0));
    m.functions.alloc(Function {
        span: Span::unknown(),
        name: Ustr::from("f"),
        name_span: Span::unknown(),
        is_coroutine: false,
        is_variadic: false,
        generics: vec![],
        generic_bounds: vec![],
        param_decls: vec![],
        num_required: 0,
        return_ty: None,
        lua_body: None,
        is_extern: false,
        annotations: vec![],
        ffi_lib: None,
    });

    m.id = ws.intern_module(m.file);
    let mid = ws.insert_module(m);

    let mut gamma = Gamma::new(&ws, &body, mid);

    // two structurally-equal `(int, bool)` tuples at distinct arena indices,
    // built in the gamma's OWN inference arena -- Gamma::new owns its `out`, so
    // TyIds minted in a throwaway BodyInference are unresolvable by `gamma.get`.
    let mk = |gamma: &mut Gamma| -> TyId {
        let a = gamma.out.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Int));
        let b = gamma.out.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Bool));
        TyId::Local(gamma.out.ty_arena.alloc(TypeExpr::Tuple {
            elements: IdxRange::new_inclusive(a..=b),
        }))
    };
    let t1 = mk(&mut gamma);
    let t2 = mk(&mut gamma);
    assert_ne!(t1, t2, "tuples must be distinct TyIds -- that's the point");

    // borrow the module from the workspace, not through gamma — the BodyCtx
    // must not hold gamma borrowed while assign_expr_type takes &mut gamma
    let c = BodyCtx {
        module: ws.module(mid),
        body: &body,
    };

    gamma.assign_expr_type(c, e, t1).expect("first assignment");
    gamma
        .assign_expr_type(c, e, t2)
        .expect("structurally-equal reassign must not error");
}

// Cross-module call resolution: a function call must resolve against OTHER
// modules in the workspace (the foundation for calling into a preloaded
// stdlib), not just the current file. Two modules, distinct ids; module B
// calls `helper` defined in module A. Single-source fixtures can't express
// this, so it's white-box.
#[test]
fn cross_module_call_resolves() {
    // Distinct file ids so the two modules don't collide in the workspace
    // (semantic_pass keys modules by `Ast.file`).
    let mod_a = parse_rock_with_file("fn helper(x: int) -> int { x }", "mod_a").unwrap();
    let mod_b = parse_rock_with_file("fn f() -> int { helper(5) }", "mod_b").unwrap();

    // `f`'s call to `helper` must resolve cross-module (into mod_a) and
    // `helper(5): int` must unify with f's declared `-> int`.
    semantic_pass(&mut [mod_a, mod_b]).expect("cross-module call should resolve + typecheck");
}

// Bug 2: a struct defined in one module, used as a type in another, must
// resolve cross-module. `lower_type_expr` resolves struct names against the
// current module only, so a `Point` param in mod_b can't see mod_a's struct.
#[test]
fn cross_module_struct_type_resolves() {
    let mod_a = parse_rock_with_file("struct Point { x: int, y: int }", "mod_a").unwrap();
    let mod_b = parse_rock_with_file("fn f(p: Point) -> int { 5 }", "mod_b").unwrap();

    semantic_pass(&mut [mod_a, mod_b]).expect("cross-module struct type should resolve");
}

// Bug 3: field access on a cross-module struct. Inference looks the struct up
// in the current module only, so `p.x` (Point from mod_a) can't find the def.
#[test]
fn cross_module_struct_field_resolves() {
    let mod_a = parse_rock_with_file("struct Point { x: int, y: int }", "mod_a").unwrap();
    let mod_b = parse_rock_with_file("fn f(p: Point) -> int { p.x }", "mod_b").unwrap();

    // p.x must resolve Point's field cross-module (into mod_a) -> int
    semantic_pass(&mut [mod_a, mod_b]).expect("cross-module struct field access should resolve");
}

// Bug 3 (literal path): constructing a struct defined in another module.
#[test]
fn cross_module_struct_literal_resolves() {
    let mod_a = parse_rock_with_file("struct Point { x: int, y: int }", "mod_a").unwrap();
    let mod_b =
        parse_rock_with_file("fn f() -> Point { Point { x: 1, y: 2 } }", "mod_b").unwrap();

    semantic_pass(&mut [mod_a, mod_b]).expect("cross-module struct literal should resolve");
}

/// Sentinel: `TyId` must stay niche-packed at 8 bytes. The packing depends on
/// `ModuleId` carrying a niche (`NonMaxU32`) so rustc folds the enum tag in. A
/// future change that drops the niche (e.g. `ModuleId(u32)`) silently
/// re-inflates `TyId` to 12 bytes and reverts the S5 memory-traffic shrink with
/// no other symptom -- this goes red the moment that happens.
#[test]
fn tyid_stays_niche_packed() {
    use std::mem::size_of;
    assert_eq!(size_of::<ModuleId>(), 4, "ModuleId must stay 4 bytes (NonMaxU32)");
    assert_eq!(
        size_of::<TyId>(),
        8,
        "TyId must stay niche-packed at 8 bytes; a non-niche ModuleId field re-inflates it to 12"
    );
}


// Lowered holes and fresh inference metavars must never share a `VarId`.
// Lowering mints hole `Infer` ids from a module-scoped counter (`lower.rs`
// `_next_var_id`); inference seeds `Gamma::new` `next_var_id` from
// `Module.lower_var_high_water` so every fresh metavar starts above all lowered
// ids. If the two spaces overlap, a function's Static param/return holes and its
// Local fresh vars collide on one `substitutions` key -- two different vars,
// one binding (unsound). This guards the seed: a RED here means `next_var_id`
// stopped clearing the lowered high-water.
#[test]
fn fresh_metavar_ids_never_collide_with_lowered_holes() {
    let src = "\
fn id<T>(x: T) -> T { return x }
fn pair<A, B>(a: A, b: B) -> A { return a }
fn holey(x: _) -> _ {
    let a: _ = id(1)
    let b: _ = pair(2, id(3))
    let c: _ = id(id(4))
    return x
}";
    let ws = check(src).expect("crafted holes+generics module type-checks");

    let mut collisions: Vec<(ModuleId, String, u32)> = Vec::new();
    for (mid, inf) in ws.inferences_iter() {
        // lower-minted hole ids live in the module's Static ty_arena
        let static_holes: FxHashSet<hir::VarId> = ws
            .module(mid)
            .ty_arena
            .iter()
            .filter_map(|(_, t)| match t {
                hir::TypeExpr::Infer(v, _) => Some(*v),
                _ => None,
            })
            .collect();

        // a body's Local ty_arena holds its fresh metavars (+ cloned local holes)
        for (owner, body) in &inf.per_fn {
            for (_, t) in body.ty_arena.iter() {
                if let hir::TypeExpr::Infer(v, _) = t {
                    if static_holes.contains(v) {
                        collisions.push((mid, format!("{owner:?}"), v.0));
                    }
                }
            }
        }
    }

    assert!(
        collisions.is_empty(),
        "fresh metavar / lowered-hole VarId collision -- `Gamma::new` `next_var_id` \
         must seed from `Module.lower_var_high_water`: {collisions:?}"
    );
}

// ─── @pure verification (RED until bone grows a purity pass) ─────────────────
//
// rock rejects all of these via its `@pure` verifier; bone is a strict port, so
// it must too. They are `backend-diff tests/errors` LEADs today (rock rejects,
// bone accepts) -- ported from `tests/errors/pure_verification.rock.err`. Each
// assertion FAILS now (genuine RED -- verified bone accepts each) and goes green
// when the author lands purity verification.
//
// SCOPE: only the 7 genuine-side-effect cases are pinned here. rock's verifier
// ALSO rejects 6 cases its own fixture calls "accepted limitations" of the scope
// walker (match-with-binding, self-recursion, returning a lambda, heap
// struct/array literal, param collection-index read). Those are pure in
// principle, so per "bone implements the intended rule where rock's is an
// accident" they are design decisions for the author -- match rock's
// conservatism or implement the precise rule -- NOT pinned as required
// rejections here.

/// True iff bone records ANY rejection for `src`: a top-level error (parse /
/// semantic_pass / annotation registration), or a per-body inference error on a
/// user module. The `@pure` programs below are otherwise well-typed, so the only
/// thing that flips this to `true` is a purity pass rejecting the body.
fn bone_rejects(src: &str) -> bool {
    let ws = match check(src) {
        Ok(ws) => ws,
        Err(_) => return true,
    };
    // "Rejected" = any error diagnostic. Lowering is total (it collects errors
    // and returns Ok rather than fail-fasting), so an interface / lowering error
    // is a rejection just as much as an inference error or a hard `Err`.
    ws.modules.iter().any(|m| {
        m.id != ModuleId::builtin()
            && (!m.interface_errors.is_empty()
                || !m.lowering_errors.is_empty()
                || ws.inference(m.id).map_or(false, |inf| !inf.errors.is_empty()))
    })
}

#[test]
fn pure_reads_mutable_global_rejected() {
    // Reads a mutable global -- not referentially transparent.
    assert!(
        bone_rejects("global G: int = 0\n@pure\nfn f() -> int { return G }"),
        "@pure fn reading a mutable global must be rejected"
    );
}

#[test]
fn pure_writes_global_rejected() {
    // Assigns to a mutable global -- a mutation side effect.
    assert!(
        bone_rejects("global G: int = 0\n@pure\nfn f() -> int { G = 5  return 0 }"),
        "@pure fn writing a global must be rejected"
    );
}

// (Compound-assign `G += 1` is the same purity violation as the plain
// `pure_writes_global` case -- a global mutation -- but `+=` desugars through
// `+`, which this no-stdlib harness can't resolve, so it can't be pinned here
// without false-greening on the operator. The purity pass handles all
// assignment forms uniformly; writing `=` covers the class.)

#[test]
fn pure_calls_non_pure_rejected() {
    // Calls a fn whose body is not provably pure -> transitively impure.
    // (operator-free: `TICK = 9` not `TICK + 1`, so the no-stdlib harness
    // doesn't reject for an unresolved `+` instead of for purity.)
    let src = "\
global TICK: int = 0
fn bump() -> int { TICK = 9  return TICK }
@pure
fn f() -> int { return bump() }";
    assert!(
        bone_rejects(src),
        "@pure fn calling a non-pure fn must be rejected"
    );
}

#[test]
fn pure_mutates_param_field_rejected() {
    // Mutation through a parameter -- observable at the call site.
    let src = "\
struct P { x: int, y: int }
@pure
fn f(p: P) -> int { p.x = 99  return p.x }";
    assert!(
        bone_rejects(src),
        "@pure fn mutating a param field must be rejected"
    );
}

#[test]
fn pure_while_loop_with_mutation_rejected() {
    // Purity must walk into loop bodies. (operator-free: `while true { C = 5 }`
    // so the no-stdlib harness rejects for the global write, not an unresolved
    // operator in the condition/increment.)
    let src = "\
global C: int = 0
@pure
fn f() -> int { while true { C = 5 }  return C }";
    assert!(
        bone_rejects(src),
        "@pure fn whose loop body writes a global must be rejected"
    );
}

#[test]
fn pure_and_debug_impure_conflict_rejected() {
    // `@pure` (no side effects) and `@debug_impure` (debug-only side effect) on
    // one fn is a contradiction -- rock rejects at annotation registration,
    // before the purity walk. (May surface as a top-level error, not a body
    // error -- bone_rejects covers both.)
    assert!(
        bone_rejects("@pure\n@debug_impure\nfn f() -> int { return 1 }"),
        "@pure + @debug_impure on one fn must be rejected as contradictory"
    );
}

// ─── Duplicate-symbol detection (RED until bone rejects redefinitions) ───────
//
// rock rejects redefining a type name (enum/struct/newtype), within a module
// and across modules. bone accepts today (backend-diff tests/errors LEADs),
// which is a soundness hole: a duplicate definition makes "which def wins"
// ambiguous. Ported from tests/errors/symbols/duplicate_symbols.rock.err.
// RED now (verified bone accepts each); green when the author lands a
// duplicate-definition check at symbol collection. Single-module dups surface
// as a top-level semantic_pass error; `bone_rejects` (check() Err) covers that.

#[test]
fn duplicate_enum_rejected() {
    assert!(
        bone_rejects("enum E { A }\nenum E { B }"),
        "redefining an enum must be rejected"
    );
}

#[test]
fn duplicate_struct_rejected() {
    assert!(
        bone_rejects("struct S { x: int }\nstruct S { y: int }"),
        "redefining a struct must be rejected"
    );
}

#[test]
fn duplicate_newtype_rejected() {
    assert!(
        bone_rejects("newtype N(int)\nnewtype N(string)"),
        "redefining a newtype must be rejected"
    );
}

// (The cross-module case -- `struct Foo` in two files -- is NOT pinned here: a
// real 2-module workspace needs the intern/insert dance the single-shot
// `check()` harness doesn't do, and a naive `semantic_pass(&mut [a, b])`
// false-greens on a spurious collision rather than a real duplicate-symbol
// diagnostic. `backend-diff tests/errors` covers that case as the live oracle.)

// ─── ![T?] is unsound: NativeArray of nullable T (RED) ───────────────────────
//
// `![T?]` is a bare Lua table; an embedded `null` makes `#arr` / `ipairs` stop
// at the first nil, silently truncating iteration -- use `[T?]` (Vec, tracked
// `.len`) instead. rock rejects the type; bone accepts today (LEAD). The bad
// type alone triggers it -- no literal/loop needed (keeps the no-stdlib harness
// from false-greening on `print`/`for`/interpolation). RED now; green when bone
// rejects a nullable NativeArray element type.

#[test]
fn native_array_of_nullable_rejected() {
    assert!(
        bone_rejects("fn f(xs: ![int?]) -> int { return 0 }"),
        "a NativeArray element type may not be nullable (![T?] is unsound)"
    );
}

// ─── Control-flow placement (RED until bone validates it) ────────────────────
//
// `break`/`continue` are only meaningful inside a loop; `return` only in
// statement position (not a sub-expression). rock rejects these; bone accepts
// (backend-diff LEADs). Statement-position forms below isolate the placement
// rule from the value-position-if branch-type join (which bone already checks
// and would false-green on). Ported from tests/errors/control_flow/.

#[test]
fn continue_outside_loop_rejected() {
    assert!(
        bone_rejects("fn f() { continue }"),
        "`continue` with no enclosing loop must be rejected"
    );
}

#[test]
fn break_outside_loop_rejected() {
    assert!(
        bone_rejects("fn f() { break }"),
        "`break` with no enclosing loop must be rejected"
    );
}

#[test]
fn return_in_subexpression_rejected() {
    // `return` inside a tuple/multi-assign RHS sub-expression.
    assert!(
        bone_rejects("fn f() { let (x, y) = (1, if true { return } else { 2 }) }"),
        "`return` in a sub-expression position must be rejected"
    );
}

// ─── yield outside a coroutine (RED) ─────────────────────────────────────────
//
// A `yield` in a plain lambda has no enclosing coroutine to suspend -- the
// lambda is its own non-`co` fn. rock rejects; bone propagates the enclosing
// `co fn`'s yield type into the lambda and accepts (LEAD). One case pins the
// rule (bare `() => yield` is the same violation). Ported from
// tests/errors/yield_in_plain_lambda.rock.err.

#[test]
fn yield_in_plain_lambda_rejected() {
    assert!(
        bone_rejects("co fn g() -> Coroutine<int> { let f = () => yield 5  yield 0 }"),
        "`yield` inside a non-coroutine lambda must be rejected"
    );
}

// ─── Newtype destructuring vs scrutinee type (RED) ───────────────────────────
//
// A newtype pattern `Uid(x)` may only destructure a value of that newtype. rock
// rejects destructuring a struct or a bare int with a newtype pattern; bone
// checks the pattern's ARITY (already rejects too-many/too-few) but not that the
// pattern's newtype matches the scrutinee type (LEAD). Ported from
// tests/errors/newtypes/destructuring_errors.rock.err.

#[test]
fn newtype_pattern_on_struct_rejected() {
    let src = "\
newtype Uid(int)
struct U { id: int }
fn f(u: U) -> int { let Uid(x) = u  return x }";
    assert!(
        bone_rejects(src),
        "destructuring a struct with a newtype pattern must be rejected"
    );
}

#[test]
fn newtype_pattern_on_int_rejected() {
    let src = "\
newtype Uid(int)
fn f() -> int { let x = 42  let Uid(y) = x  return y }";
    assert!(
        bone_rejects(src),
        "destructuring a bare int with a newtype pattern must be rejected"
    );
}

// ─── Duplicate match patterns (RED) ──────────────────────────────────────────
//
// A second wildcard `_` arm is dead -- rock rejects it; bone accepts (LEAD). One
// case pins the rule (3+ wildcards is the same violation). Ported from
// tests/errors/patterns/duplicate_patterns.rock.err.

#[test]
fn duplicate_wildcard_arm_rejected() {
    let src = "fn f() -> int { let t = 1  return match t { 0 => 10  _ => 20  _ => 30 } }";
    assert!(
        bone_rejects(src),
        "a duplicate wildcard `_` match arm must be rejected"
    );
}

// ─── Generic arity in parameter type position (RED) ──────────────────────────
//
// A generic type used without its type args (`x: G` for `struct G<T>`) is
// rejected by rock. bone catches this in `let` annotations already but misses it
// in function PARAMETER positions (LEAD). Ported from
// tests/errors/generics/generic_validation.rock.err
// (function_param_missing_type_args).

#[test]
fn generic_param_missing_type_args_rejected() {
    let src = "\
struct G<T> { v: T }
fn f(x: G) -> int { return 0 }";
    assert!(
        bone_rejects(src),
        "a generic type used without type args in a param position must be rejected"
    );
}

// ─── @pure identity-pure split (decided: identity-pure, not value-pure) ───────
//
// bone's `@pure` verifier rejects constructs that break dup-safety (observable
// heap identity / reads of mutable state) but ACCEPTS constructs that are
// referentially transparent even though rock's scope-walker rejects them as a
// limitation. Reject: heap struct literal, array literal, lambda, collection
// index. Accept (divergence from rock, by design): match-with-binding and
// self-recursion. Ported from tests/errors/pure_verification.rock.err.

#[test]
fn pure_heap_struct_literal_rejected() {
    // Non-`value` struct literal allocates a fresh heap object (identity).
    let src = "\
struct S { n: int }
@pure
fn f() -> S { return S { n: 0 } }";
    assert!(bone_rejects(src), "@pure returning a heap struct literal must be rejected");
}

#[test]
fn pure_array_literal_rejected() {
    let src = "\
@pure
fn f() -> [int] { return [0] }";
    assert!(bone_rejects(src), "@pure building an array literal must be rejected");
}

#[test]
fn pure_lambda_rejected() {
    let src = "\
@pure
fn f() -> (int) -> int { return x => x }";
    assert!(bone_rejects(src), "@pure creating a lambda must be rejected");
}

#[test]
fn pure_collection_index_rejected() {
    let src = "\
@pure
fn f(v: [int]) -> int { return v[0] }";
    assert!(bone_rejects(src), "@pure indexing a collection must be rejected");
}

#[test]
fn pure_match_with_binding_accepted() {
    // DIVERGENCE from rock: a match with a binding pattern is referentially
    // transparent. rock rejects it as a scope-walker limitation; bone accepts.
    let src = "\
enum E { A(int) }
@pure
fn f(e: E) -> int { return match e { A(x) => x } }";
    assert!(!bone_rejects(src), "@pure with a match-binding is pure -- bone accepts (diverges from rock)");
}

#[test]
fn pure_self_recursion_accepted() {
    // DIVERGENCE from rock: a @pure fn calling itself is pure (the callee is
    // @pure). rock rejects self-recursion without fixpoint analysis; bone accepts.
    let src = "\
@pure
fn f(n: int) -> int { return f(n) }";
    assert!(!bone_rejects(src), "@pure self-recursion is pure -- bone accepts (diverges from rock)");
}

#[test]
fn value_annotation_on_struct_rejected() {
    // `@value struct` is retired; only the `value Foo { ... }` keyword is
    // canonical. Ported from tests/errors/types/value_annotation_rejected.rock.err.
    assert!(
        bone_rejects("@value\nstruct Foo { x: int }"),
        "the retired `@value` annotation form must be rejected"
    );
    // The canonical keyword form is still accepted.
    assert!(
        !bone_rejects("value Foo { x: int }"),
        "the `value` keyword form must still be accepted"
    );
}

#[test]
fn null_to_bare_any_rejected() {
    // Bare `any` excludes null; the null literal needs `any?`. A nullable VALUE
    // (concrete inner) is still allowed -- only the literal is rejected (rock
    // parity). Ported from tests/errors/types/nullable_assignment.rock.err.
    assert!(
        bone_rejects("fn f() { let x: any = null }"),
        "null literal assigned to bare `any` must be rejected"
    );
    assert!(
        !bone_rejects("fn f() { let x: any? = null }"),
        "null assigned to `any?` must be accepted"
    );
    assert!(
        !bone_rejects("fn f() { let y: int? = 5  let x: any = y }"),
        "a nullable VALUE assigned to `any` is allowed (rock parity)"
    );
}

// NOTE: int-division-to-float (the check_expr `__op_div` guard) can't be pinned
// in this no-stdlib harness -- the `/` operator (`__op_div`) lives in stdlib, so
// any `a / b` here fails to resolve and false-reds. Oracle for that rule:
// `rock backend-diff tests/errors/types/int_division_promotion.rock.err` (0
// LEADs) + the full tests/ sweep under bone.

#[test]
fn redundant_null_coalesce_rejected() {
    // `??` on a non-nullable LHS is dead. Ported from
    // tests/errors/types/null_checks.rock.err.
    assert!(
        bone_rejects("fn f() { let x: int = 5  let y = x ?? 0 }"),
        "`??` on a non-nullable value must be rejected"
    );
    assert!(
        !bone_rejects("fn f() { let x: int? = 5  let y = x ?? 0 }"),
        "`??` on a nullable value is meaningful and accepted"
    );
}

// Collect every body-level error message across the user module. Used by the
// diagnostic-quality tests below, which assert on the rendered wording rather
// than just `is_err`.
fn all_err_msgs(src: &str) -> Vec<String> {
    let ws = check(src).expect("body-level errors are collected, not fatal");
    let mut msgs = Vec::new();
    for m in ws.modules.iter() {
        if m.id == ModuleId::builtin() {
            continue;
        }
        if let Some(inf) = ws.inference(m.id) {
            for errs in inf.errors.values() {
                for e in errs {
                    msgs.push(e.message.clone());
                }
            }
        }
    }
    msgs
}

// A plain nominal mismatch (no generics on either side) must not be reported
// as "different generic constructors" -- it must name both types plainly.
#[test]
fn nominal_mismatch_message_names_both_types() {
    let src = "\
struct Mob { x: int }
struct Data { y: int }
fn take(m: Mob) -> int { return m.x }
fn caller(d: Data) -> int { return take(d) }";
    let msgs = all_err_msgs(src);
    let joined = msgs.join(" | ");
    assert!(
        msgs.iter().any(|m| m.contains("Mob") && m.contains("Data")),
        "mismatch message should name both `Mob` and `Data`: {joined}"
    );
    assert!(
        !joined.contains("generic constructors"),
        "non-generic nominal mismatch must not say 'generic constructors': {joined}"
    );
}

// `recv.field()` where `field` is a non-callable field and a same-named free
// fn exists (so UFCS is attempted and fails on the receiver type) -- the error
// must point at the field/`()` slip, not a bare unify failure.
#[test]
fn field_called_as_method_with_shadowing_free_fn() {
    let src = "\
struct Data { cooldown: float = 1.5 }
struct Thing { id: int }
fn make() -> Data { return Data { cooldown: 2.0 } }
fn cooldown(t: Thing) -> float { return 9.0 }
fn caller() -> float { return make().cooldown() }";
    let joined = all_err_msgs(src).join(" | ");
    assert!(
        joined.contains("cooldown") && joined.contains("field") && joined.contains("()"),
        "should flag `cooldown` as a field called with `()`: {joined}"
    );
}

// Same slip, but no same-named free fn at all -- must NOT say "Function
// doesn't exist" (the field plainly exists); it must flag the field.
#[test]
fn field_called_as_method_no_free_fn() {
    let src = "\
struct Data { cooldown: float = 1.5 }
fn make() -> Data { return Data { cooldown: 2.0 } }
fn caller() -> float { return make().cooldown() }";
    let joined = all_err_msgs(src).join(" | ");
    assert!(
        !joined.contains("doesn't exist"),
        "field exists -- must not claim the function doesn't exist: {joined}"
    );
    assert!(
        joined.contains("cooldown") && joined.contains("field"),
        "should flag `cooldown` as a field: {joined}"
    );
}

#[test]
fn redundant_safe_nav_field_rejected() {
    // `?.` on a non-nullable receiver is dead.
    assert!(
        bone_rejects("struct Bar { val: int }\nfn f(b: Bar) -> int { return b?.val }"),
        "`?.` on a non-nullable receiver must be rejected"
    );
    assert!(
        !bone_rejects("struct Bar { val: int }\nfn f(b: Bar?) -> int? { return b?.val }"),
        "`?.` on a nullable receiver is meaningful and accepted"
    );
}

/// Unfuck #32: when every overload is rejected and the only near-miss is an
/// `Iterator`-bounded candidate that failed CONFORMANCE (params fit, bounds
/// did not), the error surfaces `check_iterator_bounds`' precise reason
/// appended to the "No overload for ..." headline (`select_overload`'s
/// `bound_fail` slot, filled by `score_candidate`).
#[test]
fn all_rejected_overload_error_surfaces_bound_conformance_reason() {
    let src = r#"
struct Multi { i: int }
fn next(it: Multi) -> (bool, int, int) { return (false, 0, 0) }
fn wrap<I: Iterator<T>, T>(x: I, n: int) -> int { return n }
fn wrap(x: string, n: int) -> int { return n }
fn main() {
    let m = Multi { i: 0 }
    let _ = wrap(m, 1)
}
"#;
    let ws = check(src).expect("body errors are collected, not aborted");
    let mid = ws.modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    let inf = ws.inference(mid).expect("user module inferred");
    let msgs: Vec<&str> = inf
        .errors
        .values()
        .flatten()
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        msgs.iter().any(|m| m.contains("No overload for `wrap`")),
        "the call must reject: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("does not satisfy")),
        "the bound-conformance rejection reason must be surfaced: {msgs:?}"
    );
}
