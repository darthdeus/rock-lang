// Structural tests: assertions on workspace/HIR/inference internals that a
// `.rock` fixture marker can't express (arena layouts, per-body bookkeeping,
// cross-module wiring, diagnostic spans). Type-level behavior belongs in
// tests/fixtures/*.rock instead.

use crate::test_support::{diagnostic_messages, diagnostics};
use bone_core::modules::{NsAliases, PackageAliases};
use bone_semantic::prelude::*;
use rock_core::source_code::{IdentityStorage, PackageTag};
use skull_parser::parse_module;

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

/// Parse `src` as `file` owned by package `id`/`name`.
fn parse_in(src: &str, file: &str, package: (u32, &str)) -> skull_parser::Ast {
    let mut module = parse_module(src, Ustr::from(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
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

#[test]
fn an_unresolved_v1_variant_pattern_is_labeled_at_its_path() {
    let src =
        "fn f(n: int) -> int {\n    match n {\n        Nope::A => 1,\n        _ => 0,\n    }\n}";
    let module = parse_in(src, "app.rock", (1, "app"));
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
            "enum E { V { n: int } }\nfn impure() -> int { 1 }\n@pure fn f() -> E { E::V { n: impure() } }",
            "f",
            "not provably pure",
            "impure()",
        ),
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

#[test]
fn any_interpolation_errors_label_the_hole() {
    for src in [
        "fn f(value: any) -> string { return \"{value}\" }",
        "fn f(value: any) -> string { return \"{value:.2f}\" }",
        "fn f(value: any?) -> string { return \"{value}\" }",
    ] {
        let ws = check(src).expect("semantic pass recovers with a body error");
        let err = fn_errors(&ws, "f")
            .iter()
            .find(|e| e.message.contains("cannot interpolate"))
            .unwrap_or_else(|| panic!("{src}: no interpolation error"));
        let label = err.labels.first().expect("the hole is labeled");
        assert_eq!(text(src, &label.span), "value", "{src}");
    }
}

// A statement-tailed body reports the missing value instead of leaking the
// internal `()` type, and never masks a genuine error in the body.
#[test]
fn missing_return_value_says_so_instead_of_unifying_unit() {
    let src = "fn f() -> string {\n    let y = 1\n    let z = 2\n}";
    let ws = check(src).expect("recovered, not aborted");
    let err = fn_errors(&ws, "f").first().expect("a return-type error");
    let span = err.labels.first().expect("labeled error").span;
    assert!(
        text(src, &span).len() < src.len(),
        "underlines the whole fn"
    );

    let src = "fn f() -> string {\n    let w = undefined_symbol_here\n    let z = 1\n}";
    let ws = check(src).expect("recovered, not aborted");
    let errs = diagnostics(&ws);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("undefined_symbol_here")),
        "the undefined-global error was masked: {errs:?}"
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
        let errors = diagnostics(&ws);
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
    diagnostic_messages(&semantic_pass(&mut modules).expect("valid Arena borrow fixture"))
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

const ACTOR: &str = "pub struct Actor {\n    pub health: int,\n    pub private(set) mana: int,\n    private secret: int,\n}";

#[test]
fn structural_interface_conformance_rejects_private_set_field_for_writable_interface() {
    use bone_semantic::inference::coerce_to_interface;

    let lib_mod = parse_in(ACTOR, "lib.rock", (1, "game"));
    let consumer_mod = parse_in(
        "pub struct WritableMana { pub mana: int }\n\
         pub struct WritableHealth { pub health: int }\n\
         pub struct MissingField { pub missing: int }\n\
         pub struct ReadableSecret { pub secret: int }",
        "consumer.rock",
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
    diagnostic_messages(&semantic_pass(&mut [std_module, user_module]).expect("semantic pass"))
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
        diagnostic_messages(&ws)
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
    use bone_semantic::identity_storage::identity_storage_exempt;

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
                parse_in(&format!("{header}\n{}", mob(name)), file, (1, "game"))
            })
            .collect();
        modules.push(parse_in(
            "struct RootExtraMob { hp: int }\nstruct RootExtraMobRoom { mobs: [RootExtraMob] }\n",
            "unheaded.rock",
            (1, "game"),
        ));
        modules.push(parse_in(
            &format!("pub mod world;\n{}", mob("OtherMob")),
            "other/world.rock",
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
    let errors = diagnostic_messages(&ws);
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
        |file: &str| identity_storage_exempt(&ws, ws.module(module_by_file(&ws, file)));
    assert!(exemption_of("nav.rock"));
    assert!(exemption_of("nav/graph.rock"));
    assert!(exemption_of("world.rock"));
    // a prefix matches whole segments; a declaration covers its own package
    // only; the package root is not under `nav`
    assert!(!exemption_of("navigation.rock"));
    assert!(!exemption_of("other/world.rock"));
    assert!(!exemption_of("lib.rock"));
    // A file without a header belongs to the root, which matches no prefix.
    assert!(ws.fragment_scopes.contains_key(&ustr("unheaded.rock")));
    assert!(!exemption_of("unheaded.rock"));
    let errors = diagnostic_messages(&ws);
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

fn annotation_resolution_errors(sources: &[(&str, &str)]) -> Vec<String> {
    let mut modules: Vec<_> = sources
        .iter()
        .map(|(file, source)| parse_in(source, file, (1, "sample")))
        .collect();
    match semantic_pass_with_aliases(&mut modules, &aliases(&[(1, "sample", &[])])) {
        Err(error) => vec![error.message],
        Ok(ws) => diagnostic_messages(&ws),
    }
}

#[test]
fn misspelled_struct_annotation_is_rejected() {
    for source in [
        "@consuem struct Owned { item: int }",
        "mod inner { @consuem struct Owned { item: int } }",
    ] {
        let errors = annotation_resolution_errors(&[("main.rock", source)]);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("unknown annotation `@consuem`")),
            "{source}: {errors:?}"
        );
    }
}

#[test]
fn nested_decorator_spelled_consume_is_not_the_builtin() {
    let errors = annotation_resolution_errors(&[(
        "main.rock",
        "mod inner { deco init consume {} @consume struct Owned { item: int } }",
    )]);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("annotation `@consume` is not allowed on a struct")),
        "{errors:?}"
    );
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
