//! Runs the shared `tests/fmt` corpus through skull-fmt and reports a pass/fail
//! histogram. Informational during bring-up: it prints a summary (run with
//! `--nocapture`) and only fails if NOTHING passes (a smoke gate).

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
    // Print first chunk of failures for triage.
    let detail = std::env::var("FMT_DETAIL").is_ok();
    let take = if detail { failures.len() } else { 0 };
    for (file, name, input, exp, got) in failures.iter().take(take) {
        eprintln!("---- {file} :: {name} ----");
        eprintln!("INPUT:\n{input}");
        eprintln!("EXPECTED:\n{exp}");
        eprintln!("GOT:\n{got}\n");
    }
    eprintln!("\n=== failures by file ===");
    let mut by_file: std::collections::BTreeMap<String, usize> = Default::default();
    for (file, ..) in &failures {
        *by_file.entry(file.clone()).or_default() += 1;
    }
    let mut v: Vec<_> = by_file.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    for (file, n) in v {
        eprintln!("{n:3}  {file}");
    }

    assert!(pass > 0, "no corpus cases passed");
}
