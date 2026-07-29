//! A `nobox` rejection must name the function being emitted. The span can point
//! into an inlined stdlib body, where every `xs.push(..)` site in a project
//! reports as the same `unified.rock` line -- without the enclosing function the
//! diagnostic carries no information about WHICH site was at fault.
//!
//! The other half of the original defect -- a rejection landing in stdlib losing
//! its snippet and labels, because `<bone-stdlib:NAME>` is in no project source
//! set -- is covered at the RENDERER level by
//! `bone_backend::tests::a_diagnostic_spanned_into_stdlib_renders_its_snippet`.
//! It cannot be covered from here: where a rejection lands is a property of
//! lowering, and normalization already moved this program's rejection out of
//! stdlib onto the user's own ctor.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

// A nobox field inside a GC-TIER outer (the nullable field makes `Opt` emit as a
// `_vt` table), so there is no cdata to write leaves into and the nested ctor
// still materializes.
const NOBOX_IN_GC_TIER_OUTER: &str = r#"
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
    let stderr = compile_stderr(NOBOX_IN_GC_TIER_OUTER);
    assert!(
        stderr.contains("main"),
        "the rejection must name the enclosing function -- otherwise sites that \
         share an inlined body are indistinguishable:\n{stderr}"
    );
}
