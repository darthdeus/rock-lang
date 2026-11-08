//! `TYPE: code_action` fixtures run through the real `rock lsp serve`: the
//! action is requested, its edit applied, and the result compared.
use std::process::Command;

const FIXTURE: &str = "\
TEST: inline_count
TYPE: code_action
---
fn main() {
    let «start»count«end» = 1 + 2
    print(count * count)
}
---
OPERATION: code_action
---
EXPECTED:
ACTION_TITLE: Inline local `count`
RESULT:
fn main() {
    print((1 + 2) * (1 + 2))
}
===
";

/// Run `rock test` on `fixture` written to a temporary `.rock.lsp` file and
/// return its combined output.
fn run_fixture(fixture: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("action.rock.lsp"), fixture).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(dir.path())
        .args(["test", "action.rock.lsp"])
        .env("ROCK_TEST_VERBOSE", "1")
        .env("ROCK_INVOCATION_LOG", "off")
        .output()
        .expect("start rock test");
    test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

#[test]
fn a_code_action_fixture_applies_the_action_and_passes() {
    let output = run_fixture(FIXTURE);
    assert!(
        output.contains("LSP tests: ok. 1 passed; 0 failed"),
        "{output}"
    );
}

/// The expected text is compared, so a wrong RESULT fails the fixture.
#[test]
fn a_code_action_fixture_with_the_wrong_result_fails() {
    let output = run_fixture(&FIXTURE.replace("print((1 + 2) * (1 + 2))", "print(3 * 3)"));
    assert!(output.contains("0 passed; 1 failed"), "{output}");
}

/// An action the server does not offer fails, naming the titles it did offer.
#[test]
fn a_code_action_fixture_naming_an_absent_action_fails() {
    let output = run_fixture(&FIXTURE.replace("Inline local `count`", "Extract function"));
    assert!(output.contains("0 passed; 1 failed"), "{output}");
    assert!(output.contains("Inline local `count`"), "{output}");
}
