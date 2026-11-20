use std::fs;
use std::path::Path;

use super::collect_files;

#[test]
fn corpus_cases_format_to_their_expected_output() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fmt");
    let mut files = Vec::new();
    collect_files(&dir, "fmt", &mut files);
    files.sort();

    let mut checked = 0;
    let mut failures = Vec::new();
    for file in files {
        let short = file.strip_prefix(&dir).unwrap().display();
        let text = fs::read_to_string(&file).unwrap().replace("\r\n", "\n");
        for case in test_utils::parse_sections(&text) {
            let (input, expected) = case
                .content
                .split_once("\n//=== EXPECTED ===\n")
                .unwrap_or((case.content, case.content));
            checked += 1;
            if let Err(error) = check_case(case.name, input.trim_matches('\n'), expected.trim_end())
            {
                failures.push(format!("{short} :: {}: {error}", case.name));
            }
        }
    }
    assert!(checked > 0, "corpus silently empty");
    assert!(
        failures.is_empty(),
        "{} of {checked} corpus case(s) failed:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn check_case(name: &str, input: &str, expected: &str) -> Result<(), String> {
    let parses = skull_parser::parse_module_clean(input, "<corpus>".into()).is_ok();
    if parses == name.starts_with("parse_error") {
        return Err(format!("parses is {parses}"));
    }
    let got = if parses {
        skull_fmt::try_format(input, &skull_fmt::FormatConfig::default())?
    } else {
        skull_fmt::format_code(input)
    };
    if got.trim_end() != expected {
        return Err(format!(
            "\nINPUT:\n{input}\nEXPECTED:\n{expected}\nGOT:\n{got}"
        ));
    }
    if parses && skull_parser::parse_module_clean(&got, "<corpus>".into()).is_err() {
        return Err(format!("output does not parse:\n{got}"));
    }
    if skull_fmt::format_code(&got) != got {
        return Err(format!("output is not a fixed point:\n{got}"));
    }
    Ok(())
}
