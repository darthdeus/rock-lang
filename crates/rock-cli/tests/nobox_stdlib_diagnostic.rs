//! A `nobox` rejection must render its snippet and name the function being
//! emitted. Both were missing: stdlib modules are parsed under
//! `<bone-stdlib:NAME>`, which no project source set contains, so a rejection
//! landing in an inlined stdlib body dropped its snippet and every label; and
//! without the enclosing function every `xs.push(..)` site in a project reported
//! as the same `unified.rock` line, carrying no information about WHICH site was
//! at fault.
//!
//! The fixture no longer lands IN stdlib: nested-ctor normalization fixed the
//! FFI-clean case that used to, so what remains (a GC-tier outer) boxes at the
//! user's own ctor. The stdlib registration in `render_errs` is therefore no
//! longer covered end-to-end here -- re-point this fixture if a stdlib-landing
//! rejection reappears.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

// A nobox field inside a GC-TIER outer (the nullable field makes `Opt` emit as a
// `_vt` table), so there is no cdata to write leaves into and the nested ctor
// still materializes. The rejection surfaces inside the inlined `Vec<T>.push`
// body in stdlib, which is what this file is about -- an FFI-clean outer is
// flattened by normalization now and no longer rejects.
const NOBOX_IN_STDLIB_PUSH: &str = r#"
nobox value NB { x: float, y: float }
value Opt { pos: NB?, a: float, b: float, c: float }
global XS: [Opt]
fn setup() { XS = vec_new() }
fn main() {
    setup()
    XS.push(Opt { pos: NB { x: 1.0, y: 2.0 }, a: 1.0, b: 2.0, c: 3.0 })
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
        stderr.contains("XS.push"),
        "the source line must be rendered, not silently dropped:\n{stderr}"
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
