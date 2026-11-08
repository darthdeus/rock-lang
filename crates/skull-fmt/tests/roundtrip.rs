//! Round-trip checks for specific forms: formatting keeps the AST, reaches a
//! fixed point and reparses cleanly.

use rock_core::source_code::Dialect;

#[test]
fn nullable_function_returning_nullable_value_keeps_its_outer_parens() {
    let source = "struct Hooks {\n    callback: (() -> Vec2?)? = null,\n}\n";
    let formatted = skull_fmt::format_code(source);
    let ast = skull_parser::parse_module_clean(&formatted, "<roundtrip>".into())
        .expect("formatted nullable callback must parse");
    assert!(ast.recovery_errors.is_empty(), "{formatted}");
    assert!(formatted.contains("(() -> Vec2?)?"), "{formatted}");
}

#[test]
fn nullable_parenthesized_function_type_roundtrips_as_the_same_ast() {
    let source = "struct Hooks {\n    on_close: (fn())? = null,\n}\n";
    let source_ast = skull_parser::parse_module_clean(source, "<source>".into())
        .expect("the nullable parenthesized function type must parse");
    let formatted = skull_fmt::format_code(source);
    let formatted_ast = skull_parser::parse_module_clean(&formatted, "<formatted>".into())
        .expect("formatted output must parse");

    assert_eq!(
        source_ast.module_sexp(),
        formatted_ast.module_sexp(),
        "formatting `(fn())?` must preserve its nullable function-type AST; output was:\n{formatted}"
    );
    assert_eq!(
        formatted.trim(),
        "struct Hooks {\n    on_close: (fn() -> ())? = null,\n}"
    );
}

/// Modules-v1 forms round-trip under the v1 dialect: format(format(x)) == f
/// format(x), the output reparses clean, and — the load-bearing check — a
/// v1 ascription prints as `(expr : Type)`, which still parses in v1
/// (an infix re-print would not).
#[test]
fn modules_v1_forms_roundtrip() {
    let sources = [
        "fn f() {\n    let x = (5 : int)\n    let y = (x : float) / 2\n    print(y)\n}\n",
        "fn f() {\n    let g = ((v => v + 1) : fn(int) -> int)\n    print(g(1))\n}\n",
        "fn f() {\n    let t = ((1, 2) : (int, int))\n    print(t)\n}\n",
        "struct S {\n    name: string,\n    private(set) health: float = 100,\n    pub field: int,\n}\n",
        "pub mod combat {\n    pub fn heal() {}\n}\n",
        "mod a {\n    pub fn f() {}\n}\n\nmod b {\n    pub fn g() {}\n}\n",
        "use a::b::*;\nuse a::b::{Actor, heal as restore};\nuse geo::operator(+);\n\nfn f() {}\n",
        "pub @inline fn a() -> int { 1 }\n\npub(package) @pure\nfn b() -> int {\n    2\n}\n",
    ];
    for src in sources {
        let cfg = skull_fmt::FormatConfig::default();
        let once = skull_fmt::try_format_with_dialect(src, &cfg, Dialect::V1)
            .unwrap_or_else(|e| panic!("format failed for {src:?}: {e}"));
        let twice = skull_fmt::try_format_with_dialect(&once, &cfg, Dialect::V1)
            .unwrap_or_else(|e| panic!("reformat failed for {src:?}: {e}"));
        assert_eq!(once, twice, "format must be a fixed point for {src:?}");
        // The formatted output must reparse clean under v1.
        let ast = skull_parser::parse_module_with_dialect(
            &once,
            ustr::Ustr::from("rt.rock"),
            Dialect::V1,
        )
        .expect("formatted output must parse");
        assert!(
            ast.recovery_errors.is_empty(),
            "formatted output has recovery errors: {:?}\n{}",
            ast.recovery_errors,
            once
        );
    }
}

/// A visibility ahead of an item's annotations survives formatting.
#[test]
fn v1_visibility_on_an_annotated_item_is_kept() {
    let cfg = skull_fmt::FormatConfig::default();
    let src = "pub @inline fn a() -> int { 1 }\n\npub(package) @pure\nfn b() -> int {\n    2\n}\n";
    let plain = "pub fn c() -> int {\n    3\n}\n\npub struct S {\n    x: int,\n    pub(package) y: int,\n    pub private(set) z: int = 0,\n}\n\nprivate global G: int = 1\n\npub(in a::b) @inline inline(lua) fn d() -> int = \"\"\"return 1\"\"\"\n";
    assert_eq!(
        skull_fmt::try_format_with_dialect(plain, &cfg, Dialect::V1).expect("format"),
        plain
    );
    let out = skull_fmt::try_format_with_dialect(src, &cfg, Dialect::V1).expect("format");
    assert_eq!(out, src);
}

/// A v1 ascription must NOT print as the infix `::` form: that would not
/// reparse in v1.
#[test]
fn v1_ascription_prints_parenthesized() {
    let src = "fn f() {\n    let x = (5 : int)\n    print(x)\n}\n";
    let cfg = skull_fmt::FormatConfig::default();
    let out = skull_fmt::try_format_with_dialect(src, &cfg, Dialect::V1).expect("format");
    assert!(
        out.contains("(5 : int)"),
        "expected parenthesized spelling, got:\n{out}"
    );
    assert!(
        !out.contains("5 :: int"),
        "infix spelling leaked into v1 output:\n{out}"
    );
}

#[test]
fn closure_followed_by_another_argument_reaches_a_fixed_point() {
    for source in [
        "fn run() { invoke(() => { action() }, enabled: true) }",
        "fn run() { invoke(() => { first(); second() }, enabled: true) }",
        "fn run() { let result = invoke_callback(() => { perform_operation(context, configuration) }, report_failure) }",
        "fn run() { let result = invoke_callback(() => { perform_operation(context, configuration) }, enabled: true) }",
    ] {
        let original = skull_parser::parse_module_clean(source, "<source>".into()).unwrap();
        for max_line_length in [40, 100] {
            let config = skull_fmt::FormatConfig {
                max_line_length,
                ..Default::default()
            };
            let once = skull_fmt::try_format(source, &config).expect("format source");
            let twice = skull_fmt::try_format(&once, &config).expect("reparse formatted source");
            assert_eq!(once, twice, "source: {source}");
            let formatted = skull_parser::parse_module_clean(&once, "<formatted>".into()).unwrap();
            assert_eq!(original.module_sexp(), formatted.module_sexp());
        }
    }
}

/// A construct written on one source line whose children render across lines
/// (a multi-statement block, a block lambda) must lay out the way the next
/// pass reads the now multi-line source.
#[test]
fn single_line_containers_with_multiline_children_reach_a_fixed_point() {
    for source in [
        "fn f() {\n    print({ let x = 5; x * 2 })\n}\n",
        "fn f() {\n    print({ let x = 5; x * 2 } + { let y = 3; y + 1 })\n}\n",
        "fn f() {\n    print(if { let r = true; r } { 1 } else { 2 })\n}\n",
        "fn f() {\n    print({})\n}\n",
        "fn f() -> C {\n    return C { bump: () => { n = n + 1; return n }, tag: 1 }\n}\n",
        "fn f() -> P {\n    return P { get: () => n, bump: () => { n = n + 1; return n } }\n}\n",
        "fn f() {\n    let q = F { a + b: { c = c + 1; 10 }, d: 2 }\n}\n",
        "fn f() {\n    for x in v.iter().map(x => { t.push(x); x }).filter(x => { u(); true }) {\n        print(x)\n    }\n}\n",
        "fn f() -> P {\n    return P { get: () => n, bump: () => {\n        n = n + 1\n        return n\n    } }\n}\n",
    ] {
        let original = skull_parser::parse_module_clean(source, "<source>".into()).unwrap();
        let config = skull_fmt::FormatConfig::default();
        let once = skull_fmt::try_format(source, &config).expect("format source");
        let twice = skull_fmt::try_format(&once, &config).expect("reparse formatted source");
        assert_eq!(once, twice, "source: {source}");
        let formatted = skull_parser::parse_module_clean(&once, "<formatted>".into()).unwrap();
        assert_eq!(original.module_sexp(), formatted.module_sexp(), "{once}");
    }
}

/// Call splats print in the prefix form the parser accepts (`f(...rest)`).
#[test]
fn call_splat_roundtrips() {
    for source in [
        "macro sum_of(x: expr, rest: ...expr) -> int {\n    x + sum_of(...rest)\n}\n",
        "macro fwd(rest: ...expr) -> int { sum_of!(...rest) }\n",
        "fn f(xs: ...int) {\n    g(1, ...xs)\n}\n",
    ] {
        let original = skull_parser::parse_module_clean(source, "<source>".into()).unwrap();
        let once = skull_fmt::format_code(source);
        let formatted = skull_parser::parse_module_clean(&once, "<formatted>".into())
            .unwrap_or_else(|e| panic!("formatted splat must parse: {e:?}\n{once}"));
        assert!(formatted.recovery_errors.is_empty(), "{once}");
        assert_eq!(original.module_sexp(), formatted.module_sexp(), "{once}");
        assert_eq!(once, skull_fmt::format_code(&once));
    }
}
