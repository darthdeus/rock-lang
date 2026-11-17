// Structural tests: assertions on workspace/HIR/inference internals that a
// `.rock` fixture marker can't express (arena layouts, per-body bookkeeping,
// cross-module wiring, diagnostic spans). Type-level behavior belongs in
// tests/fixtures/*.rock instead.

use bone_core::modules::{NsAliases, PackageAliases};
use bone_semantic::prelude::*;
use rock_core::source_code::{IdentityStorage, PackageTag};
use skull_parser::{Dialect, parse_module_with_dialect};

fn parse_rock_with_file(src: &str, file: &str) -> Result<skull_parser::Ast, CompilerError> {
    skull_parser::parse_module_clean(src, Ustr::from(file))
}

fn parse_rock(src: &str) -> Result<skull_parser::Ast, CompilerError> {
    parse_rock_with_file(src, "<parse_rock>")
}

/// Parse + type-check one source as a standalone, identity-enforced module
/// (no stdlib).
fn check(src: &str) -> Result<Workspace, CompilerError> {
    let mut module = [parse_rock(src)?];
    bone_semantic::semantic_passes::semantic_pass_enforcing_identity_storage(
        &mut module,
        &NsAliases::default(),
    )
}

#[test]
fn standalone_inline_module_paths_have_an_implicit_package() {
    let mut module = parse_module_with_dialect(
        "mod helpers { pub fn answer() -> int { 42 } } fn main() -> int { helpers::answer() }",
        Ustr::from("standalone.rock"),
        Dialect::V1,
    )
    .unwrap();
    let ws = bone_semantic::semantic_passes::semantic_pass_enforcing_identity_storage(
        std::slice::from_mut(&mut module),
    )
    .expect("standalone module has an implicit package");
    assert!(ws.fragment_scopes.contains_key(&module.file));
    let errors = all_errors(&ws);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn standalone_files_do_not_share_root_definitions() {
    let mut modules = [
        parse_rock_with_file(
            "struct Item { n: int } fn f(x: Item) -> int { x.n }",
            "a.rock",
        )
        .unwrap(),
        parse_rock_with_file(
            "struct Item { s: string } fn f(x: Item) -> string { x.s }",
            "b.rock",
        )
        .unwrap(),
    ];
    let ws = semantic_pass(&mut modules).expect("standalone roots are independent");
    let a = &ws.fragment_scopes[&ustr("a.rock")];
    let b = &ws.fragment_scopes[&ustr("b.rock")];
    assert_ne!(a.package, b.package);
    assert_ne!(a.package_name, b.package_name);
    let errors = all_errors(&ws);
    assert!(errors.is_empty(), "{errors:?}");
}

fn stdlib_modules() -> Vec<skull_parser::Ast> {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| parse_rock_with_file(f.contents, &rock_stdlib::stdlib_file_id(f.name)).unwrap())
        .collect()
}

fn tag(id: u32, self_name: &str, identity_storage: IdentityStorage) -> PackageTag {
    PackageTag {
        id,
        self_name: self_name.to_string(),
        identity_storage,
        root: String::new(),
    }
}

/// Parse `src` as `file` under `dialect`, owned by package `id`/`name`.
fn parse_in(src: &str, file: &str, dialect: Dialect, package: (u32, &str)) -> skull_parser::Ast {
    let mut module = parse_module_with_dialect(src, Ustr::from(file), dialect)
        .unwrap_or_else(|e| panic!("{file}: {e}"));
    module.package = Some(tag(package.0, package.1, Default::default()));
    module
}

/// Each package reachable under its own self name, plus the listed aliases.
fn aliases(packages: &[(u32, &str, &[(&str, u32)])]) -> NsAliases {
    NsAliases {
        packages: packages
            .iter()
            .map(|(id, name, deps)| PackageAliases {
                id: *id,
                self_name: Ustr::from(name),
                aliases: deps.iter().map(|(n, id)| (Ustr::from(n), *id)).collect(),
            })
            .collect(),
    }
}

/// The index of the item named `$name` in a HIR arena.
macro_rules! named {
    ($arena:expr, $name:expr) => {
        $arena
            .iter()
            .find(|(_, item)| item.name.as_str().rsplit("::").next() == Some($name))
            .unwrap_or_else(|| panic!("`{}` exists", $name))
            .0
    };
}

fn user_mid(ws: &Workspace) -> ModuleId {
    ws.source_modules().next().expect("one user module").id
}

fn module_by_file(ws: &Workspace, file: &str) -> ModuleId {
    ws.modules
        .iter()
        .find(|m| m.file == ustr(file))
        .unwrap_or_else(|| panic!("module {file}"))
        .id
}

fn fn_owner(ws: &Workspace, name: &str) -> BodyOwner {
    BodyOwner::Function(named!(ws.module(user_mid(ws)).functions, name))
}

/// The body of the function named `name` in the single user module.
fn fn_body<'a>(ws: &'a Workspace, name: &str) -> &'a Body {
    &ws.module(user_mid(ws)).bodies[&fn_owner(ws, name)]
}

/// All errors recorded against the named function's body.
fn fn_errors<'a>(ws: &'a Workspace, name: &str) -> &'a [CompilerError] {
    ws.inference(user_mid(ws))
        .expect("user module inferred")
        .errors
        .get(&fn_owner(ws, name))
        .map_or(&[], Vec::as_slice)
}

/// The source slice a byte-offset span covers.
fn text<'a>(src: &'a str, span: &Span) -> &'a str {
    let (s, e) = span.offset_range();
    &src[s..e]
}

/// Every interface, lowering and body diagnostic in `ws`.
fn all_errors(ws: &Workspace) -> Vec<&CompilerError> {
    ws.modules
        .iter()
        .flat_map(|m| {
            m.interface_errors
                .iter()
                .chain(m.lowering_errors.values().flatten())
                .chain(
                    ws.inference(m.id)
                        .into_iter()
                        .flat_map(|inf| inf.errors.values().flatten()),
                )
        })
        .collect()
}

fn error_messages(ws: &Workspace) -> Vec<String> {
    all_errors(ws).iter().map(|e| e.message.clone()).collect()
}

#[test]
fn standalone_workspace_keeps_the_std_namespace() {
    let std_module = parse_module_with_dialect(
        "fn print() {}",
        Ustr::from("<stdlib>/fixture.rock"),
        Dialect::V1,
    )
    .expect("parse stdlib module");
    let user_module = parse_rock_with_file("fn main() {}", "main.rock").unwrap();
    let ws = semantic_pass(&mut [std_module, user_module]).expect("semantic pass");
    let std_root = ws
        .ns
        .package(u32::MAX)
        .and_then(|package| package.node(Ustr::from("")))
        .expect("stdlib namespace root");
    assert!(std_root.owned_callables.contains_key(&Ustr::from("print")));
    assert!(ws.fragment_scopes.contains_key(&ustr("main.rock")));
}

#[test]
fn an_unresolved_v1_variant_pattern_is_labeled_at_its_path() {
    let src =
        "fn f(n: int) -> int {\n    match n {\n        Nope::A => 1,\n        _ => 0,\n    }\n}";
    let module = parse_in(src, "app.rock", Dialect::V1, (1, "app"));
    let ws = semantic_pass_with_aliases(&mut [module], &aliases(&[(1, "app", &[])]))
        .expect("semantic pass");
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
    assert_eq!(text(src, &label.span), "Nope::A");
}

// The mismatch carries a second label at the expected type's origin (the
// `: Foo` annotation). Builtins (`int`) lower to a shared interned TyId with
// no per-site span, so only span-carrying types get one.
#[test]
fn type_mismatch_labels_the_expected_type_origin() {
    let src = "struct Foo { x: int }\nfn main() { let a: Foo = \"hello\" }";
    let ws = check(src).expect("semantic_pass");
    let err = fn_errors(&ws, "main")
        .iter()
        .find(|e| e.message.contains("expected `Foo`"))
        .expect("a type-mismatch error");
    assert!(
        err.labels.iter().any(|l| text(src, &l.span) == "Foo"),
        "expected a label at the `Foo` annotation; labels: {:?}",
        err.labels
    );
}

// Each diagnostic's primary label covers just the offending expression: the
// `/` subexpression for the float-division hint, the tail rather than the
// whole fn for a return mismatch, the hole for an interpolation error.
#[test]
fn diagnostics_label_the_offending_expression() {
    for (src, fn_name, message, labeled) in [
        (
            "@pure @inline operator(lua) /(a: float, b: float) -> float = \"return a / b\"\n\
             @pure @inline operator(lua) *(a: float, b: float) -> float = \"return a * b\"\n\
             fn main() { let x: int = 2.0 * (3 / 4) }",
            "main",
            "./",
            "(3 / 4)",
        ),
        ("fn f() -> string {\n    let y = 1\n    y\n}", "f", "", "y"),
        (
            "fn name(a: int) -> string {\n    return \"\"\n}\nfn f() -> string {\n    return \"x {name}\"\n}",
            "f",
            "cannot interpolate",
            "name",
        ),
        (
            "fn u() {\n}\nfn f() -> string {\n    return \"x {u()}\"\n}",
            "f",
            "cannot interpolate",
            "u()",
        ),
    ] {
        let ws = check(src).expect("recovered, not aborted");
        let err = fn_errors(&ws, fn_name)
            .iter()
            .find(|e| e.message.contains(message))
            .unwrap_or_else(|| panic!("{src}: no `{message}` error"));
        let label = err.labels.first().expect("labeled error");
        assert_eq!(text(src, &label.span), labeled, "{src}");
    }
}

// A statement-tailed body reports the missing value instead of leaking the
// internal `()` type, and never masks a genuine error in the body.
#[test]
fn missing_return_value_says_so_instead_of_unifying_unit() {
    let src = "fn f() -> string {\n    let y = 1\n    let z = 2\n}";
    let ws = check(src).expect("recovered, not aborted");
    let err = fn_errors(&ws, "f").first().expect("a return-type error");
    assert!(err.message.contains("no return value"), "{:?}", err.message);
    assert!(!err.message.contains("()"), "{:?}", err.message);
    let span = err.labels.first().expect("labeled error").span;
    assert!(
        text(src, &span).len() < src.len(),
        "underlines the whole fn"
    );

    let src = "fn f() -> string {\n    let w = undefined_symbol_here\n    let z = 1\n}";
    let ws = check(src).expect("recovered, not aborted");
    let errs = all_errors(&ws);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("undefined_symbol_here")),
        "the undefined-global error was masked: {errs:?}"
    );
}

#[test]
fn render_tuples() {
    use bone_semantic::hir::{BuiltinTy, Module, TyId, TypeExpr};
    let mut ws = Workspace::default();
    let mut m = Module {
        file: Ustr::from("test"),
        ..Default::default()
    };
    // Each compound's elements are contiguous and in order: inner's children
    // first, then the outer pair (outer_a, inner) allocated adjacently.
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
    assert_eq!(ws.render(None, TyId::Static(mid, inner)), "(int, bool)");
    assert_eq!(
        ws.render(None, TyId::Static(mid, outer)),
        "(int, (int, bool))"
    );
}

// The bug is type-invisible: `let c`'s inner block defines `let b`, and
// lowering must not let that inner statement leak into the OUTER block's
// statement range via the `first..=last` contiguity trap.
#[test]
fn nested_block_does_not_leak_into_outer_statements() {
    use bone_semantic::hir::Expr;
    let ws = check(
        "fn f() -> int {\n    let a = 1\n    let c = {\n        let b = 2\n        b\n    }\n    c\n}",
    )
    .unwrap();
    let fbody = fn_body(&ws, "f");
    let body = fbody.expr_arena[fbody.root.expect("rock function has a body")];
    let Expr::Block { statements, .. } = body else {
        panic!("function body should be a block, got {body:?}");
    };
    assert_eq!(
        statements.len(),
        2,
        "inner `let b` leaked into the outer block"
    );
}

// The same contiguity trap for parameters: the lambda default's own `x`
// binding is allocated between `f` and `k`, and must stay out of the fn's
// parameter range.
#[test]
fn lambda_param_default_does_not_leak_into_fn_params() {
    for (src, names) in [
        (
            "fn f(v: int, g: fn(int) -> int = x => x, k: int = 1) -> int { g(v) }",
            ["v", "g", "k"],
        ),
        (
            "fn f(v: int, k: int = 1, g: fn(int) -> int = x => x) -> int { g(v) }",
            ["v", "k", "g"],
        ),
    ] {
        let ws = check(src).unwrap_or_else(|e| panic!("{src}: {e:?}"));
        let errors = all_errors(&ws);
        assert!(errors.is_empty(), "{errors:?}");
        let fbody = fn_body(&ws, "f");
        let got: Vec<_> = fbody
            .param_bindings
            .map(|b| fbody.binding_arena[b].name.to_string())
            .collect();
        assert_eq!(got, names, "{src}");
    }
}

// The same contiguity trap for interpolation parts: the hole's `if` holds its
// own interpolated string, whose parts are allocated while the outer parts are
// being collected. A leaked inner `{x}` part printed the inner string twice.
#[test]
fn nested_interpolation_does_not_leak_into_outer_parts() {
    use bone_semantic::hir::{Expr, InterpPart};
    let ws = check(
        "fn f(x: int, ok: bool) -> string {\n    \"a: {if ok { \"{x}\" } else { \"no\" }}\"\n}",
    )
    .unwrap();
    let fbody = fn_body(&ws, "f");
    let body = fbody.expr_arena[fbody.root.expect("rock function has a body")];
    let Expr::Block {
        tail: Some(tail), ..
    } = body
    else {
        panic!("function body should be a block with a tail, got {body:?}");
    };
    let Expr::InterpolatedString(parts) = fbody.expr_arena[tail] else {
        panic!("tail should be an interpolated string");
    };
    let parts: Vec<_> = parts.map(|p| fbody.interp_part_arena[p]).collect();
    assert!(
        matches!(
            parts[..],
            [InterpPart::Literal(s), InterpPart::Value { expr, .. }]
                if s == "a: " && matches!(fbody.expr_arena[expr], Expr::If { .. })
        ),
        "inner interpolation parts leaked into the outer range: {parts:?}"
    );
}

// A type error in one body must not abort checking of the others: every
// owner kind collects its errors per BodyOwner instead of failing fast, so a
// watch/LSP session keeps healthy bodies' inference warm.
#[test]
fn body_type_errors_do_not_abort_other_bodies() {
    let ws = check(
        r#"struct __DecoTarget { name: string, params: [string], func: any }
struct Cfg { scale: int = "nope" }
global LIMIT: int = "too many"
deco init broken_deco {
    let s: int = target.name
}
fn broken_one() -> int { return true }
fn good() -> int { return 3 }
fn broken_two() -> bool { return 42 }"#,
    )
    .expect("body-level type errors must not abort semantic_pass");
    let mid = user_mid(&ws);
    let hir = ws.module(mid);
    let inf = ws.inference(mid).expect("user module inferred");
    let mut broken = vec![
        fn_owner(&ws, "broken_one"),
        fn_owner(&ws, "broken_two"),
        BodyOwner::Global(named!(hir.globals, "LIMIT")),
        BodyOwner::StructDefaults(named!(hir.structs, "Cfg")),
        BodyOwner::Decorator(named!(hir.decorators, "broken_deco")),
    ];
    let mut errored: Vec<_> = inf.errors.keys().copied().collect();
    broken.sort_by_key(|o| format!("{o:?}"));
    errored.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(errored, broken);
    assert!(
        inf.per_fn.contains_key(&fn_owner(&ws, "good")),
        "healthy body must have inference even when sibling bodies are broken"
    );
}

// Every item owns a Body -- "body exists <=> item exists" -- so Module.bodies
// covers every BodyOwner that pass 4 produces a result or error for. Owners
// with nothing to check (lua/extern fns, valueless globals, enums, newtypes,
// default-less structs) hold position-only bodies for hover. A valued owner's
// Body is never empty: an empty one means a lowering entry point built its
// local Body and then inserted a fresh default instead.
#[test]
fn every_item_owns_a_body_and_valued_bodies_carry_their_storage() {
    let ws = check(
        r#"struct __DecoTarget { name: string, params: [string], func: any }
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
fn rock_fn() -> int { return 3 }"#,
    )
    .unwrap();

    for (mid, inf) in ws.inferences_iter() {
        let hir = ws.module(mid);
        for owner in inf.per_fn.keys().chain(inf.errors.keys()) {
            assert!(
                hir.bodies.contains_key(owner),
                "{mid:?} {owner:?} was checked but has no Module.bodies entry"
            );
        }
    }

    let hir = ws.module(user_mid(&ws));
    let struct_owner = |name: &str| BodyOwner::StructDefaults(named!(hir.structs, name));
    let valued = [
        BodyOwner::Decorator(named!(hir.decorators, "d")),
        BodyOwner::Global(named!(hir.globals, "WITH_VALUE")),
        struct_owner("WithDefault"),
    ];
    for owner in valued {
        assert!(
            !hir.bodies[&owner].scope_arena.is_empty(),
            "{owner:?} has an empty Body"
        );
    }
    for owner in [
        fn_owner(&ws, "rock_fn"),
        fn_owner(&ws, "lua_fn"),
        BodyOwner::Global(named!(hir.globals, "NO_VALUE")),
        BodyOwner::Enum(named!(hir.enums, "Color")),
        BodyOwner::Newtype(named!(hir.newtypes, "Id")),
        struct_owner("NoDefault"),
    ]
    .into_iter()
    .chain(valued)
    {
        assert!(hir.bodies.contains_key(&owner), "{owner:?} has no Body");
    }

    // __DecoTarget also gets a StructDefaults body; the injected operator
    // prelude (__op_add, ...) adds one Function body each.
    let prelude_fns = hir
        .functions
        .iter()
        .filter(|(_, f)| {
            f.name
                .rsplit("::")
                .next()
                .unwrap_or(f.name.as_str())
                .starts_with("__op_")
        })
        .count();
    assert_eq!(
        hir.bodies.len(),
        10 + prelude_fns,
        "{:?}",
        hir.bodies.keys().collect::<Vec<_>>()
    );
}

// Scope storage is per-body: the nested fn's inner-block scope must land in
// its own Body, not a sibling's. Inequalities, not exact counts: how many
// scopes a fn allocates is lowering detail.
#[test]
fn scopes_land_in_their_own_body() {
    let ws = check(
        "fn nested() -> int {\n    let a = {\n        let b = 2\n        b\n    }\n    a\n}\n\
         fn flat() -> int { return 1 }",
    )
    .unwrap();
    let flat = fn_body(&ws, "flat").scope_arena.len();
    let nested = fn_body(&ws, "nested").scope_arena.len();
    assert!(flat >= 2, "default + root scopes, got {flat}");
    assert!(nested > flat, "nested {nested} vs flat {flat}");
}

// The module ty_arena is interface-only: `TyId::Static` is what other
// modules' cached inference holds. An in-body `let x: P` annotation lowers to
// body-local storage, so the module arena is IDENTICAL whether or not a body
// is annotated -- a body edit cannot perturb interface storage.
#[test]
fn body_annotations_do_not_touch_module_ty_arena() {
    let ty_arena_len = |annotation: &str| {
        let ws = check(&format!(
            "struct P {{ a: int }}\nfn f() -> int {{\n    let x{annotation} = P {{ a: 1 }}\n    return x.a\n}}"
        ))
        .unwrap();
        ws.module(user_mid(&ws)).ty_arena.len()
    };
    assert_eq!(
        ty_arena_len(": P"),
        ty_arena_len(""),
        "an in-body annotation leaked into the module (interface) ty_arena"
    );
}

// The "inferred twice" guard in `assign_expr_type` compares types
// STRUCTURALLY: two `(int, bool)` tuples at different arena indices are
// distinct `TyId`s but the same type. An expr is inferred once in the real
// walk, so this drives `assign_expr_type` directly.
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

    // Built in the gamma's OWN inference arena: TyIds minted in a throwaway
    // BodyInference are unresolvable by `gamma.get`.
    let mk = |gamma: &mut Gamma| -> TyId {
        let a = gamma.out.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Int));
        let b = gamma.out.ty_arena.alloc(TypeExpr::Builtin(BuiltinTy::Bool));
        TyId::Local(gamma.out.ty_arena.alloc(TypeExpr::Tuple {
            elements: IdxRange::new_inclusive(a..=b),
        }))
    };
    let t1 = mk(&mut gamma);
    let t2 = mk(&mut gamma);
    assert_ne!(t1, t2);

    let c = BodyCtx {
        module: ws.module(mid),
        body: &body,
    };
    gamma.assign_expr_type(c, e, t1).expect("first assignment");
    gamma
        .assign_expr_type(c, e, t2)
        .expect("structurally-equal reassign must not error");
}

// Calls, type names, field access and struct literals resolve against OTHER
// modules in the workspace, not just the current file.
#[test]
fn cross_module_items_resolve() {
    for b in [
        "fn f() -> int { helper(5) }",
        "fn f(p: Point) -> int { 5 }",
        "fn f(p: Point) -> int { p.x }",
        "fn f() -> Point { Point { x: 1, y: 2 } }",
    ] {
        let mod_a = parse_in(
            "fn helper(x: int) -> int { x }\nstruct Point { x: int, y: int }",
            "mod_a",
            Dialect::V1,
            (1, "shared"),
        );
        let mod_b = parse_in(b, "mod_b", Dialect::V1, (1, "shared"));
        let ws = semantic_pass(&mut [mod_a, mod_b]).expect("cross-module use should resolve");
        let errors = error_messages(&ws);
        assert!(errors.is_empty(), "{b}: {errors:?}");
    }
}

/// `TyId` packs to 8 bytes only while `ModuleId` carries a niche
/// (`NonMaxU32`); dropping it silently re-inflates every `TyId` to 12.
#[test]
fn tyid_stays_niche_packed() {
    assert_eq!(std::mem::size_of::<ModuleId>(), 4);
    assert_eq!(std::mem::size_of::<TyId>(), 8);
}

// Lowered holes and fresh inference metavars must never share a `VarId`:
// `Gamma::new` seeds `next_var_id` from `Module.lower_var_high_water`. If the
// spaces overlap, a Static hole and a Local fresh var collide on one
// `substitutions` key -- two different vars, one binding (unsound).
#[test]
fn fresh_metavar_ids_never_collide_with_lowered_holes() {
    let ws = check(
        "\
fn id<T>(x: T) -> T { return x }
fn pair<A, B>(a: A, b: B) -> A { return a }
fn holey(x: _) -> _ {
    let a: _ = id(1)
    let b: _ = pair(2, id(3))
    let c: _ = id(id(4))
    return x
}",
    )
    .expect("crafted holes+generics module type-checks");

    let mut collisions = Vec::new();
    for (mid, inf) in ws.inferences_iter() {
        let static_holes: FxHashSet<hir::VarId> = ws
            .module(mid)
            .ty_arena
            .iter()
            .filter_map(|(_, t)| match t {
                hir::TypeExpr::Infer(v, _) => Some(*v),
                _ => None,
            })
            .collect();
        for (owner, body) in &inf.per_fn {
            for (_, t) in body.ty_arena.iter() {
                if let hir::TypeExpr::Infer(v, _) = t {
                    if static_holes.contains(v) {
                        collisions.push((mid, *owner, v.0));
                    }
                }
            }
        }
    }
    assert!(collisions.is_empty(), "{collisions:?}");
}

// Declaration facts survive lowering: `ref` types, the nobox marker, float
// annotation args (`@test(returns = 1.5)` reads them), and param/field
// annotations (skull reads @size/@instance/@flat from HIR).
#[test]
fn declaration_details_survive_lowering() {
    use bone_semantic::hir::AnnoValue;
    let ws = check(
        "struct Player { hp: int }\n\
         fn identity(player: ref Player) -> ref Player { player }\n\
         nobox value V { @flat id: int, pos: float }\n\
         @test(returns = 1.5)\nfn a() -> float { return 1.5 }\n\
         @test(returns = -2.5)\nfn b() -> float { return -2.5 }\n\
         fn f(@size(32) xs: int, plain: int) -> int { return xs + plain }\n",
    )
    .expect("type checks");
    let module = ws.module(user_mid(&ws));
    let function = |name: &str| &module.functions[named!(module.functions, name)];

    let identity = function("identity");
    assert_eq!(ws.render(None, identity.param_decls[0].ty), "ref Player");
    assert_eq!(ws.render(None, identity.return_ty.unwrap()), "ref Player");

    let v = &module.structs[named!(module.structs, "V")];
    assert!(v.is_value && v.is_nobox);
    let field_annos: Vec<_> = v.fields.iter().map(|f| &f.annotations).collect();
    assert_eq!(field_annos[0].len(), 1);
    assert_eq!(field_annos[0][0].name.name.as_str(), "flat");
    assert!(field_annos[0][0].args.is_empty());
    assert!(field_annos[1].is_empty());

    for (name, want) in [("a", "Float(1.5)"), ("b", "Float(-2.5)")] {
        let args: Vec<_> = function(name).annotations[0]
            .args
            .iter()
            .map(|arg| format!("{:?}", arg.value))
            .collect();
        assert_eq!(args, [want]);
    }

    let f = function("f");
    let size = &f.param_decls[0].annotations;
    assert_eq!(size.len(), 1);
    assert_eq!(size[0].name.name.as_str(), "size");
    assert!(matches!(size[0].args[0].value, AnnoValue::Int(32)));
    assert!(f.param_decls[1].annotations.is_empty());
}

fn arena_borrow_errors(body: &str) -> Vec<String> {
    let mut modules = stdlib_modules();
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
    error_messages(&semantic_pass(&mut modules).expect("valid Arena borrow fixture"))
}

#[test]
fn arena_value_borrows_reject_detected_invalidation() {
    const HELD: &str = "global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 0 })\n";
    const RECURSIVE: &str = "fn one(n: int) -> ref BorrowCell { if n == 0 { return CELLS.borrow(HELD).unwrap() }; two(n - 1) }\n\
        fn two(n: int) -> ref BorrowCell { one(n) }\n\
        fn forward<T>(r: ref T) -> ref T { r }\n";
    let mut bodies: Vec<String> = [
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); CELLS.remove(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); invalidate(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { consume(CELLS.borrow(h).unwrap(), h) }",
        "fn probe(h: Entity<BorrowCell>) { later(CELLS.borrow(h).unwrap(), CELLS.remove(h)) }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); let alias: ref BorrowCell = r; invalidate(h); alias.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); r = CELLS.borrow(h).unwrap(); invalidate(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); opaque(); r.n = 1 }",
        // a lambda body and a local function body
        "fn probe(h: Entity<BorrowCell>) { let f = () => { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); invalidate(h); r.n = 1 }; f() }",
        "fn probe(h: Entity<BorrowCell>) { fn nested(k: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(k).unwrap(); invalidate(k); r.n = 1 } nested(h) }",
        // an invoked closure and an invoked local function
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); let f = () => invalidate(h); f(); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); fn nested(k: Entity<BorrowCell>) { invalidate(k) } nested(h); r.n = 1 }",
    ]
    .map(String::from)
    .into();
    // hidden returns, including an early one
    bodies.extend([
        "fn hidden() -> ref BorrowCell { return CELLS.borrow(HELD).unwrap() }\nfn probe() { let r: ref BorrowCell = hidden(); CELLS.remove(HELD); r.n = 1 }",
        "fn hidden(flag: bool) -> ref BorrowCell { if flag { return CELLS.borrow(HELD).unwrap() }; CELLS.borrow(HELD).unwrap() }\nfn probe() { let r: ref BorrowCell = hidden(true); CELLS.remove(HELD); r.n = 1 }",
    ].map(|b| format!("{HELD}{b}")));
    // return origins through generics and both recursive entry points
    bodies.extend(["one(1)", "two(1)", "forward(one(1))"].map(|origin| {
        format!("{HELD}{RECURSIVE}fn probe() {{ let r: ref BorrowCell = {origin}; CELLS.remove(HELD); r.n = 1 }}")
    }));
    let missed: Vec<_> = bodies
        .iter()
        .map(|body| (body, arena_borrow_errors(body)))
        .filter(|(_, errors)| !errors.iter().any(|e| e.contains("Arena value borrow")))
        .collect();
    assert!(missed.is_empty(), "missed Arena invalidation: {missed:?}");
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
        let operands: &[&str] = if section.name.starts_with("both_nullable") {
            &["x", "y"]
        } else if section.name.starts_with("nullable_right") {
            &["y"]
        } else {
            &["x"]
        };
        let labeled: Vec<_> = error
            .labels
            .iter()
            .inspect(|label| assert_eq!(label.message, "nullable operand"))
            .map(|label| text(section.content, &label.span))
            .collect();
        assert_eq!(labeled, operands, "{error:?}");
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
/// single visible candidate. A call with no fitting overload stays distinct
/// from one whose match is hidden.
#[test]
fn overload_visibility_follows_each_call() {
    const HIDDEN: &str = "the matching overload is not accessible";
    for (app, want) in [
        (
            "fn external() -> string { f(std::f(1.5)) }",
            Some(
                "No accessible overload for `f` with argument types (string); the matching overload is not accessible",
            ),
        ),
        (
            "fn external() -> string {\n    let n = f(1)\n    let s = std::f(1.5)\n    f(s)\n}",
            Some(HIDDEN),
        ),
        (
            "fn external() -> int {\n    let a = f(1)\n    let s = std::f(1.5)\n    let b = f(f(2))\n    let t: string = f(1.5)\n    f(a)\n}",
            None,
        ),
        ("fn external() -> int { g(g(1)) }", None),
        (
            "fn external() -> string { g(\"x\") }",
            Some(
                "No accessible overload for `lib::g` with argument types (string); the matching overload is not accessible",
            ),
        ),
        ("fn external() -> int { f(true) }", Some("No overload for")),
        ("fn external() -> int { g(true) }", Some("No overload for")),
        (
            "fn external() -> int { f(f(true)) }",
            Some("No overload for"),
        ),
    ] {
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
        let got = crate::test_support::check_types_embedded_with_stdlib(
            &crate::test_support::parse_embedded_files(&src),
            &[("<stdlib>/probe.rock", "fn f(x: float) -> string { \"\" }")],
        );
        match (want, got) {
            (None, Ok(())) => {}
            (Some(want), Err(error)) => {
                assert!(error.contains(want), "{app}: {error}");
                if want == "No overload for" {
                    assert!(!error.contains("not accessible"), "{app}: {error}");
                }
            }
            (want, got) => panic!("{app}: want {want:?}, got {got:?}"),
        }
    }
}

// A resolved call to a generic fn persists the winning candidate's
// instantiated generics in `call_generic_bindings` -- the binding source for
// skull's inliner call-site specialization.
#[test]
fn generic_call_records_instantiation_bindings() {
    let ws = check(
        r#"value V2 { x: float, y: float }
fn pick<T>(a: T, b: T) -> T { return a }
fn main() {
    let v = pick(V2 { x: 1.0, y: 2.0 }, V2 { x: 3.0, y: 4.0 })
    let i = pick(1, 2)
}"#,
    )
    .expect("clean check");
    let body = &ws
        .inference(user_mid(&ws))
        .expect("user module inferred")
        .per_fn[&fn_owner(&ws, "main")];
    let mut rendered: Vec<String> = body
        .call_generic_bindings
        .values()
        .map(|bindings| {
            assert_eq!(bindings.len(), 1, "pick has one generic param");
            ws.render(Some(body), bindings[0])
        })
        .collect();
    rendered.sort();
    assert_eq!(rendered, ["V2", "int"]);
}

/// The backend-variant collapse keeps each signature group together, in
/// order of first appearance. A group with a repeated backend is not
/// collapsed, so its members stay adjacent: the "available overloads" list
/// is in collapse order, not declaration order.
#[test]
fn backend_variant_groups_list_in_first_appearance_order() {
    let ws = check(
        r#"
inline(lua) fn g(x: int) -> int = """return x"""
fn g(x: string) -> int { return 0 }
inline(lua) fn g(x: int) -> int = """return x"""
extern(js) fn g(x: bool, y: int) -> int
fn main() {
    let _ = g(1.5)
}
"#,
    )
    .expect("semantic_pass");
    let expected = "No overload for `g` with argument types (float)\navailable overloads:\n  \
                    g(x: int) -> int\n  g(x: int) -> int\n  g(x: string) -> int\n  g(x: bool, y: int) -> int";
    let msgs = error_messages(&ws);
    assert!(msgs.iter().any(|m| m == expected), "{msgs:?}");
}

const ACTOR: &str = "pub struct Actor {\n    pub health: int,\n    pub private(set) mana: int,\n    private secret: int,\n}";

#[test]
fn external_field_access_enforces_visibility() {
    use bone_semantic::inference::{AccessDenied, FieldAccessNeed::*, field_access};

    let v1_mod = parse_in(ACTOR, "lib.rock", Dialect::V1, (1, "game"));
    let ws = semantic_pass_with_aliases(&mut [v1_mod], &aliases(&[(1, "game", &[])]))
        .expect("semantic pass");
    let body = Body::default();
    let gamma = Gamma::new(&ws, &body, module_by_file(&ws, "lib.rock"));

    let external = AccessSite::default();
    let in_package = AccessSite {
        package: Some(1),
        module_path: &[],
    };
    for (key, field, site, need, allowed) in [
        ("game::Actor", "health", external, Read, true),
        ("game::Actor", "health", external, Write, true),
        ("game::Actor", "mana", external, Read, true),
        ("game::Actor", "mana", external, Write, false),
        ("game::Actor", "secret", external, Read, false),
        ("game::Actor", "secret", external, Write, false),
        ("game::Actor", "mana", in_package, Write, true),
        ("game::Actor", "secret", in_package, Read, true),
        ("game::Actor", "secret", in_package, Write, true),
    ] {
        let got = field_access(&gamma, ustr(key), ustr(field), site, need);
        let ok = if allowed {
            got.is_ok()
        } else {
            matches!(got, Err(AccessDenied::Privilege { need: n, .. }) if n == need)
        };
        assert!(ok, "{key}.{field} {need:?} from {site:?}: {got:?}");
    }
}

#[test]
fn structural_interface_conformance_rejects_private_set_field_for_writable_interface() {
    use bone_semantic::inference::coerce_to_interface;

    let lib_mod = parse_in(ACTOR, "lib.rock", Dialect::V1, (1, "game"));
    let consumer_mod = parse_in(
        "pub struct WritableMana { pub mana: int }\n\
         pub struct WritableHealth { pub health: int }\n\
         pub struct MissingField { pub missing: int }\n\
         pub struct ReadableSecret { pub secret: int }",
        "consumer.rock",
        Dialect::V1,
        (2, "consumer"),
    );
    let mut ws = semantic_pass_with_aliases(
        &mut [lib_mod, consumer_mod],
        &aliases(&[(1, "game", &[]), (2, "consumer", &[("game", 1)])]),
    )
    .expect("semantic pass");
    for iface in [
        "WritableMana",
        "WritableHealth",
        "MissingField",
        "ReadableSecret",
    ] {
        ws.interface_names
            .insert(ustr(&format!("consumer::{iface}")));
    }

    let body = Body::default();
    let mut gamma = Gamma::new(&ws, &body, module_by_file(&ws, "consumer.rock"));
    let actor_ty = gamma.nominal_app(ustr("game::Actor"), &[], Span::unknown());
    let mut coerce = |iface: &str| {
        coerce_to_interface(
            &mut gamma,
            actor_ty,
            ustr(&format!("consumer::{iface}")),
            Span::unknown(),
        )
        .map_err(|e| e.message)
    };

    assert_eq!(coerce("WritableHealth"), Ok(()));
    assert_eq!(
        coerce("WritableMana").unwrap_err(),
        "field `mana` on struct `Actor` is not writable from this module"
    );
    // A genuinely absent source field gets the interface-specific diagnostic;
    // a present but inaccessible one keeps the access diagnostic.
    assert_eq!(
        coerce("MissingField").unwrap_err(),
        "type game::Actor does not have required field `missing` of interface `consumer::MissingField`"
    );
    assert_eq!(
        coerce("ReadableSecret").unwrap_err(),
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
    user_module.package = package.map(|name| tag(1, name, Default::default()));
    error_messages(&semantic_pass(&mut [std_module, user_module]).expect("semantic pass"))
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
    assert!(admitted.is_empty(), "{admitted:?}");

    for user in [
        // not a recursive waiver
        "struct Mob { hp: int }\nglobal MOBS: ArenaStore<[Mob]> = ArenaStore { generations: [], data: [] }",
        "struct Mob { hp: int }\nglobal MOBS: PlainStore<Mob> = PlainStore { data: [] }",
    ] {
        let errors = identity_storage_errors(arena, MINI_ARENA_STORE, Some("game"), user);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("cannot store identity struct `game::Mob`")),
            "{user}: {errors:?}"
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
    let map_field = identity_storage_errors(
        "<stdlib>/arena.rock",
        MINI_ARENA_STORE,
        Some("game"),
        "struct Mob { hp: int }\nstruct Rows {\n    @identity_storage\n    by_id: Map<int, Mob>,\n}",
    );
    assert_eq!(
        map_field,
        ["`@identity_storage` is reserved for the standard arena row store"],
    );
}

// A package's own declaration decides whether its sites are reported: the
// package name exempts nothing, and stdlib and unpackaged programs keep the
// compiler's built-in exemption.
#[test]
fn identity_storage_exemption_follows_the_package_declaration() {
    let user = "struct Mob { hp: int }\nstruct Room { mobs: [Mob] }\nfn count(held: [Mob]) -> int { return 0 }";
    let enforced = |errors: &[String]| {
        ["field `mobs`", "parameter `held`"].iter().all(|site| {
            errors
                .iter()
                .any(|e| e.contains(&format!("{site} cannot store identity struct `")))
        })
    };
    let undeclared =
        identity_storage_errors("<stdlib>/fixture.rock", "fn print() {}", Some("game"), user);
    assert!(enforced(&undeclared), "{undeclared:?}");
    let unpackaged = identity_storage_errors("<stdlib>/fixture.rock", "fn print() {}", None, user);
    assert!(unpackaged.is_empty(), "{unpackaged:?}");

    let declared = |identity_storage: IdentityStorage| {
        let mut module = parse_rock_with_file(user, "main.rock").expect("parse user module");
        module.package = Some(tag(1, "migrating", identity_storage));
        let ws = semantic_pass_into(Default::default(), &mut [module], &Default::default())
            .expect("semantic pass");
        error_messages(&ws)
    };
    let exempt = declared(IdentityStorage::Exempt);
    assert!(exempt.is_empty(), "{exempt:?}");
    let declared = declared(IdentityStorage::Enforced);
    assert!(enforced(&declared), "{declared:?}");
}

// A declared module exemption covers a qualified module path prefix of its own
// package, read from each file's `mod` header: the module it names and every
// module below it, never a sibling that shares its spelling or a file with no
// path.
#[test]
fn identity_storage_module_exemption_follows_the_module_path() {
    use bone_semantic::identity_storage::{IdentityStorageExemption, identity_storage_exemption};

    let files = [
        ("nav.rock", "pub mod nav;", "NavMob"),
        ("nav/graph.rock", "pub mod nav::graph;", "GraphMob"),
        ("navigation.rock", "pub mod navigation;", "NavigationMob"),
        ("world.rock", "pub mod world;", "WorldMob"),
        ("lib.rock", "", "RootMob"),
    ];
    let mob = |mob: &str| {
        format!("pub struct {mob} {{ hp: int }}\npub struct {mob}Room {{ mobs: [{mob}] }}\n")
    };
    let build = |game: IdentityStorage| {
        let mut modules: Vec<_> = files
            .iter()
            .map(|(file, header, name)| {
                parse_in(
                    &format!("{header}\n{}", mob(name)),
                    file,
                    Dialect::V1,
                    (1, "game"),
                )
            })
            .collect();
        modules.push(parse_in(
            "struct RootExtraMob { hp: int }\nstruct RootExtraMobRoom { mobs: [RootExtraMob] }\n",
            "unheaded.rock",
            Dialect::V1,
            (1, "game"),
        ));
        modules.push(parse_in(
            &format!("pub mod world;\n{}", mob("OtherMob")),
            "other/world.rock",
            Dialect::V1,
            (2, "other"),
        ));
        for module in &mut modules {
            if let Some(package) = module.package.as_mut().filter(|p| p.id == 1) {
                package.identity_storage = game.clone();
            }
        }
        modules[6].package.as_mut().unwrap().identity_storage = IdentityStorage::Enforced;
        semantic_pass_with_aliases(
            &mut modules,
            &aliases(&[(1, "game", &[]), (2, "other", &[])]),
        )
        .expect("semantic pass")
    };

    let ws = build(IdentityStorage::Enforced);
    let errors = error_messages(&ws);
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
    let exemption_of =
        |file: &str| identity_storage_exemption(&ws, ws.module(module_by_file(&ws, file)));
    let module = |name: &str| Some(IdentityStorageExemption::Module(ustr(name)));
    assert_eq!(exemption_of("nav.rock"), module("nav"));
    assert_eq!(exemption_of("nav/graph.rock"), module("nav"));
    assert_eq!(exemption_of("world.rock"), module("world"));
    // a prefix matches whole segments; a declaration covers its own package
    // only; the package root is not under `nav`
    assert_eq!(exemption_of("navigation.rock"), None);
    assert_eq!(exemption_of("other/world.rock"), None);
    assert_eq!(exemption_of("lib.rock"), None);
    // A file without a header belongs to the root, which matches no prefix.
    assert!(ws.fragment_scopes.contains_key(&ustr("unheaded.rock")));
    assert_eq!(exemption_of("unheaded.rock"), None);
    let errors = error_messages(&ws);
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

// Identity-to-`any` erasure is recorded where `coerce` accepts the `any`
// target, and a coercion inside a trial that is rolled back leaves no record:
// overload scoring and `as` classification try coercions they then undo.
#[test]
fn any_erasure_record_follows_trial_rollback() {
    use bone_semantic::inference::coerce;

    let ws = check("struct Mob { hp: int }\nfn main() { }").expect("semantic pass");
    let body = Body::default();
    let mut gamma = Gamma::new(&ws, &body, user_mid(&ws));
    let mob = gamma.nominal_app(ustr("Mob"), &[], Span::unknown());
    let any = ws.builtins.any();

    let snap = gamma.out.checkpoint();
    assert!(gamma.trial(|g| coerce(g, mob, any, Span::unknown()).is_ok()));
    assert_eq!(gamma.out.any_erasures.len(), 1, "accepted erasure recorded");
    gamma.out.rollback_to(snap);
    assert!(
        gamma.out.any_erasures.is_empty(),
        "rolled-back trial kept it"
    );

    coerce(&mut gamma, mob, any, Span::unknown()).expect("identity coerces to any at inference");
    assert_eq!(
        gamma.out.any_erasures.len(),
        1,
        "committed coercion lost it"
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
    let inference = &ws.inference(user_mid(&ws)).unwrap().per_fn[&fn_owner(&ws, "f")];
    assert_eq!(inference.for_iter_protocol.len(), 1);
    assert!(
        inference
            .for_iter_protocol
            .values()
            .all(|info| info.iter.is_none())
    );
}

fn annotation_resolution_errors(sources: &[(&str, &str)]) -> Vec<String> {
    let mut modules: Vec<_> = sources
        .iter()
        .map(|(file, source)| parse_in(source, file, Dialect::V1, (1, "sample")))
        .collect();
    match semantic_pass_with_aliases(&mut modules, &aliases(&[(1, "sample", &[])])) {
        Err(error) => vec![error.message],
        Ok(ws) => error_messages(&ws),
    }
}

#[test]
fn annotations_resolve_like_other_names() {
    let ambiguous = annotation_resolution_errors(&[
        ("a.rock", "mod a; pub deco init test {}"),
        ("b.rock", "mod b; pub deco init test {}"),
        ("main.rock", "use a::*; use b::*; @test fn probe() {}"),
    ]);
    assert!(
        ambiguous
            .iter()
            .any(|error| error.contains("test") && error.contains("ambiguous")),
        "an ambiguous annotation does not select the builtin: {ambiguous:?}"
    );

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

    // A custom decorator spelled like a builtin is not the builtin.
    let custom = annotation_resolution_errors(&[(
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
        custom
            .iter()
            .any(|error| error.contains("relocatable Vec or Map storage")),
        "{custom:?}"
    );

    for valid in [
        &[
            ("a.rock", "mod a; pub deco init test {}"),
            ("main.rock", "use a::*; @test fn probe() {}"),
        ][..],
        &[(
            "main.rock",
            "deco init wire {} fn changed() {} @wire(onchange=[changed]) fn probe() {}",
        )],
    ] {
        let errors = annotation_resolution_errors(valid);
        assert!(errors.is_empty(), "{valid:?}: {errors:?}");
    }
}

#[test]
fn standalone_struct_can_shadow_a_stdlib_name() {
    let mut modules = stdlib_modules();
    let source = "struct Scene { name: string }\nfn main() { let s = Scene { name: \"hi\" } print(s.name) }\n";
    modules.push(parse_rock_with_file(source, "user.rock").expect("user parses"));
    let ws = semantic_pass(&mut modules).expect("semantic pass");
    let errors = all_errors(&ws);
    assert!(errors.is_empty(), "{errors:?}");
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
    let mut modules = stdlib_modules();
    modules.push(parse_rock(src).expect("parse"));
    let ws = semantic_pass(&mut modules).expect("semantic_pass");

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
// the types it builds for the losers must not stay in the body's arena, which
// lives as long as the compile. Only the committed call instantiates its
// generic, so `main`'s arena holds exactly one inference variable.
#[test]
fn overload_scoring_leaves_no_types_in_the_body_arena() {
    use bone_semantic::hir::TypeExpr;

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
    let bi = &ws.inference(user_mid(&ws)).unwrap().per_fn[&fn_owner(&ws, "main")];
    let vars = bi
        .ty_arena
        .iter()
        .filter(|(_, t)| matches!(t, TypeExpr::Infer(..)))
        .count();
    assert_eq!(vars, 1, "losing candidates left their types behind");
}

// Enum variant uses carry the spans of their qualifier and variant name
// tokens, so LSP rename rewrites exactly the name at every use.
#[test]
fn enum_variant_uses_carry_name_token_spans() {
    use bone_semantic::hir::{Expr, Pattern};

    let src = "\
enum Color { Red, Green }
enum Shape { Rect { w: int }, Circle(int) }
fn f(c: Color, s: Shape) -> int {
    let x = Color::Red
    let a = match c {
        Color::Red => 1,
        Color::Green => 2,
    }
    return match s {
        Shape::Rect { w } => w,
        Shape::Circle(r) => r,
    }
}";
    let ws = check(src).expect("semantic_pass");
    let body = fn_body(&ws, "f");

    let (enum_name_span, variant_span) = body
        .expr_arena
        .values()
        .find_map(|e| match e {
            Expr::EnumVariant {
                variant,
                enum_name_span,
                variant_span,
                ..
            } if variant.as_str() == "Red" => Some((*enum_name_span, *variant_span)),
            _ => None,
        })
        .expect("an Expr::EnumVariant for Color::Red");
    assert_eq!(text(src, &body.name_spans[enum_name_span]), "Color");
    assert_eq!(text(src, &body.name_spans[variant_span]), "Red");

    let pattern_spans = |want: &str| {
        body.pattern_arena
            .values()
            .find_map(|p| match p {
                Pattern::EnumVariant {
                    variant,
                    enum_name_span,
                    variant_span,
                    ..
                } if variant.as_str() == want => Some((*enum_name_span, Some(*variant_span))),
                Pattern::Record {
                    name: Some(name),
                    enum_name_span,
                    name_span,
                    ..
                } if name.as_str() == want => Some((*enum_name_span, *name_span)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("a pattern for {want}"))
    };
    for (variant, qualifier) in [("Green", "Color"), ("Circle", "Shape"), ("Rect", "Shape")] {
        let (qualifier_span, variant_span) = pattern_spans(variant);
        assert_eq!(text(src, &qualifier_span.unwrap()), qualifier);
        assert_eq!(text(src, &variant_span.unwrap()), variant);
    }
}

// A v1 macro is expansion-owned: it is not a callable family member nor a
// definition, while ordinary overloads beside it stay one callable family.
#[test]
fn expanded_macros_are_not_callable_definitions() {
    use bone_core::modules::DefKind;
    use bone_semantic::semantic_passes::build_namespaces;

    const SOURCE: &str = include_str!("fixtures/cw7_macro_namespace_kind.rock");
    let provider = parse_in(SOURCE, "provider.rock", Dialect::V1, (1, "provider"));
    assert!(
        provider.recovery_errors.is_empty(),
        "{:?}",
        provider.recovery_errors
    );
    let ordinary_count = provider
        .items
        .iter()
        .filter(|item| {
            matches!(&item.kind, skull_parser::ItemKind::Function(f) if f.name.name == ustr("ordinary"))
        })
        .count();
    assert!(
        ordinary_count > 1,
        "fixture must exercise an overload family"
    );
    let aliases = aliases(&[(1, "provider", &[])]);
    let mut modules = vec![provider];
    let mut ws = Workspace::default();
    build_namespaces(&mut ws, &modules, &aliases).expect("build namespace");
    let root = ws
        .ns
        .package(1)
        .and_then(|pkg| pkg.node(ustr("")))
        .expect("provider namespace root");
    assert!(!root.owned_callables.contains_key(&ustr("identity_value")));
    assert!(!root.callable_members.contains_key(&ustr("identity_value")));
    assert!(
        !ws.defs
            .values()
            .any(|meta| meta.name == ustr("identity_value"))
    );
    let ordinary = root.owned_callables[&ustr("ordinary")];
    assert_eq!(ws.defs[&ordinary].kind, DefKind::Callable);
    assert_eq!(
        root.callable_members[&ustr("ordinary")].len(),
        ordinary_count
    );
    let checked = semantic_pass_with_aliases(&mut modules, &aliases)
        .expect("macro expansion and ordinary overloads still lower");
    let errors = error_messages(&checked);
    assert!(errors.is_empty(), "{errors:?}");

    let stdlib = skull_parser::parse_module_with_dialect(
        SOURCE,
        ustr("<stdlib>/macro_provider.rock"),
        Dialect::V1,
    )
    .expect("parse stdlib provider");
    let mut ws = Workspace::default();
    build_namespaces(&mut ws, &[stdlib], &NsAliases::default()).expect("stdlib namespace");
    let root = ws.ns.std_root().expect("stdlib root");
    assert!(!root.owned_callables.contains_key(&ustr("identity_value")));
    assert!(root.owned_callables.contains_key(&ustr("ordinary")));
}
