//! A `nobox` rejection must name the function being emitted. The span can point
//! into an inlined stdlib body, where every `xs.push(..)` site in a project
//! reports as the same `unified.rock` line -- without the enclosing function the
//! diagnostic carries no information about WHICH site was at fault.
//!
//! The other half of the original defect -- a rejection landing in stdlib losing
//! its snippet and labels, because the stdlib module is in no project source
//! set -- is covered at the RENDERER level by
//! `bone_backend::tests::a_diagnostic_spanned_into_stdlib_renders_its_snippet`.
//! It cannot be covered from here: where a rejection lands is a property of
//! lowering, and normalization already moved this program's rejection out of
//! stdlib onto the user's own ctor.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

// Interpolating a WHOLE tuple that carries a nobox value: that asks for one
// runtime-typed value, so the element materializes with nowhere else to go.
//
// A tuple element in a COLLECTION used to stand here; it stopped materializing
// when the box learned to carry the leaves. This sink is F1 erasure, which is
// permanent by design -- prefer it over any row the case matrix lists as live.
const NOBOX_IN_A_TUPLE_ELEMENT: &str = r#"
nobox value NB { x: float, y: float }
global XS: [NB] = []
fn main() {
    XS.push(NB { x: 1.0, y: 2.0 })
    let t = (XS[0], 1.0)
    print("{t}")
}
"#;

fn compile_stderr(src: &str) -> String {
    let dir = std::env::temp_dir().join("rock_nobox_diag_attribution");
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
fn a_nobox_rejection_names_the_function_being_emitted() {
    let stderr = compile_stderr(NOBOX_IN_A_TUPLE_ELEMENT);
    assert!(
        stderr.contains("main"),
        "the rejection must name the enclosing function -- otherwise sites that \
         share an inlined body are indistinguishable:\n{stderr}"
    );
}
