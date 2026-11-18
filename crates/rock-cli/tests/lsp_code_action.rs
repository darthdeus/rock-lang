//! Code action fixtures run through the real `rock lsp serve`: the action is
//! requested, its edit applied, and the result compared.

const FIXTURE: &str = r#"
[[tests]]
name = "inline_count"
kind = "code_action"
position = [1, 8]
source = '''
fn main() {
    let count = 1 + 2
    print(count * count)
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [
  { op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true },
  { op = "applied_text_equals", path = ["result"], selector = { title = "Inline local `count`" }, file = "$primary", source = "primary", expected = '''
fn main() {
    print((1 + 2) * (1 + 2))
}''', encoding = "utf16", trim = "end" },
]
"#;

/// Run `rock test` on `fixture` written to a temporary `.rock.lsp` file and
/// return its combined output.
fn run_fixture(fixture: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("action.rock.lsp"), fixture).unwrap();
    let out = crate::support::rock(dir.path())
        .args(["test", "action.rock.lsp"])
        .env("ROCK_TEST_VERBOSE", "1")
        .output()
        .expect("start rock test");
    crate::support::text(&out)
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
