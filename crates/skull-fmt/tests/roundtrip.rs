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

/// The v1 `mod a::b;` file header keeps its `;` and is followed by one
/// blank line.
#[test]
fn v1_file_header_keeps_its_semicolon() {
    let cfg = skull_fmt::FormatConfig::default();
    for src in [
        "mod main;\n\nuse super::helpers::shout;\n\nfn main() {\n    print(shout(\"hi\"))\n}\n",
        "pub mod a::b;\n\n// lead\nfn f() {\n    g()\n}\n",
        "private mod secret;\n\npub fn f() {\n    g()\n}\n",
    ] {
        let once = skull_fmt::try_format_with_dialect(src, &cfg, Dialect::V1)
            .unwrap_or_else(|e| panic!("format failed for {src:?}: {e}"));
        assert_eq!(once, src);
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
        "fn f() {\n    x = [{ y: {} }]\n}\n",
        "fn f() {\n    let a = [1, { let x = 5; x * 2 }]\n}\n",
        "fn f() {\n    let t = ({ let x = 5; x * 2 }, 1)\n}\n",
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

/// Statements sharing a line are sliced verbatim to the end of that line; a
/// multi-line statement whose last line starts another statement must not
/// copy that statement into the slice.
#[test]
fn shared_line_group_does_not_duplicate_the_next_statement() {
    for source in [
        "fn f() {\n    () let\n    y 13\n}\n",
        "fn f() {\n    a(); b(() => {\n        c()\n    }); d()\n}\n",
    ] {
        let original = skull_parser::parse_module_clean(source, "<source>".into()).unwrap();
        let once = skull_fmt::format_code(source);
        let twice = skull_fmt::format_code(&once);
        assert_eq!(once, twice, "source: {source}");
        let formatted = skull_parser::parse_module_clean(&once, "<formatted>".into()).unwrap();
        assert_eq!(original.module_sexp(), formatted.module_sexp(), "{once}");
    }
}

/// A statement split onto its own line keeps code that follows it on the
/// source line out of its slice, and keeps the `;` when the next statement
/// starts with an operator that would otherwise continue it (`a; -b`).
#[test]
fn split_statements_keep_their_boundaries() {
    for source in [
        "fn f(){({y 7;&  y})}fn g(){}",
        "fn f() {\n    a(); -b\n}\n",
        "fn f() {\n    x = 7; .y = 1\n}\n",
        "fn f() {\n    x = 7; -q // note\n}\n",
    ] {
        let original = skull_parser::parse_module_clean(source, "<source>".into()).unwrap();
        let once = skull_fmt::format_code(source);
        let twice = skull_fmt::format_code(&once);
        assert_eq!(once, twice, "source: {source}");
        let formatted = skull_parser::parse_module_clean(&once, "<formatted>".into())
            .unwrap_or_else(|e| panic!("formatted output must parse: {e:?}\n{once}"));
        assert!(formatted.recovery_errors.is_empty(), "{once}");
        assert_eq!(original.module_sexp(), formatted.module_sexp(), "{once}");
    }
}

/// Explicit call type arguments and one-element tuples survive formatting.
#[test]
fn call_type_arguments_and_single_tuples_are_kept() {
    for source in [
        "fn f() {\n    let nb = native_buffer_alloc<WideLeaf>(1)\n}\n",
        "fn f() {\n    let k = key<T>()\n}\n",
        "fn f() {\n    let m = Map<string, int>()\n}\n",
        "fn f() {\n    let r = a.b<int>(1).c(2).d()\n}\n",
        "fn f() {\n    let r = a.b(1).c<int>(2).d()\n}\n",
        "fn f() {\n    let single = (5,)\n}\n",
    ] {
        let original = skull_parser::parse_module_clean(source, "<source>".into()).unwrap();
        let once = skull_fmt::format_code(source);
        let formatted = skull_parser::parse_module_clean(&once, "<formatted>".into()).unwrap();
        assert_eq!(original.module_sexp(), formatted.module_sexp(), "{once}");
        assert_eq!(once, skull_fmt::format_code(&once));
    }
}

/// A unary minus over a negative operand must not fuse into `--`.
#[test]
fn unary_minus_over_a_negative_operand_keeps_its_space() {
    for source in [
        "fn f() {\n    return - -1\n}\n",
        "fn f() {\n    let x = sin(- -1.3) - - --y\n}\n",
    ] {
        let original = skull_parser::parse_module_clean(source, "<source>".into()).unwrap();
        let once = skull_fmt::format_code(source);
        let formatted = skull_parser::parse_module_clean(&once, "<formatted>".into()).unwrap();
        assert_eq!(original.module_sexp(), formatted.module_sexp(), "{once}");
        assert_eq!(once, skull_fmt::format_code(&once));
    }
}

/// A tight `..` must not fuse with a neighbouring `.` into `...`.
#[test]
fn range_operator_next_to_a_dot_keeps_its_spaces() {
    for source in [
        "fn f() {\n    print(src .. .w.f0)\n}\n",
        "fn f() {\n    let r = a..b\n}\n",
        "fn f() {\n    sum2(.. .p)\n}\n",
        "fn f() {\n    (s? ?.x)\n}\n",
        "fn f() {\n    s? .x\n}\n",
        "fn f() {\n    s? .a().b()\n}\n",
    ] {
        let original = skull_parser::parse_module_clean(source, "<source>".into()).unwrap();
        let once = skull_fmt::format_code(source);
        let formatted = skull_parser::parse_module_clean(&once, "<formatted>".into())
            .unwrap_or_else(|e| panic!("formatted range must parse: {e:?}\n{once}"));
        assert!(formatted.recovery_errors.is_empty(), "{once}");
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

/// Format `source` and require: unchanged output, a fixed point, and a clean
/// reparse to the same s-expression.
fn assert_format_keeps(source: &str) {
    let original = skull_parser::parse_module_clean(source, "<source>".into()).unwrap();
    let once = skull_fmt::format_code(source);
    assert_eq!(once, source);
    let formatted = skull_parser::parse_module_clean(&once, "<formatted>".into())
        .unwrap_or_else(|e| panic!("formatted output must parse: {e:?}\n{once}"));
    assert!(formatted.recovery_errors.is_empty(), "{once}");
    assert_eq!(original.module_sexp(), formatted.module_sexp(), "{once}");
    assert_eq!(once, skull_fmt::format_code(&once));
}

/// The `!` of a macro call enables type-shaped argument parsing, so neither
/// `m!(Foo<int>)` nor the method form `recv.m!(a)` may be respelled as a plain
/// call (`m(Foo(int))`, `m(recv, a)`).
#[test]
fn macro_call_bang_is_kept() {
    for line in [
        "let a = m!(1, 2)",
        "let a = m!()",
        "let b = x.m!(a)",
        "let b = x.m!()",
        "let b = x.y.m!(a, 2)",
        "let b = f(1).m!(a)",
        "let c = x.m!(a).n!(b).k!()",
        "let c = x.m!(a).n(b).k!()",
        "let c = x.n(b).m!(a)",
        "let d = m!(Foo<int>, [int], (int, string), int?)",
        "let d = m!(Map<int, Box<int>>)",
        "let e = x.m!(Map<int, string>)",
        "let e = x.m!([Box<int>])",
        "let f = m!(n!(1), x.k!(2))",
        "let f = m!(x.n!(Foo<int>))",
        "let g = m!(...rest)",
        "let h = x.m!(a) + n!(b)",
    ] {
        assert_format_keeps(&format!("fn f() {{\n    {line}\n}}\n"));
    }
}

/// A spaced unary minus over a number parses as `UnaryMinus(1)`; printed
/// tight it would lex as the literal `-1`, a different AST.
#[test]
fn spaced_unary_minus_over_a_number_keeps_its_shape() {
    for line in [
        "x += - 1",
        "x = - 1",
        "x = - 1.5",
        "x = - .5",
        "f(- 2)",
        "x -= -1",
        "x = -y",
    ] {
        assert_format_keeps(&format!("fn f() {{\n    {line}\n}}\n"));
    }
}
