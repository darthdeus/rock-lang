//! Code action fixtures run through the real `rock lsp serve`.

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
  { op = "applied_text_equals", path = ["result"], selector = { title = "Inline local `count`" }, file = "$primary", expected = '''
fn main() {
    print((1 + 2) * (1 + 2))
}''' },
]
"#;

const LATER_CASE: &str = r#"
[[tests]]
name = "later"
kind = "edit_sequence"
[[tests.steps]]
mode = "replace"
source = "x"
"#;

fn replace_block(fixture: &str, start: &str, end: &str, replacement: &str) -> String {
    let start = fixture.find(start).unwrap();
    let end = start + fixture[start..].find(end).unwrap();
    format!("{}{}{}", &fixture[..start], replacement, &fixture[end..])
}

fn set_checks(fixture: &str, checks: &str) -> String {
    let start = fixture.find("checks = [").unwrap();
    let end = start + fixture[start..].find("\n]").unwrap() + 2;
    format!(
        "{}checks = {}{}",
        &fixture[..start],
        checks,
        &fixture[end..]
    )
}

fn set_request(fixture: &str, request: &str) -> String {
    replace_block(fixture, "request = ", "\nchecks = ", request)
}

fn fixture_with_source_ref(reference: &str, forward: bool) -> String {
    let base = FIXTURE.replace("name = \"inline_count\"", "name = \"base\"");
    let bad = FIXTURE.replace("name = \"inline_count\"", "name = \"bad_source\"");
    let bad = replace_block(
        &bad,
        "source = ",
        "\nrequest = ",
        &format!("source = {reference}"),
    );
    format!("{base}{bad}{}", if forward { LATER_CASE } else { "" })
}

fn edit_sequence_fixture(steps: &str) -> String {
    let fixture = FIXTURE.replace("kind = \"code_action\"", "kind = \"edit_sequence\"");
    let fixture = set_request(&fixture, "");
    format!("{}{steps}", set_checks(&fixture, "[]"))
}

fn run_fixture(fixture: &str) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("action.rock.lsp"), fixture).unwrap();
    crate::support::rock(dir.path())
        .args(["test", "action.rock.lsp"])
        .env("ROCK_TEST_VERBOSE", "1")
        .output()
        .expect("start rock test")
}

fn failed(fixture: String, fragment: &'static str) -> (String, i32, &'static str) {
    (fixture, 1, fragment)
}

#[test]
fn generic_lsp_fixtures_report_parse_and_reply_failures() {
    let later_ref = fixture_with_source_ref("{ from_test = \"later\" }", true);
    let empty_ref =
        fixture_with_source_ref("{ from_test = \"base\", replace = [[\"\", \"x\"]] }", false);
    let missing_ref = fixture_with_source_ref(
        "{ from_test = \"base\", replace = [[\"missing\", \"x\"]] }",
        false,
    );
    let first_step = edit_sequence_fixture(
        "\n[[tests.steps]]\nmode = \"type_at\"\nline = 0\ncol = 0\ntext = \"x\"\n",
    );
    let forward_step = edit_sequence_fixture(
        "\n[[tests.steps]]\nmode = \"replace\"\nsource = \"x\"\n[[tests.steps]]\nmode = \"replace\"\nsource = { from_step = 3 }\n",
    );
    let cases = vec![
        (FIXTURE.into(), 0, "1 passed; 0 failed"),
        failed(
            FIXTURE.replace("print((1 + 2) * (1 + 2))", "print(3 * 3)"),
            "result after",
        ),
        failed(
            FIXTURE.replace("Inline local `count`", "Extract function"),
            "no action titled \"Extract function\"; the server offered [\"Inline local `count`\"]",
        ),
        failed(
            FIXTURE.replace(
                "kind = \"code_action\"",
                "kind = \"code_action\"\nunknown = true",
            ),
            "unknown field `unknown`",
        ),
        failed(
            FIXTURE.replace("name = \"inline_count\"\n", ""),
            "missing field `name`",
        ),
        failed(
            FIXTURE.replace("kind = \"code_action\"", "kind = 7"),
            "invalid type",
        ),
        failed(
            set_request(FIXTURE, ""),
            "request fixture needs a request and checks",
        ),
        failed(
            set_checks(FIXTURE, "[]"),
            "request fixture needs a request and checks",
        ),
        failed(
            fixture_with_source_ref("{ from_test = \"missing\" }", false),
            "unknown earlier SOURCE \"missing\"",
        ),
        failed(later_ref, "unknown earlier SOURCE \"later\""),
        failed(
            fixture_with_source_ref("{ from_test = \"base\", from_fixture = true }", false),
            "invalid case source reference",
        ),
        failed(empty_ref, "REPLACE source is empty or absent"),
        failed(missing_ref, "REPLACE source is empty or absent"),
        failed(
            first_step,
            "first edit_sequence step must replace the document",
        ),
        failed(
            forward_step,
            "SOURCE must name an earlier full-document step",
        ),
        failed(
            edit_sequence_fixture(
                "\n[[tests.steps]]\nmode = \"replace\"\nsource = \"fn main() {}\"\n[[tests.steps.checks]]\nrequest = { transport = \"rpc\", method = \"textDocument/semanticTokens/full\", params = { textDocument = { uri = \"$uri\" } } }\nassertions = [{ op = \"gte\", path = [\"result\", \"data\"], value = 100000 }]\n",
            ),
            "step 1 SemanticTokens",
        ),
        failed(
            set_checks(FIXTURE, "[{ op = \"equals\", path = [], value = \"x\" }]"),
            "check path is empty",
        ),
        failed(
            set_checks(
                FIXTURE,
                "[{ op = \"contains_all\", path = [\"result\"], value = [] }]",
            ),
            "contains_all check has no values",
        ),
        failed(
            set_request(
                FIXTURE,
                "request = { transport = \"cli\", argv = [\"not-a-command\", \"$file\"] }",
            ),
            "unrecognized subcommand",
        ),
        failed(
            set_checks(
                FIXTURE,
                "[{ op = \"equals\", path = [\"result\", 0, \"missing\"], value = \"x\" }]",
            ),
            "reply has no missing field",
        ),
        failed(
            set_checks(
                FIXTURE,
                "[{ op = \"trimmed_equals\", path = [\"result\"], value = \"x\" }]",
            ),
            "reply field is not a string",
        ),
    ];
    for (fixture, expected_exit, fragment) in cases {
        let output = run_fixture(&fixture);
        let text = crate::support::text(&output);
        assert_eq!(output.status.code(), Some(expected_exit), "{text}");
        assert!(text.contains(fragment), "missing {fragment:?}: {text}");
    }
}
