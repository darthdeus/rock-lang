//! Regression: a syntax error the bone/skull front end "recovers" from must
//! still be reported, not silently swallowed.
//!
//! skull-parser has statement- and item-level error recovery (for LSP graceful
//! degradation) that records the dropped tokens in `Ast.recovery_errors`. No
//! consumer reads that field, so genuinely-invalid code -- e.g. an extra
//! trailing `)` after a call -- parses clean on the bone backend even though
//! rock's parser rejects the same input. (Found in the wild: `mag/targetting.rock`
//! `journal_log(msg, hue))` compiled silently.)
//!
//! Drives the real CLI (`rock compile`) so the test is agnostic to *where* the
//! fix surfaces the recovery error (parser, CLI consumer, or check pass).
//! `compile` type-checks via the bone front end on the default (skull) backend
//! and on `--backend bone`; a parse error must make it exit non-zero.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo for binary integration tests.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn write_tmp(tag: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_paren_repro_{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("a.rock");
    std::fs::write(&f, src).unwrap();
    f
}

// An extra trailing `)` after a complete call -- one unbalanced close paren.
const BAD: &str = "fn foo(x: int) -> int {\n    x\n}\n\nfn bar() {\n    foo(1))\n}\n";
const GOOD: &str = "fn foo(x: int) -> int {\n    x\n}\n\nfn bar() {\n    foo(1)\n}\n";

fn compile_exit_ok(path: &std::path::Path, backend: Option<&str>) -> bool {
    let mut cmd = Command::new(rock_bin());
    cmd.args(["compile", path.to_str().unwrap()]);
    if let Some(b) = backend {
        cmd.args(["--backend", b]);
    }
    cmd.output().unwrap().status.success()
}

#[test]
fn default_backend_rejects_extra_close_paren() {
    // Sanity anchor: the default (skull) front end rejects this.
    let bad = write_tmp("default_bad", BAD);
    assert!(
        !compile_exit_ok(&bad, None),
        "default backend should reject an extra trailing `)`"
    );
}

#[test]
fn bone_backend_rejects_extra_close_paren() {
    let good = write_tmp("bone_good", GOOD);
    let bad = write_tmp("bone_bad", BAD);
    assert!(
        compile_exit_ok(&good, Some("bone")),
        "valid code must still compile on the bone backend"
    );
    assert!(
        !compile_exit_ok(&bad, Some("bone")),
        "bone backend must reject an extra trailing `)`: skull-parser recovery \
         swallows it into Ast.recovery_errors, which no consumer surfaces"
    );
}

// The same swallow class at ITEM level: only `fn` parse errors used to be
// recorded; a malformed struct/enum/global was dropped with no recovery entry.
const MALFORMED_ITEMS: &[(&str, &str)] = &[
    ("struct", "struct S {\n    field:\n}\n\nfn main() {}\n"),
    ("enum", "enum E {\n    A,,\n    B\n}\n\nfn main() {}\n"),
    ("global", "global X int = 1\n\nfn main() {}\n"),
];

#[test]
fn bone_backend_rejects_malformed_non_fn_items() {
    for (kind, src) in MALFORMED_ITEMS {
        let f = write_tmp(&format!("bone_item_{kind}"), src);
        assert!(
            !compile_exit_ok(&f, Some("bone")),
            "bone backend must reject a malformed `{kind}` item -- non-fn item \
             parse errors were dropped without a recovery_errors entry"
        );
    }
}
