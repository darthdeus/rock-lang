//! `--no-stdlib` compiles with no standard-library modules: a program that
//! names a stdlib declaration fails, an operator-free one still builds.
use crate::support::{invoke, text};

const NEEDS_STDLIB: &str = "fn main() {\n    print(\"from stdlib\")\n}\n";
const OPERATOR_FREE: &str = "fn main() {\n    let x = 1\n}\n";
const NEEDS_STDLIB_TEST: &str =
    "@test(output = \"from stdlib\")\nfn uses_print() {\n    print(\"from stdlib\")\n}\n";
const OPERATOR_FREE_TEST: &str = "@test\nfn bare() {\n    let x = 1\n}\n";

fn program(source: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rock"), source).unwrap();
    root
}

#[test]
fn no_stdlib_rejects_a_stdlib_declaration_in_check_and_run() {
    let root = program(NEEDS_STDLIB);
    for command in ["check", "run"] {
        let with = invoke(root.path(), &[command, "main.rock"]);
        assert!(with.status.success(), "{command}:\n{}", text(&with));
        let without = invoke(root.path(), &[command, "main.rock", "--no-stdlib"]);
        assert!(
            !without.status.success(),
            "{command} --no-stdlib must not find `print`:\n{}",
            text(&without)
        );
        assert!(
            text(&without).contains("Function `print` doesn't exist"),
            "{command} --no-stdlib:\n{}",
            text(&without)
        );
    }
    let run = invoke(root.path(), &["run", "main.rock"]);
    assert!(text(&run).contains("from stdlib"), "{}", text(&run));
}

#[test]
fn no_stdlib_still_builds_an_operator_free_program() {
    let root = program(OPERATOR_FREE);
    for command in ["check", "run"] {
        for flags in [&[][..], &["--no-stdlib"][..]] {
            let mut args = vec![command, "main.rock"];
            args.extend_from_slice(flags);
            let out = invoke(root.path(), &args);
            assert!(out.status.success(), "{args:?}:\n{}", text(&out));
        }
    }
}

#[test]
fn no_stdlib_applies_to_rock_test() {
    let root = program(NEEDS_STDLIB_TEST);
    let with = invoke(root.path(), &["test", "main.rock"]);
    assert!(with.status.success(), "{}", text(&with));
    let without = invoke(root.path(), &["test", "main.rock", "--no-stdlib"]);
    assert!(!without.status.success(), "{}", text(&without));

    let root = program(OPERATOR_FREE_TEST);
    for flags in [&[][..], &["--no-stdlib"][..]] {
        let mut args = vec!["test", "main.rock"];
        args.extend_from_slice(flags);
        let out = invoke(root.path(), &args);
        assert!(out.status.success(), "{args:?}:\n{}", text(&out));
    }
}
