//! Formatting must keep every comment. `try_format` refuses output whose
//! comments differ from the input's, so a source that parses but does not
//! format is a comment the printer lost.

use rock_core::source_code::Dialect;
use std::fs;
use std::path::{Path, PathBuf};

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

/// The formatted source, or the error from the dialect that parses it;
/// `None` when it parses in neither.
fn format_any_dialect(src: &str) -> Option<Result<String, String>> {
    let cfg = skull_fmt::FormatConfig::default();
    [Dialect::Legacy, Dialect::V1]
        .into_iter()
        .find_map(|dialect| {
            skull_parser::parse_module_clean_with_dialect(src, "<src>".into(), dialect)
                .ok()
                .map(|_| skull_fmt::try_format_with_dialect(src, &cfg, dialect))
        })
}

#[track_caller]
fn assert_keeps_comments(src: &str) {
    let formatted = match format_any_dialect(src) {
        Some(Ok(formatted)) => formatted,
        other => panic!("must format: {other:?}\n{src}"),
    };
    assert_eq!(
        format_any_dialect(&formatted),
        Some(Ok(formatted.clone())),
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
        match format_any_dialect(&src) {
            None => continue,
            Some(Ok(_)) => {}
            Some(Err(e)) => failures.push(format!("{}: {e}", file.display())),
        }
        checked += 1;
    }
    assert!(checked > 100, "only {checked} sources formatted");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
