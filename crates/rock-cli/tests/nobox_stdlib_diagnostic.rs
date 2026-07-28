//! A `nobox` rejection that lands inside an inlined stdlib body must still be
//! readable: the stdlib source has to be registered with the renderer (it is
//! parsed under `<bone-stdlib:NAME>`, which no project source set contains), and
//! the message has to name the function being emitted -- otherwise every
//! `xs.push(..)` site in a project reports as the same `unified.rock` line and
//! the diagnostic carries no information about WHICH site is at fault.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

// A nobox field inside a >4-leaf value struct: the Vec keeps boxed elements, so
// the element ctor materializes the nobox field and the guarantee fires. The
// rejection surfaces inside the inlined `Vec<T>.push` body in stdlib.
const NOBOX_IN_STDLIB_PUSH: &str = r#"
nobox value NB { x: float, y: float }
value E5 { pos: NB, a: float, b: float, c: float }
global XS: [E5]
fn setup() { XS = vec_new() }
fn main() {
    setup()
    XS.push(E5 { pos: NB { x: 1.0, y: 2.0 }, a: 1.0, b: 2.0, c: 3.0 })
    print("{XS.len}")
}
"#;

fn compile_stderr(src: &str) -> String {
    let dir = std::env::temp_dir().join("rock_nobox_stdlib_diag");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("stick.toml"),
        "[package]\nname = \"nobox_diag\"\nversion = \"0.1.0\"\ntype = \"bin\"\n",
    )
    .unwrap();
    let f = dir.join("a.rock");
    std::fs::write(&f, src).unwrap();
    let out = Command::new(rock_bin()).args(["compile", f.to_str().unwrap()]).output().unwrap();
    assert!(!out.status.success(), "the nobox guarantee must reject this program");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_nobox_rejection_inside_stdlib_renders_its_snippet() {
    let stderr = compile_stderr(NOBOX_IN_STDLIB_PUSH);
    assert!(
        !stderr.contains("Unable to fetch source"),
        "stdlib sources must be registered with the renderer:\n{stderr}"
    );
    // The snippet itself, not just the file id -- an unregistered source drops
    // the source line and every label while still printing the header.
    assert!(
        stderr.contains("Vec<T>.push"),
        "the stdlib source line must be rendered, not silently dropped:\n{stderr}"
    );
}

#[test]
fn a_nobox_rejection_names_the_function_being_emitted() {
    let stderr = compile_stderr(NOBOX_IN_STDLIB_PUSH);
    assert!(
        stderr.contains("main"),
        "the rejection must name the enclosing function -- the span points at \
         the inlined stdlib body, so without it every push site is \
         indistinguishable:\n{stderr}"
    );
}
