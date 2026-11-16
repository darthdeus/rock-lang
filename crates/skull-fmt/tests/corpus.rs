//! Runs the `tests/fmt` corpus through skull-fmt: every case must format to
//! its expected output, which must reparse and be a fixed point.

use std::fs;
use std::path::{Path, PathBuf};

struct Case {
    file: String,
    name: String,
    input: String,
    expected: String,
}

/// Every case in the corpus, in file order.
fn cases() -> Vec<Case> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fmt");
    let mut files = Vec::new();
    collect(&dir, &mut files);
    files.sort();
    files
        .iter()
        .flat_map(|f| {
            let short = f.strip_prefix(&dir).unwrap().to_string_lossy().to_string();
            parse_cases(&short, &fs::read_to_string(f).unwrap())
        })
        .collect()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).expect("read the corpus").flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("fmt") {
            out.push(p);
        }
    }
}

/// Split a `.rock.fmt` file into cases. A case is `//=== TEST CASE: name ===`
/// then input then `//=== EXPECTED ===`+output OR `//=== NO CHANGE ===`
/// (output == input).
fn parse_cases(file: &str, text: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    let marker = |l: &str, kw: &str| {
        let t = l.trim();
        t.starts_with("//===") && t.contains(kw)
    };
    while i < lines.len() {
        if !marker(lines[i], "TEST CASE") {
            i += 1;
            continue;
        }
        let name = lines[i]
            .trim()
            .trim_start_matches("//===")
            .trim_end_matches("===")
            .replace("TEST CASE:", "")
            .trim()
            .to_string();
        i += 1;
        let start = i;
        while i < lines.len() && !marker(lines[i], "EXPECTED") && !marker(lines[i], "NO CHANGE") {
            i += 1;
        }
        let input = trim_block(&lines[start..i]);
        let no_change = i < lines.len() && marker(lines[i], "NO CHANGE");
        i += 1; // consume sentinel
        let expected = if no_change {
            input.clone()
        } else {
            let start = i.min(lines.len());
            while i < lines.len() && !marker(lines[i], "TEST CASE") {
                i += 1;
            }
            trim_block(&lines[start..i.max(start)])
        };
        cases.push(Case {
            file: file.to_string(),
            name,
            input,
            expected,
        });
    }
    cases
}

fn trim_block(lines: &[&str]) -> String {
    let is_blank = |l: &&str| l.trim().is_empty();
    let start = lines
        .iter()
        .position(|l| !is_blank(l))
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|l| !is_blank(l))
        .map_or(start, |e| e + 1);
    lines[start..end].join("\n")
}

#[test]
fn corpus_cases_format_to_their_expected_output() {
    let cases = cases();
    assert!(!cases.is_empty(), "corpus silently empty");
    let mut failures = Vec::new();
    for c in &cases {
        let id = format!("{} :: {}", c.file, c.name);
        // Only a `parse_error*` case may hold source that does not parse; it
        // checks the passthrough, so a case cannot pass by accident.
        let parses = skull_parser::parse_module_clean(&c.input, "<corpus>".into()).is_ok();
        if parses == c.name.starts_with("parse_error") {
            failures.push(format!("{id}: parses is {parses}"));
            continue;
        }
        let got = if parses {
            match skull_fmt::try_format(&c.input, &skull_fmt::FormatConfig::default()) {
                Ok(got) => got,
                Err(e) => {
                    failures.push(format!("{id}: {e}"));
                    continue;
                }
            }
        } else {
            skull_fmt::format_code(&c.input)
        };
        if got.trim_end() != c.expected.trim_end() {
            failures.push(format!(
                "{id}\nINPUT:\n{}\nEXPECTED:\n{}\nGOT:\n{got}",
                c.input, c.expected
            ));
        } else if parses && skull_parser::parse_module_clean(&got, "<corpus>".into()).is_err() {
            failures.push(format!("{id}: output does not parse:\n{got}"));
        } else if skull_fmt::format_code(&got) != got {
            failures.push(format!("{id}: output is not a fixed point:\n{got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} corpus case(s) failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n\n")
    );
}
