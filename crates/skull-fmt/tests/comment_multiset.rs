//! Formatting must keep every comment: over the repository's Rock sources and
//! the formatter corpus, the multiset of comments in the output equals the one
//! in the input. Comment placement is covered by the corpus; this catches the
//! silent deletion a fixed expected output only pins down for known shapes.

use rock_core::source_code::Dialect;
use rock_tokenizer::{TokenKind, Tokenizer};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn comments(src: &str) -> BTreeMap<String, usize> {
    let tokens = Tokenizer::new(src, "<comments>".into())
        .tokenize()
        .expect("tokenizing a source that formatted must succeed");
    let mut out = BTreeMap::new();
    for token in tokens {
        if let TokenKind::Comment(text) = token.kind {
            *out.entry(text.as_str().trim().to_string()).or_insert(0) += 1;
        }
    }
    out
}

/// What the output lost and gained relative to the input, or `None` when the
/// two hold the same comments.
fn comment_diff(input: &str, output: &str) -> Option<String> {
    let (before, after) = (comments(input), comments(output));
    if before == after {
        return None;
    }
    let lost = before
        .iter()
        .filter(|(text, n)| after.get(*text).unwrap_or(&0) < n)
        .map(|(text, _)| text.as_str())
        .collect::<Vec<_>>();
    let gained = after
        .iter()
        .filter(|(text, n)| before.get(*text).unwrap_or(&0) < n)
        .map(|(text, _)| text.as_str())
        .collect::<Vec<_>>();
    Some(format!("lost {lost:?}, gained {gained:?}"))
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir)
        .expect("read a source directory")
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rock") {
            out.push(path);
        }
    }
}

fn format_any_dialect(src: &str) -> Option<String> {
    let cfg = skull_fmt::FormatConfig::default();
    skull_fmt::try_format_with_dialect(src, &cfg, Dialect::Legacy)
        .or_else(|_| skull_fmt::try_format_with_dialect(src, &cfg, Dialect::V1))
        .ok()
}

#[track_caller]
fn assert_keeps_comments(src: &str) {
    let formatted = format_any_dialect(src).unwrap_or_else(|| panic!("must format:\n{src}"));
    if let Some(diff) = comment_diff(src, &formatted) {
        panic!("{diff}\ninput:\n{src}\noutput:\n{formatted}");
    }
    assert_eq!(
        format_any_dialect(&formatted).as_deref(),
        Some(formatted.as_str()),
        "not a fixed point"
    );
}

#[test]
fn parameter_lists_keep_every_comment() {
    for src in [
        "fn f(\n    // before first\n    x: int,\n    y: int,\n) {\n}\n",
        "fn f(\n    // only\n    x: int,\n) {\n}\n",
        "fn f(\n    x: int,\n    // between\n    y: int,\n) {\n}\n",
        "fn f(\n    x: int,\n    y: int,\n    // after last\n) {\n}\n",
        "fn f(\n    x: int,\n    y: int\n    // after last, no comma\n) {\n}\n",
        "fn f(\n    // nothing here\n) {\n}\n",
        "fn f(\n    /* block before */\n    x: int,\n    /* block after */\n) {\n}\n",
        "fn f( // on the open paren\n    x: int\n) {\n}\n",
        "fn f(\n    x: int, /* inline block */\n    y: int\n) {\n}\n",
        "fn f(x: int) // after the close paren\n{\n}\n",
        "fn f(\n    x: int\n) -> /* in the return */ int {\n    x\n}\n",
        "fn f(x: int)\n// between signature and body\n{\n}\n",
        "struct S {}\nfn S.m(\n    // receiver\n    self,\n    // other\n    y: int,\n    // last\n) {\n}\n",
        "fn f() {\n    fn g(\n        // nested first\n        x: int,\n        // nested last\n    ) {\n    }\n}\n",
        "fn f() {\n    fn g(x: int) // nested close\n    {\n    }\n}\n",
    ] {
        assert_keeps_comments(src);
    }
}

#[test]
fn empty_bodies_keep_their_comments() {
    for src in [
        "struct S {\n    // empty\n}\n",
        "struct S {\n    /* empty */\n}\n",
        "struct S { /* inline */ }\n",
        "enum E {\n    // empty\n}\n",
    ] {
        assert_keeps_comments(src);
    }
}

#[test]
fn repository_sources_keep_every_comment() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["tests", "crates/rock-stdlib", "crates/gamelibs"] {
        collect(&root.join(dir), &mut files);
    }
    files.sort();
    let mut checked = 0;
    let mut failures = Vec::new();
    for file in &files {
        let src = fs::read_to_string(file).expect("read a Rock source");
        // Negative fixtures do not parse; the formatter leaves those alone.
        let Some(formatted) = format_any_dialect(&src) else {
            continue;
        };
        checked += 1;
        if let Some(diff) = comment_diff(&src, &formatted) {
            failures.push(format!("{}: {diff}", file.display()));
        }
    }
    assert!(checked > 100, "only {checked} sources formatted");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn formatter_corpus_keeps_every_comment() {
    let mut files = Vec::new();
    crate::corpus::collect_fmt_files(&crate::corpus::corpus_dir(), &mut files);
    files.sort();
    let mut failures = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("read a corpus file");
        for case in crate::corpus::parse_cases(&file.display().to_string(), &text) {
            let Some(formatted) = format_any_dialect(&case.input) else {
                continue;
            };
            if let Some(diff) = comment_diff(&case.input, &formatted) {
                failures.push(format!("{} :: {}: {diff}", case.file, case.name));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
