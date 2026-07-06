//! Real-source guardrails for the LIVE formatter (`rock fmt` routes through
//! `skull_fmt::format_code`). Over every `.rock` file in stdlib + games + stone
//! + mag, formatting must:
//!   1. be idempotent -- `format(format(x)) == format(x)` (a fixed point), and
//!   2. reparse cleanly -- the output is valid Rock, and
//!   3. neither invent nor drop comments -- the output's comment multiset
//!      EQUALS the source's (catches the duplicate-trailing-comment class AND
//!      the standalone-comment-loss class).
//!
//! These ran ad-hoc while a round of real-file corruption bugs was fixed
//! (byte-vs-char slicing, operator bodies, blank/param oscillation, an extra `>`
//! that produced unparseable output, trailing-comment loss/dup); this pins them.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("rock") {
            out.push(p);
        }
    }
}

fn rock_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in ["stdlib", "games", "stone", "mag"] {
        collect(&repo_root().join(dir), &mut out);
    }
    out.sort();
    out
}

/// Multiset of `//...` comment suffixes per line (trailing whitespace trimmed).
/// `//` inside a verbatim string/lua body appears identically in both source and
/// output, so it cancels in the subset check.
fn comment_multiset(src: &str) -> std::collections::HashMap<String, usize> {
    let mut m = std::collections::HashMap::new();
    for line in src.lines() {
        if let Some(idx) = line.find("//") {
            *m.entry(line[idx..].trim_end().to_string()).or_default() += 1;
        }
    }
    // Block comments (`/* ... */`), whole-slice keyed so `/* x */` and `// x`
    // never alias. A `/*` inside a `//` line comment or a string round-trips
    // verbatim, so it cancels in the equality check.
    let mut rest = src;
    while let Some(a) = rest.find("/*") {
        if let Some(b) = rest[a..].find("*/") {
            *m.entry(rest[a..a + b + 2].to_string()).or_default() += 1;
            rest = &rest[a + b + 2..];
        } else {
            break;
        }
    }
    m
}

#[test]
fn real_sources_are_idempotent_reparse_and_keep_comments() {
    let files = rock_files();
    assert!(
        files.len() > 100,
        "expected the real corpus (>100 files), found {} -- repo layout changed?",
        files.len()
    );

    let mut fails = Vec::new();
    for f in &files {
        let short = f.strip_prefix(repo_root()).unwrap_or(f).display();
        let Ok(src) = fs::read_to_string(f) else { continue };

        let once = skull_fmt::format_code(&src);
        let twice = skull_fmt::format_code(&once);
        if once != twice {
            fails.push(format!("not idempotent: {short}"));
            continue;
        }

        // Only a *changed* output is the formatter's responsibility to keep
        // valid (an unparseable input is passed through unchanged).
        if once != src {
            match skull_parser::parse_module(&once, "<roundtrip>".into()) {
                Ok(ast) if ast.recovery_errors.is_empty() => {}
                Ok(ast) => fails.push(format!(
                    "output has recovery errors: {short}: {:?}",
                    ast.recovery_errors
                )),
                Err(e) => fails.push(format!("output does not parse: {short}: {e:?}")),
            }
        }

        let src_c = comment_multiset(&src);
        let out_c = comment_multiset(&once);
        for (text, n) in &out_c {
            if *n > src_c.get(text).copied().unwrap_or(0) {
                fails.push(format!("comment invented/duplicated in {short}: {text:?}"));
            }
        }
        // ...and drops none: every source comment survives (formatting must
        // preserve comments, not just avoid inventing them).
        for (text, n) in &src_c {
            if *n > out_c.get(text).copied().unwrap_or(0) {
                fails.push(format!("comment dropped in {short}: {text:?}"));
            }
        }
    }

    assert!(
        fails.is_empty(),
        "{} of {} real files failed:\n{}",
        fails.len(),
        files.len(),
        fails.join("\n")
    );
}
