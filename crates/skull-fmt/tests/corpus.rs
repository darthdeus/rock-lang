//! Runs the shared `tests/fmt` corpus through skull-fmt and asserts every case
//! formats to its expected output. Prints a pass/fail histogram (run with
//! `--nocapture`) and fails on ANY mismatch, listing the offending cases. A
//! genuine bring-up gap goes in `KNOWN_FAILING`, which forces a same-commit
//! removal the moment the case starts passing.

use std::fs;
use std::path::{Path, PathBuf};

struct Case {
    file: String,
    name: String,
    input: String,
    expected: String,
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fmt")
}

fn collect_fmt_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_fmt_files(&p, out);
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
        let mut input = Vec::new();
        let mut no_change = false;
        while i < lines.len() && !marker(lines[i], "EXPECTED") && !marker(lines[i], "NO CHANGE") {
            input.push(lines[i]);
            i += 1;
        }
        if i < lines.len() && marker(lines[i], "NO CHANGE") {
            no_change = true;
        }
        i += 1; // consume sentinel
        let input_s = trim_block(&input);
        let expected = if no_change {
            input_s.clone()
        } else {
            let mut out = Vec::new();
            while i < lines.len() && !marker(lines[i], "TEST CASE") {
                out.push(lines[i]);
                i += 1;
            }
            trim_block(&out)
        };
        cases.push(Case {
            file: file.to_string(),
            name,
            input: input_s,
            expected,
        });
    }
    cases
}

/// Cases the formatter does not yet handle. `(file, case_name)` — matched
/// exactly against the values printed in the failure list. Empty means the
/// whole corpus is expected to pass; adding an entry is a ratchet: the case
/// starts passing -> the "expected-fail passed" check goes red -> you must
/// remove the entry in the same commit.
const KNOWN_FAILING: &[(&str, &str)] = &[];

fn trim_block(lines: &[&str]) -> String {
    let mut v: Vec<&str> = lines.to_vec();
    while v.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
        v.remove(0);
    }
    while v.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        v.pop();
    }
    v.join("\n")
}

#[test]
fn corpus_report() {
    let mut files = Vec::new();
    collect_fmt_files(&corpus_dir(), &mut files);
    files.sort();

    let mut total = 0;
    let mut pass = 0;
    let mut parse_err = 0;
    let mut failures = Vec::new();

    for f in &files {
        let text = fs::read_to_string(f).unwrap();
        let short = f.strip_prefix(corpus_dir()).unwrap().to_string_lossy().to_string();
        let cases = parse_cases(&short, &text);
        for c in cases {
            total += 1;
            let got = skull_fmt::format_code(&c.input);
            let got_trim = got.trim_end();
            let exp_trim = c.expected.trim_end();
            if got_trim == exp_trim {
                pass += 1;
            } else {
                if got_trim == c.input.trim_end() && skull_parser::parse_module(&c.input, "x".into()).is_err() {
                    parse_err += 1;
                }
                failures.push((c.file, c.name, c.input, exp_trim.to_string(), got_trim.to_string()));
            }
        }
    }

    eprintln!("\n=== skull-fmt corpus: {pass}/{total} cases pass ({parse_err} parse-fail passthrough) ===\n");

    let is_known = |file: &str, name: &str| {
        KNOWN_FAILING.iter().any(|(f, n)| *f == file && *n == name)
    };

    // Partition failures into unexpected (must go red) and known bring-up gaps.
    let mut unexpected = Vec::new();
    for (file, name, input, exp, got) in &failures {
        if is_known(file, name) {
            continue;
        }
        unexpected.push((file.clone(), name.clone(), input.clone(), exp.clone(), got.clone()));
    }

    // Ratchet: an allowlisted case that now passes must be removed same-commit.
    let failed: std::collections::HashSet<(String, String)> =
        failures.iter().map(|(f, n, ..)| (f.clone(), n.clone())).collect();
    let stale: Vec<_> = KNOWN_FAILING
        .iter()
        .filter(|(f, n)| !failed.contains(&(f.to_string(), n.to_string())))
        .collect();

    if !unexpected.is_empty() {
        eprintln!("=== {} unexpected failure(s) ===", unexpected.len());
        for (file, name, input, exp, got) in &unexpected {
            eprintln!("---- {file} :: {name} ----");
            eprintln!("INPUT:\n{input}");
            eprintln!("EXPECTED:\n{exp}");
            eprintln!("GOT:\n{got}\n");
        }
        let list: Vec<String> = unexpected
            .iter()
            .map(|(f, n, ..)| format!("{f} :: {n}"))
            .collect();
        panic!(
            "{} corpus case(s) formatted differently than expected:\n{}",
            unexpected.len(),
            list.join("\n")
        );
    }

    assert!(
        stale.is_empty(),
        "KNOWN_FAILING entries now pass -- remove them:\n{}",
        stale
            .iter()
            .map(|(f, n)| format!("{f} :: {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    assert!(pass > 0, "no corpus cases passed (corpus silently empty?)");
}
