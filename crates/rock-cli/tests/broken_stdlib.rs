//! A stdlib that does not parse is a reported error naming the file, never a
//! panic: the stdlib can be read from a directory a developer is editing.
use crate::support::{rock, text};
use std::path::Path;

/// A copy of the checkout's stdlib with `text` appended to `vec.rock`.
fn stdlib_with(dir: &Path, text: &str) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../stdlib");
    for name in rock_stdlib::STDLIB_NAMES {
        let mut contents = std::fs::read_to_string(source.join(name)).unwrap();
        if *name == "vec.rock" {
            contents.push_str(text);
        }
        std::fs::write(dir.join(name), contents).unwrap();
    }
}

#[test]
fn check_reports_a_stdlib_that_does_not_tokenize() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rock"), "fn main() {\n}\n").unwrap();
    let stdlib = tempfile::tempdir().unwrap();
    stdlib_with(
        stdlib.path(),
        "\nfn broken() {\n    let s = \"unterminated\n}\n",
    );

    let out = rock(root.path())
        .args(["check", "main.rock"])
        .env("ROCK_STDLIB_DIR", stdlib.path())
        .output()
        .unwrap();
    let output = text(&out);
    assert!(!out.status.success(), "{output}");
    assert!(!output.contains("panicked"), "{output}");
    assert!(output.contains("vec.rock"), "{output}");
}
