use bone_lints::LintOptions;
use rock_core::errors::CompilerErrorLevel;

fn findings(src: &str) -> Vec<bone_lints::Finding> {
    crate::support::checked_lint(
        &[("<stdlib-ufcs>", src)],
        &LintOptions::only(["stdlib_ufcs".into()]),
    )
}

#[test]
fn resolved_stdlib_calls_offer_receiver_syntax() {
    let src = "fn main() {\n    let xs = [1]\n    vec_push(xs, 2)\n    let _n = vec_len(xs)\n    let _s = vec_get(xs, 0)\n    vec_set(xs, 0, 3)\n    let _m = string_match(\"hello\", \"h\")\n}";
    let found = findings(src);
    assert_eq!(found.len(), 5, "{found:?}");
    for (finding, method) in found
        .iter()
        .zip(["push", "len", "get", "set", "string_match"])
    {
        assert!(
            finding.error.message.contains(&format!(".{method}(")),
            "{finding:?}"
        );
        assert_eq!(finding.level(), CompilerErrorLevel::Advice);
    }
    assert!(
        crate::lint(&[("<stdlib-ufcs>", src)], &LintOptions::default(), true)
            .iter()
            .all(|f| f.lint != "stdlib_ufcs")
    );
    assert!(
        LintOptions::only(["stdlib_ufcs".into()])
            .unknown()
            .is_empty()
    );
}

#[test]
fn skips_methods_constructors_named_arguments_and_user_overloads() {
    let src = "struct User {}\nfn vec_len(u: User) -> int { 0 }\nfn main() {\n    let xs: [int] = vec_new()\n    xs.push(1)\n    let _a = xs.len()\n    let _b = vec_len(User {})\n    vec_push(item: 2, vec: xs)\n    let _c = vec_get(xs, index: 0)\n    let _d = string_char(65)\n}";
    assert!(findings(src).is_empty(), "{:?}", findings(src));
}

#[test]
fn skips_struct_spreads_that_expand_into_named_arguments() {
    let src = "struct Push { vec: [int], item: int }\nfn main() {\n    let p = Push { vec: [1], item: 2 }\n    vec_push(..p)\n}";
    assert!(findings(src).is_empty());
}
