//! `@test(returns = <literal>)` discovery: the annotation arg becomes
//! `SkullTest::returns` (V1 of `docs/multi-backend-testing-plan.md`), and the
//! malformed shapes reject with a spanned CompilerError.

mod common;

use skull::ValueExpect;

fn tests_of(src: &str) -> Result<Vec<skull::SkullTest>, skull_parser::CompilerError> {
    let ws = common::ws_with_stdlib(src);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("module id");
    skull::module_tests(&ws, mid)
}

#[test]
fn returns_literal_extracts_int_float_bool() {
    let tests = tests_of(
        "@test(returns = 43)\nfn a() -> int { return 43 }\n\
         @test(returns = -2.5)\nfn b() -> float { return -2.5 }\n\
         @test(returns = true)\nfn c() -> bool { return true }\n\
         @test\nfn d() { }\n",
    )
    .expect("discovery ok");
    let got: Vec<(&str, Option<ValueExpect>)> =
        tests.iter().map(|t| (t.name.as_str(), t.returns)).collect();
    assert_eq!(
        got,
        vec![
            ("a", Some(ValueExpect::Int(43))),
            ("b", Some(ValueExpect::Float(-2.5))),
            ("c", Some(ValueExpect::Bool(true))),
            ("d", None),
        ]
    );
}

#[test]
fn returns_plus_output_is_legal_and_both_extract() {
    let tests = tests_of(
        "@test(returns = 1, output = \"hi\")\nfn a() -> int { print(\"hi\")\n return 1 }\n",
    )
    .expect("discovery ok");
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0].returns, Some(ValueExpect::Int(1)));
    assert_eq!(tests[0].output.as_deref(), Some("hi"));
}

#[test]
fn returns_plus_expect_failure_rejects() {
    let err = tests_of(
        "@test(expect_failure = true, returns = 42)\nfn a() -> int { return 42 }\n",
    )
    .expect_err("contradictory annotation must reject");
    assert!(
        err.message.contains("returns") && err.message.contains("expect_failure"),
        "unexpected message: {}",
        err.message
    );
}

#[test]
fn returns_non_literal_rejects() {
    for bad in ["\"str\"", "foo"] {
        let err = tests_of(&format!("@test(returns = {bad})\nfn a() -> int {{ return 1 }}\n"))
            .expect_err("non int/float/bool literal must reject");
        assert!(
            err.message.contains("returns expects an int/float/bool literal"),
            "unexpected message for {bad}: {}",
            err.message
        );
    }
}
