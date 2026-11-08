//! A stdlib that does not parse is a reported error naming the file, never a
//! panic: the stdlib can be read from a directory a developer is editing.
use std::path::Path;
use std::process::{Command, Output};

fn invoke(root: &Path, stdlib: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root)
        .args(args)
        .env("ROCK_DIR", root)
        .env("ROCK_INVOCATION_LOG", "off")
        .env("ROCK_STDLIB_DIR", stdlib)
        .output()
        .expect("start rock")
}

fn text(out: &Output) -> String {
    test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

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

    let out = invoke(root.path(), stdlib.path(), &["check", "main.rock"]);
    let output = text(&out);
    assert!(!out.status.success(), "{output}");
    assert!(!output.contains("panicked"), "{output}");
    assert!(output.contains("vec.rock"), "{output}");
}
