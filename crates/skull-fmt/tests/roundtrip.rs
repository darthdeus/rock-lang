//! Round-trip checks the `tests/fmt` corpus cannot express and narrow line widths.

#[test]
fn modules_v1_forms_format_unchanged() {
    let cfg = skull_fmt::FormatConfig::default();
    for src in [
        "fn f() {\n    let x = (5 : int)\n    let y = (x : float) / 2\n    print(y)\n}\n",
        "fn f() {\n    let g = ((v => v + 1) : fn(int) -> int)\n    print(g(1))\n}\n",
        "fn f() {\n    let t = ((1, 2) : (int, int))\n    print(t)\n}\n",
        "struct S {\n    name: string,\n    private(set) health: float = 100,\n    pub field: int,\n}\n",
        "use a::b::*;\nuse a::b::{Actor, heal as restore};\nuse geo::operator(+);\n\nfn f() {\n}\n",
        "pub @inline fn a() -> int { 1 }\n\npub(package) @pure\nfn b() -> int {\n    2\n}\n",
        "pub fn c() -> int {\n    3\n}\n\npub struct S {\n    x: int,\n    pub(package) y: int,\n    pub private(set) z: int = 0,\n}\n\nprivate global G: int = 1\n\npub(package) @inline inline(lua) fn d() -> int = \"\"\"return 1\"\"\"\n",
        "mod main;\n\nuse super::helpers::shout;\n\nfn main() {\n    print(shout(\"hi\"))\n}\n",
        "pub mod a::b;\n\n// lead\nfn f() {\n    g()\n}\n",
        "private mod secret;\n\npub fn f() {\n    g()\n}\n",
    ] {
        let once = skull_fmt::try_format(src, &cfg)
            .unwrap_or_else(|e| panic!("format failed for {src:?}: {e}"));
        assert_eq!(once, src);
        skull_parser::parse_module_clean(&once, "<v1>".into())
            .unwrap_or_else(|e| panic!("{src:?} must reparse: {}", e.message));
    }
}

#[test]
fn closure_followed_by_another_argument_reaches_a_fixed_point() {
    for source in [
        "fn run() { invoke(() => { action() }, enabled: true) }",
        "fn run() { invoke(() => { first(); second() }, enabled: true) }",
        "fn run() { let result = invoke_callback(() => { perform_operation(context, configuration) }, report_failure) }",
        "fn run() { let result = invoke_callback(() => { perform_operation(context, configuration) }, enabled: true) }",
    ] {
        for max_line_length in [40, 100] {
            let config = skull_fmt::FormatConfig {
                max_line_length,
                ..Default::default()
            };
            let once = skull_fmt::try_format(source, &config).expect("format source");
            for argument in ["enabled: true", "report_failure"] {
                assert_eq!(once.contains(argument), source.contains(argument), "{once}");
            }
            let twice = skull_fmt::try_format(&once, &config).expect("reparse formatted source");
            assert_eq!(once, twice, "source: {source}");
            skull_parser::parse_module_clean(&once, "<formatted>".into()).unwrap();
        }
    }
}

#[test]
fn calls_wrap_when_a_nested_closure_breaks() {
    let config = skull_fmt::FormatConfig {
        max_line_length: 40,
        ..Default::default()
    };
    let source = "fn f() {\n    check(probe(() => { work(one, two, three) }) != null)\n}\n";
    let once = skull_fmt::try_format(source, &config).expect("format nested closure");
    let twice = skull_fmt::try_format(&once, &config).expect("reparse nested closure");
    assert_eq!(once, twice);
    assert_eq!(
        once,
        "fn f() {
    check(
        probe(() => {
            work(one, two, three)
        }) != null
    )
}
"
    );
}

#[test]
fn formatting_keeps_surface_tree_structure() {
    let config = skull_fmt::FormatConfig::default();
    let tree = |source: &str| {
        let ast = skull_parser::parse_module_clean(source, "<roundtrip>".into()).unwrap();
        format!("{ast:#?}")
    };
    let mut failures = Vec::new();
    for source in [
        "fn ns::Type.method<T>(self, extra:T) { extra }\n",
        "fn ns::Type<T>.method<U>(self, extra:U) { extra }\n",
        "fn f() {\n    profile @ {\n        action()\n    }\n}\n",
        "fn f(callback: fn(int)?) { callback }\n",
        "fn f(callback: fn(int) -> bool? ?) { callback }\n",
        "fn f(value: (int,)) { value }\n",
        "fn f() {\n    match x {\n        0 => break,\n        1 => continue,\n        _ => return\n    }\n}\n",
    ] {
        let formatted = skull_fmt::try_format(source, &config).expect("format source");
        if tree(source) != tree(&formatted) {
            failures.push(format!("{source:?} became {formatted:?}"));
        }
        let twice = skull_fmt::try_format(&formatted, &config).unwrap();
        if twice != formatted {
            failures.push(format!("{formatted:?} became {twice:?} on the second pass"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn nested_calls_keep_an_existing_hanging_layout() {
    let config = skull_fmt::FormatConfig::default();
    let source =
        "fn f() {\n    check(materialize(read(\n        first,\n        second\n    )))\n}\n";
    assert_eq!(skull_fmt::try_format(source, &config).unwrap(), source);
}

#[test]
fn authored_expression_continuations_stay_broken() {
    let config = skull_fmt::FormatConfig {
        max_line_length: 45,
        ..Default::default()
    };
    let source = "global current: int =\n    compute(next, configuration)\n\nfn f() {\n    static total: int =\n        compute(current, configuration)\n    result =\n        compute(current, configuration)\n    let (width, height) =\n        measure(current, configuration)\n    match result {\n        Mode::Primary =>\n            handle(current, configuration),\n        _ => fallback(),\n    }\n}\n";
    assert_eq!(skull_fmt::try_format(source, &config).unwrap(), source);
}
