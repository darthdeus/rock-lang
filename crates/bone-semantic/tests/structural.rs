// Structural tests: assertions on workspace/HIR/inference internals that a
// `.rock` fixture marker can't express (arena layouts, per-body bookkeeping,
// cross-module wiring). Type-level behavior belongs in tests/fixtures/*.rock
// instead.

use bone_semantic::prelude::*;

/// Parse a source string into a raw (uncooked) skull `Ast`. Shims rock's old
/// `parse_rock`/`parse_rock_with_file` so call sites need no rewriting.
fn parse_rock(src: &str) -> Result<skull_parser::Ast, CompilerError> {
    skull_parser::parse_module_clean(src, Ustr::from("<parse_rock>"))
}
fn parse_rock_with_file(src: &str, file: &str) -> Result<skull_parser::Ast, CompilerError> {
    skull_parser::parse_module_clean(src, Ustr::from(file))
}

/// Parse + type-check one source as a standalone module (no stdlib) — the
/// `parse_rock(src)? -> semantic_pass(&mut [module])` path nearly every
/// structural test shares. Callers add their own `.unwrap()`/`.expect(..)`/
/// `.is_err()`. One place to touch when the front-end entry signature changes.
fn check(src: &str) -> Result<bone_semantic::inference::Workspace, CompilerError> {
    let mut module = [parse_rock(src)?];
    bone_semantic::semantic_passes::semantic_pass_enforcing_identity_storage(&mut module)
}

#[test]
fn legacy_only_workspace_keeps_modules_v1_namespace_empty() {
    use bone_core::modules::{NsAliases, PackageAliases};
    use skull_parser::{Dialect, parse_module_with_dialect};

    let mut module = parse_module_with_dialect(
        "fn main() { print(1) }",
        Ustr::from("main.rock"),
        Dialect::Legacy,
    )
    .expect("parse legacy module");
    module.package = Some(rock_core::source_code::PackageTag {
        id: 1,
        self_name: "game".to_string(),
        identity_storage: Default::default(),
        root: String::new(),
    });
    let aliases = NsAliases {
        packages: vec![PackageAliases {
            id: 1,
            self_name: Ustr::from("game"),
            aliases: vec![],
        }],
    };

    let ws = semantic_pass_with_aliases(&mut [module], &aliases).expect("semantic pass");
    assert!(ws.ns.packages.is_empty());
    assert!(ws.fragment_scopes.is_empty());
}

#[test]
fn mixed_workspace_rejects_legacy_use_at_the_use_span() {
    use bone_core::modules::{NsAliases, PackageAliases};
    use skull_parser::{Dialect, parse_module_with_dialect};

    let mut provider = parse_module_with_dialect(
        "pub struct Provider {}",
        Ustr::from("provider.rock"),
        Dialect::V1,
    )
    .expect("parse v1 provider");
    provider.package = Some(rock_core::source_code::PackageTag {
        id: 1,
        self_name: "provider".to_string(),
        identity_storage: Default::default(),
        root: String::new(),
    });

    let legacy_src = "use provider::Provider;\nfn main() {}";
    let legacy = parse_module_with_dialect(legacy_src, Ustr::from("legacy.rock"), Dialect::Legacy)
        .expect("parse legacy consumer");

    let aliases = NsAliases {
        packages: vec![PackageAliases {
            id: 1,
            self_name: Ustr::from("provider"),
            aliases: vec![],
        }],
    };
    let err = semantic_pass_with_aliases(&mut [provider, legacy], &aliases)
        .expect_err("a legacy use must be rejected");

    assert_eq!(
        err.message,
        "item-level `use` requires modules v1 (`[language] modules = \"v1\"`)"
    );
    let label = err.labels.first().expect("the rejected use is labeled");
    assert_eq!(label.span.file, Ustr::from("legacy.rock"));
    let covered = &legacy_src[label.span.offset_range().0..label.span.offset_range().1];
    assert!(
        covered.starts_with("use provider::Provider"),
        "labeled span: {covered:?}"
    );
}

#[test]
fn an_unresolved_v1_variant_pattern_is_labeled_at_its_path() {
    use bone_core::modules::{NsAliases, PackageAliases};
    use skull_parser::{Dialect, parse_module_with_dialect};

    let src =
        "fn f(n: int) -> int {\n    match n {\n        Nope::A => 1,\n        _ => 0,\n    }\n}";
    let mut module = parse_module_with_dialect(src, Ustr::from("app.rock"), Dialect::V1)
        .expect("parse v1 module");
    module.package = Some(rock_core::source_code::PackageTag {
        id: 1,
        self_name: "app".to_string(),
        identity_storage: Default::default(),
        root: String::new(),
    });
    let aliases = NsAliases {
        packages: vec![PackageAliases {
            id: 1,
            self_name: Ustr::from("app"),
            aliases: vec![],
        }],
    };

    let ws = semantic_pass_with_aliases(&mut [module], &aliases).expect("semantic pass");
    let err = ws
        .modules
        .iter()
        .flat_map(|m| m.lowering_errors.values().flatten())
        .find(|e| e.message.contains("did not resolve"))
        .expect("the unbound pattern prefix is a lowering error");
    let label = err
        .labels
        .first()
        .expect("the unresolved pattern is labeled");
    assert_eq!(label.span.file, Ustr::from("app.rock"));
    let covered = &src[label.span.offset_range().0..label.span.offset_range().1];
    assert_eq!(covered, "Nope::A");
}

#[test]
fn legacy_only_workspace_keeps_the_std_namespace() {
    use skull_parser::Dialect;

    let std_module = skull_parser::parse_module_with_dialect(
        "fn print() {}",
        Ustr::from("<stdlib>/fixture.rock"),
        Dialect::Legacy,
    )
    .expect("parse stdlib module");
    let mut user_module = skull_parser::parse_module_with_dialect(
        "fn main() {}",
        Ustr::from("main.rock"),
        Dialect::Legacy,
    )
    .expect("parse legacy module");
    user_module.package = Some(rock_core::source_code::PackageTag {
        id: 1,
        self_name: "game".to_string(),
        identity_storage: Default::default(),
        root: String::new(),
    });

    let ws = semantic_pass(&mut [std_module, user_module]).expect("semantic pass");
    let std_root = ws
        .ns
        .package(u32::MAX)
        .and_then(|package| package.node(Ustr::from("")))
        .expect("stdlib namespace root");
    assert!(std_root.owned_callables.contains_key(&Ustr::from("print")));
    assert!(ws.fragment_scopes.is_empty());
}

// The per-body errors recorded for `fn main` in a self-contained source
// (semantic_pass returns Ok; type errors live per-body).
fn main_body_errors(src: &str) -> Vec<CompilerError> {
    body_errors(src, "main")
}

fn body_errors(src: &str, name: &str) -> Vec<CompilerError> {
    let ws = check(src).expect("semantic_pass");
    let mid = ws.source_modules().next().expect("one user module").id;
    let (fn_idx, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .expect("fn exists");
    let inf = ws.inference(mid).expect("user module inferred");
    inf.errors
        .get(&BodyOwner::Function(fn_idx))
        .cloned()
        .unwrap_or_default()
}

// P1b: the mismatch carries a second label at the expected type's origin (the
// `: Foo` annotation), so the reader sees WHERE the expectation came from.
// Works for span-carrying types (nominal/compound); builtins (`int`) lower to a
// shared interned TyId with no per-site span.
#[test]
fn type_mismatch_labels_the_expected_type_origin() {
    let src = "struct Foo { x: int }\nfn main() { let a: Foo = \"hello\" }";
    let err = main_body_errors(src)
        .into_iter()
        .find(|e| e.message.contains("expected `Foo`"))
        .expect("a type-mismatch error");
    let has_origin = err.labels.iter().any(|l| {
        let (lo, hi) = l.span.offset_range();
        src.get(lo..hi) == Some("Foo")
    });
    assert!(
        has_origin,
        "expected a label at the `Foo` annotation; labels: {:?}",
        err.labels
            .iter()
            .map(|l| (l.span.offset_range(), l.message.clone()))
            .collect::<Vec<_>>()
    );
}

// The hint's label points at the offending `/` subexpression, not the whole
// mismatched expression, so the caret lands on `(3 / 4)` rather than the outer
// `2.0 * (3 / 4)`.
#[test]
fn float_div_hint_span_points_at_the_division() {
    let src = r#"
@pure @inline operator(lua) /(a: float, b: float) -> float = "return a / b"
@pure @inline operator(lua) *(a: float, b: float) -> float = "return a * b"
fn main() { let x: int = 2.0 * (3 / 4) }
"#;
    let ws = check(src).expect("semantic_pass");
    let mid = ws.source_modules().next().expect("one user module").id;
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
        .offset_range();
    assert_eq!(
        &src[lo..hi],
        "(3 / 4)",
        "label should cover just the division"
    );
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

    let mid = ws.source_modules().next().expect("one user module").id;
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

// The same contiguity trap for parameters: the lambda default's own `x`
// binding is allocated between `f` and `k`, and must stay out of the fn's
// parameter range. With the lambda default last it never could.
#[test]
fn lambda_param_default_does_not_leak_into_fn_params() {
    for (src, names) in [
        (
            "fn f(v: int, g: fn(int) -> int = x => x, k: int = 1) -> int { g(v) + k }",
            ["v", "g", "k"],
        ),
        (
            "fn f(v: int, k: int = 1, g: fn(int) -> int = x => x) -> int { g(v) + k }",
            ["v", "k", "g"],
        ),
    ] {
        let ws = check(src).unwrap_or_else(|e| panic!("{src}: {e:?}"));
        let mid = ws.source_modules().next().expect("one user module").id;
        let hir = ws.module(mid);
        let (f_idx, _) = hir.functions.iter().next().expect("one function");
        let fbody = &hir.bodies[&BodyOwner::Function(f_idx)];
        let got: Vec<_> = fbody
            .param_bindings
            .map(|b| fbody.binding_arena[b].name.to_string())
            .collect();
        assert_eq!(got, names, "{src}");
    }
}

// The same contiguity trap for interpolation parts: the hole's `if` holds its
// own interpolated string, whose parts are allocated while the outer parts are
// being collected. The outer string has exactly 2 parts (`"a: "` and the `if`);
// a leaked inner `{x}` part printed the inner string twice.
#[test]
fn nested_interpolation_does_not_leak_into_outer_parts() {
    use bone_semantic::hir::{Expr, InterpPart};
    let src =
        "fn f(x: int, ok: bool) -> string {\n    \"a: {if ok { \"{x}\" } else { \"no\" }}\"\n}";
    let ws = check(src).unwrap();

    let mid = ws.source_modules().next().expect("one user module").id;
    let hir = ws.module(mid);

    let (f_idx, _) = hir.functions.iter().next().expect("one function");
    let fbody = &hir.bodies[&BodyOwner::Function(f_idx)];
    let body = fbody.expr_arena[fbody.root.expect("rock function has a body")];
    let Expr::Block {
        tail: Some(tail), ..
    } = body
    else {
        panic!(
            "function body should be a block with a tail, got {:?}",
            body
        );
    };
    let Expr::InterpolatedString(parts) = fbody.expr_arena[tail] else {
        panic!(
            "tail should be an interpolated string, got {:?}",
            fbody.expr_arena[tail]
        );
    };

    let parts: Vec<_> = parts.map(|p| fbody.interp_part_arena[p]).collect();
    assert_eq!(
        parts.len(),
        2,
        "inner interpolation parts leaked into the outer range: {parts:?}"
    );
    assert!(
        matches!(parts[0], InterpPart::Literal(s) if s == "a: "),
        "{parts:?}"
    );
    assert!(
        matches!(parts[1], InterpPart::Value { expr, .. } if matches!(fbody.expr_arena[expr], Expr::If { .. })),
        "{parts:?}"
    );
}

// Phase 0 of incremental compilation: a type error in one body must
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

    let mid = ws.source_modules().next().expect("one user module").id;
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
    let (s, e) = span.offset_range();
    &src[s..e]
}

/// All errors recorded against the named function's body.
fn recorded_errors<'a>(
    ws: &'a bone_semantic::inference::Workspace,
    fn_name: &str,
) -> &'a [CompilerError] {
    let mid = ws.source_modules().next().expect("one user module").id;
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
        errs.iter()
            .any(|e| e.message.contains("undefined_symbol_here")),
        "the real `Undefined global` error must survive, not be masked by the \
         return-type error; got: {:?}",
        errs.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

// A bare `{name}` that meant a local but named a global fn passed the checker
// and failed in lowering with no location.
#[test]
fn interpolation_hole_rejects_a_function_value_at_the_hole() {
    let src = "fn name(a: int) -> string {\n    return \"\"\n}\nfn f() -> string {\n    return \"x {name}\"\n}";
    let ws = check(src).expect("recovered, not aborted");
    let errs = recorded_errors(&ws, "f");
    let err = errs.first().expect("a recorded hole error");
    assert!(
        err.message.contains("cannot interpolate"),
        "got: {:?}",
        err.message
    );
    let span = err.labels.first().expect("labeled error").span;
    assert_eq!(text(src, &span), "name");
}

#[test]
fn interpolation_hole_rejects_a_unit_value_at_the_hole() {
    let src = "fn u() {\n}\nfn f() -> string {\n    return \"x {u()}\"\n}";
    let ws = check(src).expect("recovered, not aborted");
    let errs = recorded_errors(&ws, "f");
    let err = errs.first().expect("a recorded hole error");
    assert!(
        err.message.contains("cannot interpolate"),
        "got: {:?}",
        err.message
    );
    let span = err.labels.first().expect("labeled error").span;
    assert_eq!(text(src, &span), "u()");
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

    let mid = ws.source_modules().next().expect("one user module").id;
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

// bone-inc phase 1: every item owns a Body
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
    let mid = ws.source_modules().next().expect("one user module").id;
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

    let mid = ws.source_modules().next().expect("one user module").id;
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

    let mid = ws.source_modules().next().expect("one user module").id;
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
// interface storage.
#[test]
fn body_annotations_do_not_touch_module_ty_arena() {
    let ty_arena_len = |src: &str| {
        let ws = check(src).unwrap();
        let mid = ws.source_modules().next().expect("one user module").id;
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
        inline_body: None,
        is_extern: false,
        backend: bone_semantic::hir::FnBackend::Any,
        annotations: vec![],
        ffi_lib: None,
        is_synthetic: false,
        json_field_for: None,
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

#[test]
fn cross_module_items_resolve() {
    let cases: &[(&str, &str)] = &[
        // Cross-module call resolution: a function call must resolve against OTHER
        // modules in the workspace (the foundation for calling into a preloaded
        // stdlib), not just the current file. Two modules, distinct ids; module B
        // calls `helper` defined in module A. Single-source fixtures can't express
        // this, so it's white-box.
        // Distinct file ids so the two modules don't collide in the workspace
        // (semantic_pass keys modules by `Ast.file`).
        // `f`'s call to `helper` must resolve cross-module (into mod_a) and
        // `helper(5): int` must unify with f's declared `-> int`.
        (
            "fn helper(x: int) -> int { x }",
            "fn f() -> int { helper(5) }",
        ),
        // Bug 2: a struct defined in one module, used as a type in another, must
        // resolve cross-module. `lower_type_expr` resolves struct names against the
        // current module only, so a `Point` param in mod_b can't see mod_a's struct.
        (
            "struct Point { x: int, y: int }",
            "fn f(p: Point) -> int { 5 }",
        ),
        // Bug 3: field access on a cross-module struct. Inference looks the struct up
        // in the current module only, so `p.x` (Point from mod_a) can't find the def.
        // p.x must resolve Point's field cross-module (into mod_a) -> int
        (
            "struct Point { x: int, y: int }",
            "fn f(p: Point) -> int { p.x }",
        ),
        // Bug 3 (literal path): constructing a struct defined in another module.
        (
            "struct Point { x: int, y: int }",
            "fn f() -> Point { Point { x: 1, y: 2 } }",
        ),
    ];
    for (a, b) in cases {
        let mod_a = parse_rock_with_file(a, "mod_a").unwrap();
        let mod_b = parse_rock_with_file(b, "mod_b").unwrap();
        semantic_pass(&mut [mod_a, mod_b]).expect("cross-module use should resolve");
    }
}

/// Sentinel: `TyId` must stay niche-packed at 8 bytes. The packing depends on
/// `ModuleId` carrying a niche (`NonMaxU32`) so rustc folds the enum tag in. A
/// future change that drops the niche (e.g. `ModuleId(u32)`) silently
/// re-inflates `TyId` to 12 bytes and reverts the S5 memory-traffic shrink with
/// no other symptom -- this goes red the moment that happens.
#[test]
fn tyid_stays_niche_packed() {
    use std::mem::size_of;
    assert_eq!(
        size_of::<ModuleId>(),
        4,
        "ModuleId must stay 4 bytes (NonMaxU32)"
    );
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

#[test]
fn ref_type_survives_lowering_and_inference() {
    let ws = check(
        "struct Player { hp: int }\nfn identity(player: ref Player) -> ref Player { player }",
    )
    .expect("ref signature type checks");
    let module = ws.source_modules().next().unwrap();
    let (_, identity) = module
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "identity")
        .unwrap();
    assert_eq!(ws.render(None, identity.param_decls[0].ty), "ref Player");
    assert_eq!(
        ws.render(None, identity.return_ty.expect("return type")),
        "ref Player"
    );
}

fn arena_borrow_errors(body: &str) -> Vec<String> {
    let mut modules = rock_stdlib::stdlib_files()
        .iter()
        .map(|f| parse_rock_with_file(f.contents, &rock_stdlib::stdlib_file_id(f.name)).unwrap())
        .collect::<Vec<_>>();
    let src = format!(
        "value BorrowCell {{ n: int }}\nstruct BorrowRow {{ cell: BorrowCell }}\n\
         global CELLS: Arena<BorrowCell> = arena_new()\n\
         global ROWS: Arena<BorrowRow> = arena_new()\n\
         fn invalidate(h: Entity<BorrowCell>) {{ CELLS.remove(h) }}\n\
         fn consume(r: ref BorrowCell, h: Entity<BorrowCell>) {{ invalidate(h); r.n = 1 }}\n\
         fn later(r: ref BorrowCell, ignored: BorrowCell?) {{ r.n = 1 }}\n\
         extern(lua) fn opaque()\n{body}"
    );
    modules.push(parse_rock_with_file(&src, "borrow.rock").unwrap());
    module_error_messages(&semantic_pass(&mut modules).expect("valid Arena borrow fixture"))
}

#[test]
fn arena_value_borrows_reject_detected_invalidation() {
    for body in [
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); CELLS.remove(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); invalidate(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { consume(CELLS.borrow(h).unwrap(), h) }",
        "fn probe(h: Entity<BorrowCell>) { later(CELLS.borrow(h).unwrap(), CELLS.remove(h)) }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); let alias: ref BorrowCell = r; invalidate(h); alias.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); r = CELLS.borrow(h).unwrap(); invalidate(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); opaque(); r.n = 1 }",
    ] {
        let errors = arena_borrow_errors(body);
        assert!(
            errors.iter().any(|e| e.contains("Arena value borrow")),
            "{body}: {errors:?}"
        );
    }
}

#[test]
fn arena_value_borrow_checks_follow_hidden_returns_and_nested_bodies() {
    let mut missed = Vec::new();
    for (label, body) in [
        (
            "hidden global return",
            "global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 0 })\nfn hidden() -> ref BorrowCell { return CELLS.borrow(HELD).unwrap() }\nfn probe() { let r: ref BorrowCell = hidden(); CELLS.remove(HELD); r.n = 1 }",
        ),
        (
            "lambda body",
            "fn probe(h: Entity<BorrowCell>) { let f = () => { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); invalidate(h); r.n = 1 }; f() }",
        ),
        (
            "local function body",
            "fn probe(h: Entity<BorrowCell>) { fn nested(k: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(k).unwrap(); invalidate(k); r.n = 1 } nested(h) }",
        ),
        (
            "early hidden return",
            "global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 0 })\nfn hidden(flag: bool) -> ref BorrowCell { if flag { return CELLS.borrow(HELD).unwrap() }; CELLS.borrow(HELD).unwrap() }\nfn probe() { let r: ref BorrowCell = hidden(true); CELLS.remove(HELD); r.n = 1 }",
        ),
        (
            "invoked closure",
            "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); let f = () => invalidate(h); f(); r.n = 1 }",
        ),
        (
            "invoked local function",
            "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); fn nested(k: Entity<BorrowCell>) { invalidate(k) } nested(h); r.n = 1 }",
        ),
    ] {
        let errors = arena_borrow_errors(body);
        if !errors.iter().any(|e| e.contains("Arena value borrow")) {
            missed.push((label, errors));
        }
    }
    assert!(missed.is_empty(), "missed Arena invalidation: {missed:?}");
}

#[test]
fn arena_return_origins_follow_generics_and_both_recursive_entry_points() {
    let recursive = "global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 0 })\n\
        fn one(n: int) -> ref BorrowCell { if n == 0 { return CELLS.borrow(HELD).unwrap() }; two(n - 1) }\n\
        fn two(n: int) -> ref BorrowCell { one(n) }\n\
        fn forward<T>(r: ref T) -> ref T { r }";
    for origin in ["one(1)", "two(1)", "forward(one(1))"] {
        let body = format!(
            "{recursive}\nfn probe() {{ let r: ref BorrowCell = {origin}; CELLS.remove(HELD); r.n = 1 }}"
        );
        let errors = arena_borrow_errors(&body);
        assert!(
            errors.iter().any(|e| e.contains("Arena value borrow")),
            "{origin}: {errors:?}"
        );
    }
}

#[test]
fn arena_borrow_checks_preserve_separate_storage_and_fresh_reborrows() {
    for body in [
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); CELLS.insert(BorrowCell { n: 2 }); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); CELLS.clear(); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); CELLS.remove(h); let fresh = CELLS.insert(BorrowCell { n: 2 }); r = CELLS.borrow(fresh).unwrap(); r.n = 1 }",
        "fn probe(h: Entity<BorrowRow>) { let r: ref BorrowRow = ROWS.borrow(h).unwrap(); ROWS.remove(h); r.cell.n = 1 }",
        "fn probe(h: Entity<BorrowRow>) { let r: ref BorrowCell = ROWS.borrow(h).unwrap().cell; ROWS.remove(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowRow>) { let row = h.borrow().unwrap(); opaque(); row.cell.n = 1 }",
        "fn probe() { let cell = BorrowCell { n: 0 }; let r: ref BorrowCell = cell; opaque(); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); let cell = BorrowCell { n: 0 }; r = cell; CELLS.remove(h); r.n = 1 }",
        "fn stable(owner: ref BorrowRow) -> ref BorrowCell { return owner.cell }\nfn probe() { let owner = BorrowRow { cell: BorrowCell { n: 0 } }; let r: ref BorrowCell = stable(owner); opaque(); r.n = 1 }",
        "global HELD: Entity<BorrowRow> = ROWS.insert(BorrowRow { cell: BorrowCell { n: 0 } })\nfn stable() -> ref BorrowCell { return ROWS.borrow(HELD).unwrap().cell }\nfn probe() { let r: ref BorrowCell = stable(); ROWS.remove(HELD); opaque(); r.n = 1 }",
        "global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 0 })\nfn stable(owner: ref BorrowRow) -> ref BorrowCell { fn unused() -> int { return CELLS.borrow(HELD).unwrap().n } return owner.cell }\nfn probe() { let owner = BorrowRow { cell: BorrowCell { n: 0 } }; let r: ref BorrowCell = stable(owner); opaque(); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); let f = () => invalidate(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); fn nested(k: Entity<BorrowCell>) { invalidate(k) } r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { invalidate(h); let f = () => { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); r.n = 1 }; f() }",
        "fn probe(h: Entity<BorrowCell>) { invalidate(h); fn nested(k: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(k).unwrap(); r.n = 1 } nested(h) }",
        "fn one(n: int, r: ref BorrowCell) -> ref BorrowCell { if n == 0 { return r }; two(n - 1, r) }\nfn two(n: int, r: ref BorrowCell) -> ref BorrowCell { one(n, r) }\nfn forward<T>(r: ref T) -> ref T { r }\nfn probe() { let cell = BorrowCell { n: 0 }; let a: ref BorrowCell = forward(one(1, cell)); let b: ref BorrowCell = two(1, cell); opaque(); a.n = 1; b.n = 1 }",
        r#"
value OuterA { cell: BorrowCell }
value OuterB { cell: BorrowCell }
global AA: Arena<OuterA> = arena_new()
global BB: Arena<OuterB> = arena_new()
fn remove_a_then_use(r: ref BorrowCell, h: Entity<OuterA>) { AA.remove(h); r.n = 1 }
fn probe(ha: Entity<OuterA>, hb: Entity<OuterB>, flag: bool) {
    let a: ref BorrowCell = AA.borrow(ha).unwrap().cell
    let b: ref BorrowCell = BB.borrow(hb).unwrap().cell
    let unused: ref BorrowCell = if flag { a } else { b }
    remove_a_then_use(b, ha)
}
"#,
    ] {
        let errors = arena_borrow_errors(body);
        assert!(errors.is_empty(), "{body}: {errors:?}");
    }
}

// IDENTITY BORROW ACROSS SUSPENSION: an
// identity parameter is an implicit borrow that coroutine construction would
// retain before the first resume, so a `co fn` cannot take one at all.
#[test]
fn implicit_identity_parameter_is_rejected_on_a_coroutine() {
    for src in [
        "struct Player { hp: int }\nco fn bad(player: Player) -> Coroutine<int> { yield 1  player.hp = 1 }",
        "struct Player { hp: int }\nco fn bad(player: Player) -> Coroutine<int> { player.hp = 1  yield 1 }",
    ] {
        let err = rejection_messages(src);
        assert!(
            err.contains("coroutine parameter `player` cannot hold identity struct `Player`"),
            "a co fn identity parameter must reject, got: {err}"
        );
    }
}

#[test]
fn identity_local_cannot_live_across_a_coroutine_suspension() {
    let src = "struct Player { hp: int }\nfn make_player() -> Player { return Player { hp: 1 } }\nco fn bad() -> Coroutine<int> { let player = make_player()  yield 1  player.hp = 2 }";
    let err = rejection_messages(src);
    assert!(
        err.contains("identity borrow `player` is used after a coroutine suspension"),
        "a coroutine frame cannot retain an identity across yield, got: {err}"
    );
    let last_use_before_yield = "struct Player { hp: int }\nfn make_player() -> Player { return Player { hp: 1 } }\nco fn fine() -> Coroutine<int> { let player = make_player()  player.hp = 2  yield 1 }";
    assert_eq!(rejection_messages(last_use_before_yield), "");
}

#[test]
fn explicit_identity_ref_fills_an_implicit_identity_borrow_parameter() {
    let src = r#"
struct Player { hp: int }
fn damage(player: Player) { player.hp = 1 }
fn main(player: ref Player) { damage(player) }
"#;
    let ws = check(src).expect("semantic pass");
    let errors = ws
        .modules
        .iter()
        .filter_map(|module| ws.inference(module.id))
        .flat_map(|inference| inference.errors.values().flatten())
        .map(|error| error.message.clone())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "an explicit ref to an identity is the same borrow expected by an identity parameter: {errors:?}"
    );
}

#[test]
fn nobox_marker_reaches_typed_struct() {
    let ws = check("nobox value V { x: int }").expect("nobox value type checks");
    let module = ws.source_modules().next().unwrap();
    let struct_idx = module.struct_by_name[&Ustr::from("V")];
    assert!(module.structs[struct_idx].is_value);
    assert!(module.structs[struct_idx].is_nobox);
}

#[test]
fn nullable_operator_diagnostic_labels_operands_without_an_overload_menu() {
    let fixture = include_str!("fixtures/cw7_nullable_operator_diagnostic.rock");
    for section in test_utils::parse_sections(fixture).into_iter().take(3) {
        let ws = check(section.content).expect("body errors are collected");
        let errors: Vec<_> = ws
            .inferences_iter()
            .flat_map(|(_, inf)| inf.errors.values().flatten())
            .collect();
        assert_eq!(errors.len(), 1, "{}: {errors:?}", section.name);
        let error = errors[0];
        let (_, expected) = section
            .name
            .split_once(" @fail=")
            .expect("diagnostic contract");
        assert_eq!(error.message, expected, "{error:?}");
        let operands = if section.name.starts_with("both_nullable") {
            2
        } else {
            1
        };
        assert_eq!(error.labels.len(), operands, "{error:?}");
        for label in &error.labels {
            assert_eq!(label.message, "nullable operand");
            let (start, end) = label.span.offset_range();
            let operand = &section.content[start..end];
            assert!(matches!(operand, "x" | "y"), "{operand:?}");
            if operands == 1 {
                assert_eq!(
                    operand,
                    if section.name.starts_with("nullable_right") {
                        "y"
                    } else {
                        "x"
                    }
                );
            }
        }
    }
}

#[test]
fn nullable_operator_diagnostic_preserves_accessibility_precedence() {
    let files = crate::test_support::parse_embedded_files(
        r#"
//-- lib.rock @pkg=lib,1 @v1
pub value Number { pub n: int }
pub operator *(a: Number, b: int) -> int { b }
operator *(a: Number?, b: int) -> int { b }
//-- app.rock @pkg=app,2 @v1
use lib::{Number, operator(*)};
fn multiply(x: Number?, y: int) -> int { x * y }
"#,
    );
    let error = crate::test_support::check_types_embedded_with_stdlib(
        &files,
        &[(
            "<stdlib>/operators.rock",
            "operator *(a: float, b: float) -> float { a }",
        )],
    )
    .unwrap_err();
    assert!(
        error.contains("matching overload is not accessible"),
        "{error}"
    );
}

/// Overload visibility belongs to the call being resolved, not to the body.
/// `lib::f` contributes a public `f(int)` and a private `f(string)`; the std
/// `f(float)` joins the bare family, while `std::f` names the std member
/// alone. `g` has one public and one private overload, so a call sees a
/// single visible candidate.
fn check_overload_visibility(app: &str) -> Result<(), String> {
    let src = format!(
        r#"
//-- lib.rock @pkg=lib,1 @v1
pub fn f(n: int) -> int {{ n }}
fn f(s: string) -> string {{ s }}
pub fn g(n: int) -> int {{ n }}
fn g(s: string) -> string {{ s }}
//-- app.rock @pkg=app,2 @v1
use lib::{{f, g}};
{app}
"#
    );
    let files = crate::test_support::parse_embedded_files(&src);
    crate::test_support::check_types_embedded_with_stdlib(
        &files,
        &[("<stdlib>/probe.rock", "fn f(x: float) -> string { \"\" }")],
    )
}

#[test]
fn hidden_overload_diagnostic_survives_a_nested_same_name_call() {
    let error =
        check_overload_visibility("fn external() -> string { f(std::f(1.5)) }").unwrap_err();
    assert!(
        error.contains("No accessible overload for `f` with argument types (string); the matching overload is not accessible"),
        "{error}"
    );
}

#[test]
fn hidden_overload_diagnostic_follows_a_separate_same_name_call() {
    let error = check_overload_visibility(
        "fn external() -> string {\n    let n = f(1)\n    let s = std::f(1.5)\n    f(s)\n}",
    )
    .unwrap_err();
    assert!(
        error.contains("the matching overload is not accessible"),
        "{error}"
    );
}

#[test]
fn interleaved_calls_of_one_spelling_resolve_independently() {
    check_overload_visibility(
        "fn external() -> int {\n    let a = f(1)\n    let s = std::f(1.5)\n    let b = f(f(2))\n    let t: string = f(1.5)\n    f(a)\n}",
    )
    .unwrap();
}

#[test]
fn single_visible_candidate_with_a_hidden_member() {
    check_overload_visibility("fn external() -> int { g(g(1)) }").unwrap();
    let error = check_overload_visibility("fn external() -> string { g(\"x\") }").unwrap_err();
    assert!(
        error.contains("No accessible overload for `lib::g` with argument types (string); the matching overload is not accessible"),
        "{error}"
    );
}

#[test]
fn no_fit_stays_distinct_from_a_hidden_match() {
    for app in [
        "fn external() -> int { f(true) }",
        "fn external() -> int { g(true) }",
        "fn external() -> int { f(f(true)) }",
    ] {
        let error = check_overload_visibility(app).unwrap_err();
        assert!(error.contains("No overload for"), "{app}: {error}");
        assert!(!error.contains("not accessible"), "{app}: {error}");
    }
}

// Float literals in annotation args must survive lowering as
// `AnnoValue::Float` (incl. the unary-minus form) -- the value-test
// expectation surface (`@test(returns = 1.5)`) reads them; `Other` would make
// float expectations silently unreadable.
#[test]
fn annotation_float_args_lower_as_float() {
    let ws = check(
        "@test(returns = 1.5)\nfn a() -> float { return 1.5 }\n\
         @test(returns = -2.5)\nfn b() -> float { return -2.5 }\n",
    )
    .unwrap();
    let mid = ws.source_modules().next().expect("one user module").id;
    let mut got = vec![];
    for (_, f) in ws.module(mid).functions.iter() {
        for ann in &f.annotations {
            if ann.name.name.as_str() == "test" {
                for arg in &ann.args {
                    got.push(format!("{:?}", arg.value));
                }
            }
        }
    }
    got.sort();
    assert_eq!(
        got,
        vec!["Float(-2.5)", "Float(1.5)"],
        "float annotation args must not lower to Other"
    );
}

// Lossless lowering of param/field annotations: skull-side consumers (@size/@instance/@flat) read
// these from HIR, so
// dropping them at lowering is a silent-drop hole.
#[test]
fn param_annotations_survive_lowering() {
    let ws = check("fn f(@size(32) xs: int, plain: int) -> int { return xs + plain }\n").unwrap();
    let mid = ws.source_modules().next().expect("one user module").id;
    let (_, f) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "f")
        .expect("fn f");
    let annotated = &f.param_decls[0];
    assert_eq!(
        annotated.annotations.len(),
        1,
        "param annotation must survive lowering"
    );
    assert_eq!(annotated.annotations[0].name.name.as_str(), "size");
    assert!(
        matches!(
            annotated.annotations[0].args[0].value,
            bone_semantic::hir::AnnoValue::Int(32)
        ),
        "annotation arg must lower with its value, got {:?}",
        annotated.annotations[0].args[0].value
    );
    assert!(
        f.param_decls[1].annotations.is_empty(),
        "unannotated param stays empty"
    );
}

#[test]
fn field_annotations_survive_lowering() {
    let ws = check("value V { @flat id: int, pos: float }\n").unwrap();
    let mid = ws.source_modules().next().expect("one user module").id;
    let s = ws
        .module(mid)
        .structs
        .iter()
        .map(|(_, s)| s)
        .find(|s| s.name.as_str() == "V")
        .expect("struct V");
    let annotated = &s.fields[0];
    assert_eq!(
        annotated.annotations.len(),
        1,
        "field annotation must survive lowering"
    );
    assert_eq!(annotated.annotations[0].name.name.as_str(), "flat");
    assert!(annotated.annotations[0].args.is_empty());
    assert!(
        s.fields[1].annotations.is_empty(),
        "unannotated field stays empty"
    );
}

// A resolved call to a generic fn persists the winning candidate's
// instantiated generics (declaration order) in `call_generic_bindings` --
// the binding source for skull's inliner call-site specialization.
#[test]
fn generic_call_records_instantiation_bindings() {
    let src = r#"value V2 { x: float, y: float }
fn pick<T>(a: T, b: T) -> T { return a }
fn main() {
    let v = pick(V2 { x: 1.0, y: 2.0 }, V2 { x: 3.0, y: 4.0 })
    let i = pick(1, 2)
}"#;
    let ws = check(src).expect("clean check");
    let mid = ws.source_modules().next().expect("one user module").id;
    let hir = ws.module(mid);
    let (main_idx, _) = hir
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "main")
        .expect("main exists");
    let inf = ws.inference(mid).expect("user module inferred");
    let body = inf
        .per_fn
        .get(&BodyOwner::Function(main_idx))
        .expect("main inferred");

    assert_eq!(
        body.call_generic_bindings.len(),
        2,
        "both pick() calls must record bindings; got {:?}",
        body.call_generic_bindings
    );
    let mut rendered: Vec<String> = body
        .call_generic_bindings
        .values()
        .map(|bindings| {
            assert_eq!(bindings.len(), 1, "pick has one generic param");
            ws.render(Some(body), bindings[0])
        })
        .collect();
    rendered.sort();
    assert_eq!(
        rendered,
        vec!["V2".to_string(), "int".to_string()],
        "T must resolve to the concrete instantiation per call site"
    );
}

/// The backend-variant collapse keeps each signature group together, in
/// order of first appearance. A group with a repeated backend is not
/// collapsed, so its members stay adjacent: the "available overloads" list
/// is in collapse order, not declaration order.
#[test]
fn backend_variant_groups_list_in_first_appearance_order() {
    let src = r#"
inline(lua) fn g(x: int) -> int = """return x"""
fn g(x: string) -> int { return 0 }
inline(lua) fn g(x: int) -> int = """return x"""
extern(js) fn g(x: bool, y: int) -> int
fn main() {
    let _ = g(1.5)
}
"#;
    let msgs = check(src)
        .expect("semantic_pass")
        .inferences_iter()
        .flat_map(|(_, inf)| inf.errors.values().flatten().cloned().collect::<Vec<_>>())
        .map(|e| e.message)
        .collect::<Vec<_>>();
    let expected = "No overload for `g` with argument types (float)\navailable overloads:\n  \
                    g(x: int) -> int\n  g(x: int) -> int\n  g(x: string) -> int\n  g(x: bool, y: int) -> int";
    assert!(msgs.iter().any(|m| m == expected), "{msgs:?}");
}

#[test]
fn legacy_caller_field_access_enforces_v1_visibility_and_allows_legacy() {
    use bone_semantic::inference::{
        AccessDenied, AccessSite, FieldAccessNeed, Gamma, field_access,
    };
    use bone_semantic::semantic_passes::semantic_pass_with_aliases;
    use skull_parser::{Dialect, parse_module_with_dialect};

    let v1_src = r#"
pub struct Actor {
    pub health: int,
    pub private(set) mana: int,
    private secret: int,
}
"#;
    let mut v1_mod =
        parse_module_with_dialect(v1_src, ustr("lib.rock"), Dialect::V1).expect("parse v1");
    v1_mod.package = Some(rock_core::source_code::PackageTag {
        id: 1,
        self_name: "game".to_string(),
        identity_storage: Default::default(),
        root: String::new(),
    });

    let legacy_src = r#"
struct LegacyActor {
    health: int,
}
"#;
    let legacy_mod = parse_module_with_dialect(legacy_src, ustr("legacy.rock"), Dialect::Legacy)
        .expect("parse legacy");

    let mut aliases = bone_core::modules::NsAliases::default();
    aliases.packages.push(bone_core::modules::PackageAliases {
        id: 1,
        self_name: ustr("game"),
        aliases: vec![],
    });

    let mut modules = vec![v1_mod, legacy_mod];
    let ws = semantic_pass_with_aliases(&mut modules, &aliases).expect("semantic pass");

    let mid = ws
        .modules
        .iter()
        .find(|m| m.file == ustr("lib.rock"))
        .unwrap()
        .id;
    let body = bone_core::hir::Body::default();
    let gamma = Gamma::new(&ws, &body, mid);

    let v1_key = ustr("game::Actor");
    let legacy_site = AccessSite {
        package: None,
        module_path: &[],
    };

    // Public field: legacy can read and write
    assert!(
        field_access(
            &gamma,
            v1_key,
            ustr("health"),
            legacy_site,
            FieldAccessNeed::Read
        )
        .is_ok()
    );
    assert!(
        field_access(
            &gamma,
            v1_key,
            ustr("health"),
            legacy_site,
            FieldAccessNeed::Write
        )
        .is_ok()
    );

    // private(set) field: legacy can read
    assert!(
        field_access(
            &gamma,
            v1_key,
            ustr("mana"),
            legacy_site,
            FieldAccessNeed::Read
        )
        .is_ok()
    );

    // private(set) field: legacy write is denied
    let write_mana = field_access(
        &gamma,
        v1_key,
        ustr("mana"),
        legacy_site,
        FieldAccessNeed::Write,
    );
    assert!(
        matches!(
            write_mana,
            Err(AccessDenied::Privilege {
                need: FieldAccessNeed::Write,
                ..
            })
        ),
        "legacy caller writing private(set) field must be denied"
    );

    // private field: legacy read and write are both denied
    let read_secret = field_access(
        &gamma,
        v1_key,
        ustr("secret"),
        legacy_site,
        FieldAccessNeed::Read,
    );
    assert!(
        matches!(
            read_secret,
            Err(AccessDenied::Privilege {
                need: FieldAccessNeed::Read,
                ..
            })
        ),
        "legacy caller reading private field must be denied"
    );
    let write_secret = field_access(
        &gamma,
        v1_key,
        ustr("secret"),
        legacy_site,
        FieldAccessNeed::Write,
    );
    assert!(
        matches!(
            write_secret,
            Err(AccessDenied::Privilege {
                need: FieldAccessNeed::Write,
                ..
            })
        ),
        "legacy caller writing private field must be denied"
    );

    // Legacy-owned struct: legacy caller has unrestricted read/write
    let legacy_key = ustr("LegacyActor");
    assert!(
        field_access(
            &gamma,
            legacy_key,
            ustr("health"),
            legacy_site,
            FieldAccessNeed::Read
        )
        .is_ok()
    );
    assert!(
        field_access(
            &gamma,
            legacy_key,
            ustr("health"),
            legacy_site,
            FieldAccessNeed::Write
        )
        .is_ok()
    );

    // v1 caller within game package: can access private and private(set) fields
    let v1_site = AccessSite {
        package: Some(1),
        module_path: &[],
    };
    assert!(
        field_access(
            &gamma,
            v1_key,
            ustr("mana"),
            v1_site,
            FieldAccessNeed::Write
        )
        .is_ok()
    );
    assert!(
        field_access(
            &gamma,
            v1_key,
            ustr("secret"),
            v1_site,
            FieldAccessNeed::Read
        )
        .is_ok()
    );
    assert!(
        field_access(
            &gamma,
            v1_key,
            ustr("secret"),
            v1_site,
            FieldAccessNeed::Write
        )
        .is_ok()
    );
}

#[test]
fn structural_interface_conformance_rejects_private_set_field_for_writable_interface() {
    use bone_semantic::inference::{Gamma, coerce_to_interface};
    use bone_semantic::semantic_passes::semantic_pass_with_aliases;
    use skull_parser::{Dialect, parse_module_with_dialect};

    let lib_src = r#"
pub struct Actor {
    pub health: int,
    pub private(set) mana: int,
    private secret: int,
}
"#;
    let mut lib_mod =
        parse_module_with_dialect(lib_src, ustr("lib.rock"), Dialect::V1).expect("parse lib");
    lib_mod.package = Some(rock_core::source_code::PackageTag {
        id: 1,
        self_name: "game".to_string(),
        identity_storage: Default::default(),
        root: String::new(),
    });

    let consumer_src = r#"
pub struct WritableMana {
    pub mana: int,
}
pub struct WritableHealth {
    pub health: int,
}
pub struct MissingField {
    pub missing: int,
}
pub struct ReadableSecret {
    pub secret: int,
}
"#;
    let mut consumer_mod =
        parse_module_with_dialect(consumer_src, ustr("consumer.rock"), Dialect::V1)
            .expect("parse consumer");
    consumer_mod.package = Some(rock_core::source_code::PackageTag {
        id: 2,
        self_name: "consumer".to_string(),
        identity_storage: Default::default(),
        root: String::new(),
    });

    let mut aliases = bone_core::modules::NsAliases::default();
    aliases.packages.push(bone_core::modules::PackageAliases {
        id: 1,
        self_name: ustr("game"),
        aliases: vec![],
    });
    aliases.packages.push(bone_core::modules::PackageAliases {
        id: 2,
        self_name: ustr("consumer"),
        aliases: vec![(ustr("game"), 1)],
    });

    let mut modules = vec![lib_mod, consumer_mod];
    let mut ws = semantic_pass_with_aliases(&mut modules, &aliases).expect("semantic pass");

    let iface_mana = ustr("consumer::WritableMana");
    let iface_health = ustr("consumer::WritableHealth");
    let iface_missing = ustr("consumer::MissingField");
    let iface_secret = ustr("consumer::ReadableSecret");
    ws.interface_names.insert(iface_mana);
    ws.interface_names.insert(iface_health);
    ws.interface_names.insert(iface_missing);
    ws.interface_names.insert(iface_secret);

    let consumer_mid = ws
        .modules
        .iter()
        .find(|m| m.file == ustr("consumer.rock"))
        .unwrap()
        .id;
    let body = bone_core::hir::Body::default();
    let mut gamma = Gamma::new(&ws, &body, consumer_mid);

    let actor_ty = gamma.nominal_app(ustr("game::Actor"), &[], Span::unknown());

    // Coercing Actor to WritableHealth is allowed: `health` is pub read/write
    let ok_res = coerce_to_interface(&mut gamma, actor_ty, iface_health, Span::unknown());
    assert!(
        ok_res.is_ok(),
        "coercing to interface with pub read/write field must succeed"
    );

    // Coercing Actor to WritableMana must fail: `mana` is pub private(set), not writable from consumer
    let err_res = coerce_to_interface(&mut gamma, actor_ty, iface_mana, Span::unknown());
    assert!(
        err_res.is_err(),
        "coercing Actor to WritableMana must fail because mana is private(set)"
    );
    let err = err_res.unwrap_err();
    assert_eq!(
        err.message,
        "field `mana` on struct `Actor` is not writable from this module"
    );

    // A genuinely absent source field gets the interface-specific diagnostic.
    let missing_err = coerce_to_interface(&mut gamma, actor_ty, iface_missing, Span::unknown())
        .expect_err("coercing Actor to an interface with a missing field must fail");
    assert_eq!(
        missing_err.message,
        "type game::Actor does not have required field `missing` of interface `consumer::MissingField`"
    );

    // A present but inaccessible source field keeps the access diagnostic.
    let secret_err = coerce_to_interface(&mut gamma, actor_ty, iface_secret, Span::unknown())
        .expect_err("coercing Actor to an interface with a private field must fail");
    assert_eq!(
        secret_err.message,
        "field `secret` on struct `Actor` is not accessible from this module"
    );
}

fn identity_storage_errors(
    std_file: &str,
    std_src: &str,
    package: Option<&str>,
    user_src: &str,
) -> Vec<String> {
    let std_module = parse_rock_with_file(std_src, std_file).expect("parse stdlib module");
    let mut user_module = parse_rock_with_file(user_src, "main.rock").expect("parse user module");
    user_module.package = package.map(|name| rock_core::source_code::PackageTag {
        id: 1,
        self_name: name.to_string(),
        identity_storage: Default::default(),
        root: String::new(),
    });
    let ws = semantic_pass(&mut [std_module, user_module]).expect("semantic pass");
    module_error_messages(&ws)
}

/// Every declaration and body diagnostic in `ws`.
fn module_error_messages(ws: &bone_semantic::inference::Workspace) -> Vec<String> {
    ws.modules
        .iter()
        .flat_map(|m| {
            m.interface_errors
                .iter()
                .chain(
                    ws.inference(m.id)
                        .into_iter()
                        .flat_map(|inf| inf.errors.values().flatten()),
                )
                .map(|e| e.to_string())
        })
        .collect()
}

/// The diagnostics bone reports for one standalone, identity-enforced source.
fn rejection_messages(src: &str) -> String {
    match check(src) {
        Ok(ws) => module_error_messages(&ws).join("\n"),
        Err(err) => err.to_string(),
    }
}

const MINI_ARENA_STORE: &str = "struct ArenaStore<T> {\n    generations: [int],\n    @identity_storage\n    data: [T],\n}\nstruct PlainStore<T> { data: [T] }";

#[test]
fn identity_storage_edge_admits_only_direct_rows() {
    let arena = "<stdlib>/arena.rock";
    let admitted = identity_storage_errors(
        arena,
        MINI_ARENA_STORE,
        Some("game"),
        "struct Mob { hp: int }\nglobal MOBS: ArenaStore<Mob> = ArenaStore { generations: [], data: [] }",
    );
    assert!(
        admitted.is_empty(),
        "the annotated edge stores Arena rows: {admitted:?}"
    );

    for (label, user) in [
        (
            "not a recursive waiver",
            "struct Mob { hp: int }\nglobal MOBS: ArenaStore<[Mob]> = ArenaStore { generations: [], data: [] }",
        ),
        (
            "ordinary Vec field",
            "struct Mob { hp: int }\nglobal MOBS: PlainStore<Mob> = PlainStore { data: [] }",
        ),
    ] {
        let errors = identity_storage_errors(arena, MINI_ARENA_STORE, Some("game"), user);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("cannot store identity struct `Mob`")),
            "{label} must reject: {errors:?}"
        );
    }
}

#[test]
fn identity_storage_capability_is_the_arena_files_alone() {
    let forged = identity_storage_errors(
        "<stdlib>/scene.rock",
        MINI_ARENA_STORE,
        Some("game"),
        "fn main() {}",
    );
    assert!(
        forged
            .iter()
            .any(|e| e.contains("`@identity_storage` is reserved")),
        "another stdlib file cannot claim the Arena row store: {forged:?}"
    );
    let not_vec = identity_storage_errors(
        "<stdlib>/arena.rock",
        "struct ArenaStore<T> {\n    @identity_storage\n    row: T,\n}",
        Some("game"),
        "fn main() {}",
    );
    assert!(
        not_vec.iter().any(|e| e.contains("marks the Vec field")),
        "the capability names the row Vec: {not_vec:?}"
    );
}

#[test]
fn identity_storage_map_field_reports_only_the_capability() {
    let errors = identity_storage_errors(
        "<stdlib>/arena.rock",
        MINI_ARENA_STORE,
        Some("game"),
        "struct Mob { hp: int }\nstruct Rows {\n    @identity_storage\n    by_id: Map<int, Mob>,\n}",
    );
    assert_eq!(
        errors,
        ["`@identity_storage` is reserved for the standard arena row store"],
    );
}

// A package's own declaration decides whether its sites are reported: the
// package name exempts nothing, and stdlib and unpackaged programs keep the
// compiler's built-in exemption.
#[test]
fn identity_storage_exemption_follows_the_package_declaration() {
    use rock_core::source_code::IdentityStorage;
    let user = "struct Mob { hp: int }\nstruct Room { mobs: [Mob] }\nfn count(held: [Mob]) -> int { return 0 }";
    let enforced =
        identity_storage_errors("<stdlib>/fixture.rock", "fn print() {}", Some("game"), user);
    assert!(
        enforced
            .iter()
            .any(|e| e.contains("field `mobs` cannot store identity struct `Mob`")),
        "a package that declares nothing is enforced: {enforced:?}"
    );
    assert!(
        enforced
            .iter()
            .any(|e| e.contains("parameter `held` cannot store identity struct `Mob`")),
        "the body check follows the same declaration: {enforced:?}"
    );
    let unpackaged = identity_storage_errors("<stdlib>/fixture.rock", "fn print() {}", None, user);
    assert!(
        unpackaged.is_empty(),
        "unpackaged programs are exempt outside bone fixtures: {unpackaged:?}"
    );
    let check = |identity_storage: IdentityStorage| {
        let mut module = parse_rock_with_file(user, "main.rock").expect("parse user module");
        module.package = Some(rock_core::source_code::PackageTag {
            id: 1,
            self_name: "migrating".into(),
            identity_storage,
            root: String::new(),
        });
        let ws = bone_semantic::semantic_passes::semantic_pass_into(
            Default::default(),
            &mut [module],
            &Default::default(),
        )
        .expect("semantic pass");
        module_error_messages(&ws)
    };
    let exempt = check(IdentityStorage::Exempt);
    assert!(
        exempt.is_empty(),
        "a package declared exempt gets no diagnostics: {exempt:?}"
    );
    let declared = check(IdentityStorage::Enforced);
    assert!(
        declared
            .iter()
            .any(|e| e.contains("field `mobs` cannot store identity struct `Mob`")),
        "a package declared enforced is enforced: {declared:?}"
    );
}

// A declared module exemption covers a qualified module path prefix of its own
// package, read from each file's `mod` header: the module it names and every
// module below it, never a sibling that shares its spelling or a file with no
// path.
#[test]
fn identity_storage_module_exemption_follows_the_module_path() {
    use bone_core::modules::{NsAliases, PackageAliases};
    use bone_semantic::identity_storage::{IdentityStorageExemption, identity_storage_exemption};
    use bone_semantic::semantic_passes::semantic_pass_with_aliases;
    use rock_core::source_code::{IdentityStorage, PackageTag};
    use skull_parser::{Dialect, parse_module_with_dialect};

    let files = [
        ("nav.rock", "pub mod nav;", "NavMob"),
        ("nav/graph.rock", "pub mod nav::graph;", "GraphMob"),
        ("navigation.rock", "pub mod navigation;", "NavigationMob"),
        ("world.rock", "pub mod world;", "WorldMob"),
        ("lib.rock", "", "RootMob"),
    ];
    let build = |game: IdentityStorage| {
        let tag = |id: u32, self_name: &str, identity_storage: IdentityStorage| PackageTag {
            id,
            self_name: self_name.to_string(),
            identity_storage,
            root: String::new(),
        };
        let mut modules: Vec<_> = files
            .iter()
            .map(|(file, header, mob)| {
                let src = format!("{header}\npub struct {mob} {{ hp: int }}\npub struct {mob}Room {{ mobs: [{mob}] }}\n");
                let mut module = parse_module_with_dialect(&src, ustr(file), Dialect::V1).expect("parse v1");
                module.package = Some(tag(1, "game", game.clone()));
                module
            })
            .collect();
        let mut legacy = parse_module_with_dialect(
            "struct LegacyMob { hp: int }\nstruct LegacyMobRoom { mobs: [LegacyMob] }\n",
            ustr("legacy.rock"),
            Dialect::Legacy,
        )
        .expect("parse legacy");
        legacy.package = Some(tag(1, "game", game.clone()));
        modules.push(legacy);
        let mut other = parse_module_with_dialect(
            "pub mod world;\npub struct OtherMob { hp: int }\npub struct OtherMobRoom { mobs: [OtherMob] }\n",
            ustr("other/world.rock"),
            Dialect::V1,
        )
        .expect("parse other v1");
        other.package = Some(tag(2, "other", IdentityStorage::Enforced));
        modules.push(other);
        let aliases = NsAliases {
            packages: vec![
                PackageAliases {
                    id: 1,
                    self_name: ustr("game"),
                    aliases: vec![],
                },
                PackageAliases {
                    id: 2,
                    self_name: ustr("other"),
                    aliases: vec![],
                },
            ],
        };
        semantic_pass_with_aliases(&mut modules, &aliases).expect("semantic pass")
    };

    let ws = build(IdentityStorage::Enforced);
    let errors = module_error_messages(&ws);
    for (_, _, mob) in files {
        assert!(
            errors
                .iter()
                .any(|e| e.contains("cannot store identity struct")
                    && e.contains(&format!("{mob}`;"))),
            "every module holds the same violation while `game` is enforced: {errors:?}"
        );
    }

    let ws = build(IdentityStorage::ExemptModules(vec![
        "nav".to_string(),
        "world".to_string(),
    ]));
    let exemption_of = |file: &str| {
        let module = ws
            .modules
            .iter()
            .find(|m| m.file == ustr(file))
            .expect("fixture module");
        identity_storage_exemption(&ws, module)
    };
    let nav = Some(IdentityStorageExemption::Module(ustr("nav")));
    assert_eq!(exemption_of("nav.rock"), nav);
    assert_eq!(exemption_of("nav/graph.rock"), nav);
    assert_eq!(
        exemption_of("navigation.rock"),
        None,
        "a prefix matches whole segments"
    );
    assert_eq!(
        exemption_of("world.rock"),
        Some(IdentityStorageExemption::Module(ustr("world")))
    );
    assert_eq!(
        exemption_of("other/world.rock"),
        None,
        "a declaration covers its own package only"
    );
    assert_eq!(
        exemption_of("lib.rock"),
        None,
        "the package root is not under `nav`"
    );
    assert!(!ws.fragment_scopes.contains_key(&ustr("legacy.rock")));
    assert_eq!(
        exemption_of("legacy.rock"),
        None,
        "a module with no v1 path matches no prefix"
    );
    let errors = module_error_messages(&ws);
    assert!(
        !errors
            .iter()
            .any(|e| e.contains("NavMob`;") || e.contains("GraphMob`;")),
        "exempt modules get no diagnostics: {errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.contains("NavigationMob`;")),
        "{errors:?}"
    );
}

#[test]
fn migrated_package_identity_storage_is_enforced() {
    let user = "struct Mob { hp: int }\nstruct Room { mobs: [Mob] }\nfn count(held: [Mob]) -> int { return 0 }";
    let errors = identity_storage_errors(
        "<stdlib>/fixture.rock",
        "fn print() {}",
        Some("migrated_package"),
        user,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.contains("field `mobs` cannot store identity struct `Mob`")),
        "migrated_package fields must reject ordinary identity storage: {errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| e.contains("parameter `held` cannot store identity struct `Mob`")),
        "migrated_package bodies must reject ordinary identity storage: {errors:?}"
    );
}

// Identity-to-`any` erasure is recorded where `coerce` accepts the `any`
// target, and a coercion inside a trial that is rolled back leaves no record:
// overload scoring and `as` classification try coercions they then undo.
#[test]
fn any_erasure_record_follows_trial_rollback() {
    use bone_semantic::inference::{Gamma, coerce};

    let ws = check("struct Mob { hp: int }\nfn main() { }").expect("semantic pass");
    let mid = ws
        .modules
        .iter()
        .find(|m| m.file == ustr("<parse_rock>"))
        .unwrap()
        .id;
    let body = bone_core::hir::Body::default();
    let mut gamma = Gamma::new(&ws, &body, mid);
    let mob = gamma.nominal_app(ustr("Mob"), &[], Span::unknown());
    let any = ws.builtins.any();

    let snap = gamma.out.checkpoint();
    assert!(gamma.trial(|g| coerce(g, mob, any, Span::unknown()).is_ok()));
    assert_eq!(
        gamma.out.any_erasures.len(),
        1,
        "the accepted erasure is recorded"
    );
    gamma.out.rollback_to(snap);
    assert!(
        gamma.out.any_erasures.is_empty(),
        "a rolled-back trial keeps no erasure"
    );

    coerce(&mut gamma, mob, any, Span::unknown()).expect("identity coerces to any at inference");
    assert_eq!(
        gamma.out.any_erasures.len(),
        1,
        "a committed coercion keeps its erasure"
    );
}

#[test]
fn enumerated_arena_records_the_lazy_iterator_protocol() {
    let ws = check(
        r#"
        value Entity<T> { idx: int }
        value Arena<T> { count: int }
        struct AIter<T> { arena: Arena<T> }
        fn iter<T>(arena: Arena<T>) -> AIter<T> { AIter { arena: arena } }
        fn next<T>(it: AIter<T>) -> (bool, (Entity<T>, T)) { next(it) }
        struct EnumIter<I, T> { inner: I }
        fn enumerate<I: Iterator<T>, T>(it: I) -> EnumIter<I, T> { EnumIter { inner: it } }
        fn next<I: Iterator<T>, T>(it: EnumIter<I, T>) -> (bool, (int, T)) { next(it) }
        fn f(arena: Arena<int>) {
            for (index, (entity, value)) in enumerate(arena) { let seen = value }
        }
    "#,
    )
    .expect("enumerated Arena typechecks");
    let module = &ws.modules[1];
    let (fid, _) = module
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "f")
        .unwrap();
    let inference = &ws.inference(module.id).unwrap().per_fn[&BodyOwner::Function(fid)];
    assert_eq!(inference.for_iter_protocol.len(), 1);
    assert!(
        inference
            .for_iter_protocol
            .values()
            .all(|info| info.iter.is_none())
    );
}

fn annotation_resolution_errors(sources: &[(&str, &str)]) -> Vec<String> {
    use bone_core::modules::{NsAliases, PackageAliases};
    let mut modules: Vec<_> = sources
        .iter()
        .map(|(file, source)| {
            let mut ast = skull_parser::parse_module_clean_with_dialect(
                source,
                (*file).into(),
                skull_parser::Dialect::V1,
            )
            .unwrap();
            ast.package = Some(rock_core::source_code::PackageTag {
                id: 1,
                self_name: "sample".into(),
                identity_storage: Default::default(),
                root: String::new(),
            });
            ast
        })
        .collect();
    let aliases = NsAliases {
        packages: vec![PackageAliases {
            id: 1,
            self_name: "sample".into(),
            aliases: vec![],
        }],
    };
    match semantic_pass_with_aliases(&mut modules, &aliases) {
        Err(error) => vec![error.message],
        Ok(ws) => ws
            .modules
            .iter()
            .flat_map(|module| {
                module
                    .interface_errors
                    .iter()
                    .chain(module.lowering_errors.values().flatten())
            })
            .chain(
                ws.inferences_iter()
                    .flat_map(|(_, inf)| inf.errors.values().flatten()),
            )
            .map(|error| error.message.clone())
            .collect(),
    }
}

#[test]
fn ambiguous_annotation_does_not_select_builtin() {
    let errors = annotation_resolution_errors(&[
        ("a.rock", "mod a; pub deco init test {}"),
        ("b.rock", "mod b; pub deco init test {}"),
        ("main.rock", "use a::*; use b::*; @test fn probe() {}"),
    ]);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("test") && error.contains("ambiguous")),
        "{errors:?}"
    );
    let valid = annotation_resolution_errors(&[
        ("a.rock", "mod a; pub deco init test {}"),
        ("main.rock", "use a::*; @test fn probe() {}"),
    ]);
    assert!(valid.is_empty(), "{valid:?}");
}

#[test]
fn custom_decorator_named_frame_ref_source_does_not_pin_a_call_result() {
    let errors = annotation_resolution_errors(&[(
        "main.rock",
        "deco init frame_ref_source {}
        value Position { x: int }
        @frame_ref_source
        fn same(position: ref Position) -> ref Position { return position }
        fn main() {
            let positions = [Position { x: 1 }]
            let position: ref Position = same(positions[0])
            position.x = 3
        }",
    )]);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("relocatable Vec or Map storage")),
        "{errors:?}"
    );
}

#[test]
fn annotation_wire_onchange_references_must_resolve() {
    for (name, diagnostic) in [
        ("missing", "did not resolve"),
        ("changed", "not a single definition"),
    ] {
        let source = format!(
            "use a::*; use b::*; deco init wire {{}} @wire(onchange=[{name}]) fn probe() {{}}"
        );
        let errors = annotation_resolution_errors(&[
            ("a.rock", "mod a; pub fn changed() {}"),
            ("b.rock", "mod b; pub fn changed() {}"),
            ("main.rock", &source),
        ]);
        assert!(
            errors
                .iter()
                .any(|error| error.contains(name) && error.contains(diagnostic)),
            "{name}: {errors:?}"
        );
    }
    let valid = annotation_resolution_errors(&[(
        "main.rock",
        "deco init wire {} fn changed() {} @wire(onchange=[changed]) fn probe() {}",
    )]);
    assert!(valid.is_empty(), "{valid:?}");
}

// A legacy user struct sharing a stdlib type's name is the USER's duplicate,
// whatever its generic arity: the workspace records `duplicate struct` on the
// user module and no diagnostic may point into the stdlib. Regression: the
// user's arity overwrote the workspace-wide arity map, so a stdlib `Scene<T>`
// use failed first and the fail-fast masked the duplicate -- the only error
// pointed at `<stdlib>/scene.rock`.
#[test]
fn legacy_struct_sharing_a_stdlib_name_reports_the_duplicate_at_the_user_file() {
    let mut modules = Vec::new();
    for f in rock_stdlib::stdlib_files() {
        modules.push(
            skull_parser::parse_module_clean(
                f.contents,
                Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str()),
            )
            .expect("stdlib parses"),
        );
    }
    let user_src = "struct Scene { name: string }\nfn main() {\n    let s = Scene { name: \"hi\" }\n    print(s.name)\n}\n";
    modules.push(
        skull_parser::parse_module_clean(user_src, Ustr::from("user.rock")).expect("user parses"),
    );
    let ws = match semantic_pass(&mut modules) {
        Ok(ws) => ws,
        Err(e) => panic!("expected a recorded duplicate error, got fail-fast: {e}"),
    };
    let errs: Vec<&CompilerError> = ws
        .inferences_iter()
        .flat_map(|(_, inf)| inf.errors.values().flatten())
        .chain(
            ws.modules
                .iter()
                .flat_map(|m| m.lowering_errors.values().flatten()),
        )
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .collect();
    assert!(
        errs.iter()
            .any(|e| e.message == "duplicate struct `Scene` across modules"
                && e.location.file == Ustr::from("user.rock")),
        "expected the duplicate at the user file, got: {:?}",
        errs.iter()
            .map(|e| (e.message.as_str(), e.location.file))
            .collect::<Vec<_>>()
    );
    for e in &errs {
        for file in std::iter::once(e.location.file).chain(e.labels.iter().map(|l| l.span.file)) {
            assert!(
                !is_stdlib_module_file(file.as_str()),
                "no diagnostic may point into the stdlib, got `{}` at {file}",
                e.message
            );
        }
    }
}

/// `Body::for_each_child` reaches every expression of every body from the
/// body's roots: the whole stdlib plus a module that spells each HIR shape
/// with child expressions. A variant whose children the walk forgot leaves
/// them unreached here.
#[test]
fn hir_child_walk_reaches_every_expression() {
    use bone_core::hir::Expr;
    use bone_core::hir_walk::HirChild;

    let src = r#"
struct P { x: int = 1 + 2, y: int = 3 }
enum Shape { Dot, Circle(float), Rect { w: float, h: float } }
global G: int = 0
fn takes(a: int, b: int = 4 * 2) -> int { a + b }
fn shapes(s: Shape) -> float {
    match s {
        Shape::Dot => 0.0,
        Shape::Circle(r) if r > 1.0 => r,
        Shape::Circle(r) => -r,
        Shape::Rect { w, h } => w * h,
    }
}
fn all(xs: [int], n: int?, p: P) -> string {
    let t = (1, 2)
    let (a, b) = t
    let arr = [a, b, t.0]
    let q = P { ..p, x: 5 }
    let r = Shape::Rect { w: 1.0, h: 2.0 }
    let c = Shape::Circle(2.0)
    let f = (k: int) => k + a
    fn helper(k: int) -> int { k * 2 }
    static S: int = 7
    defer { G = 1 }
    if let v = n { G += v } else { G -= 1 }
    while let v = n { G = v; break }
    while G < 3 { G++ }
    for i in 0..3 { if i == 1 { continue } }
    let m = n?.let { n + 1 }
    let o = n?.takes(1)
    let s = xs[0] as float
    let width = 3
    let digits = 2
    match n { 1 | 2 => G = 0, 3..5 => G = 1, _ => {} }
    let _ = (m, o, s, q, r, c, f(1), helper(2), !true, ~1, -a, takes(b: 1, a: 2))
    return "{s:{width}.{digits}f} {p.y}"
}
"#;
    let mut modules: Vec<skull_parser::Ast> = rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect();
    modules.push(parse_rock(src).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");

    let mut missed = Vec::new();
    for module in &ws.modules {
        for (owner, body) in module.bodies.iter() {
            let mut reached = FxHashSet::default();
            let roots = body
                .root
                .into_iter()
                .chain(body.param_defaults.iter().flatten().copied())
                .chain(body.field_defaults.values().copied());
            for root in roots {
                reached.insert(root);
                body.walk_descendants(HirChild::Expr(root), &mut |child| match child {
                    HirChild::Expr(e) => reached.insert(e),
                    HirChild::Statement(_) | HirChild::Pattern(_) => true,
                });
            }
            for (e, expr) in body.expr_arena.iter() {
                // Lowering an `if let` chain copies its outermost node into a
                // fresh slot and leaves the original behind, unreferenced;
                // its children are reached through the copy.
                let orphaned_copy =
                    matches!(expr, Expr::IfLet { value, .. } if reached.contains(value));
                if !reached.contains(&e) && !orphaned_copy {
                    missed.push(format!("{} {owner:?}: {expr:?}", module.file));
                }
            }
        }
    }
    assert!(
        missed.is_empty(),
        "unreached expressions:\n{}",
        missed.join("\n")
    );
}

// Overload scoring tries every candidate and rolls the substitutions back;
// the types it builds for the losers (fresh generics, substituted params)
// must not stay in the body's arena, which lives as long as the compile.
// Only the committed call instantiates its generic, so `main`'s arena holds
// exactly one inference variable.
#[test]
fn overload_scoring_leaves_no_types_in_the_body_arena() {
    use bone_semantic::hir::{BodyOwner, TypeExpr};

    let ws = check(
        "\
fn pick<T>(a: T, b: int) -> T { a }
fn pick<T>(a: T, b: string) -> T { a }
fn pick<T>(a: T, b: bool) -> T { a }
fn main() {
    let x = pick(1.5, true)
}",
    )
    .unwrap();
    let mid = ws.source_modules().next().expect("one user module").id;
    let (main, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "main")
        .unwrap();
    let bi = &ws.inference(mid).unwrap().per_fn[&BodyOwner::Function(main)];
    let vars = bi
        .ty_arena
        .iter()
        .filter(|(_, t)| matches!(t, TypeExpr::Infer(..)))
        .count();
    assert_eq!(vars, 1, "losing candidates left their types behind");
}
