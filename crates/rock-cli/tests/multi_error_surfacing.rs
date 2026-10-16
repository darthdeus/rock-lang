//! Regression: `rock compile`/`run` must report EVERY diagnostic, not just the
//! first. `bone_backend::body_errors` already collects all errors source-sorted,
//! but the CLI verdict funnel collapsed to `.next()` -- so a project with 5 type
//! errors forced a fix-one-recompile-repeat loop the LSP never needed.
//!
//! Also pins the single-file skull render path resolving spans: `skull_emit_full`
//! parses with the real path as the file id, so the renderer must register the
//! source under that same id or every span lookup fails ("Unable to fetch
//! source") and the snippet/labels are silently dropped.


// Two INDEPENDENT type errors in separate fn bodies, with distinct type pairs
// so each rendered error is identifiable by substring.
const TWO_ERRORS: &str = "fn first_bad() -> int {\n    \"not an int\"\n}\n\nfn second_bad() -> int {\n    true\n}\n";

fn compile_stderr(path: &std::path::Path, backend: Option<&str>) -> (bool, String) {
    let mut cmd = crate::command::rock_bin();
    cmd.args(["compile", path.to_str().unwrap()]);
    if let Some(b) = backend {
        cmd.args(["--backend", b]);
    }
    let out = cmd.output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn write_tmp(tag: &str, name: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_multi_err_{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    // Multi-file dirs need a stick.toml for project discovery.
    std::fs::write(
        dir.join("stick.toml"),
        format!("[package]\nname = \"{tag}\"\nversion = \"0.1.0\"\ntype = \"bin\"\n"),
    )
    .unwrap();
    let f = dir.join(name);
    std::fs::write(&f, src).unwrap();
    f
}

fn assert_both_errors(stderr: &str, ctx: &str) {
    assert!(
        stderr.contains("string"),
        "{ctx}: first error (string vs int) missing from stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("bool"),
        "{ctx}: second error (bool vs int) missing from stderr -- only the \
         first diagnostic was reported:\n{stderr}"
    );
}

#[test]
fn skull_compile_reports_every_type_error() {
    let f = write_tmp("skull_single", "a.rock", TWO_ERRORS);
    let (ok, stderr) = compile_stderr(&f, None);
    assert!(!ok, "two type errors must fail the compile");
    assert_both_errors(&stderr, "skull single-file");
}

#[test]
fn bone_compile_reports_every_type_error() {
    let f = write_tmp("bone_single", "a.rock", TWO_ERRORS);
    let (ok, stderr) = compile_stderr(&f, Some("bone"));
    assert!(!ok, "two type errors must fail the compile");
    assert_both_errors(&stderr, "bone single-file");
}

#[test]
fn project_compile_reports_errors_from_every_file() {
    let one = write_tmp("skull_proj", "one.rock", "fn f() -> int {\n    \"not an int\"\n}\n");
    write_tmp("skull_proj", "two.rock", "fn g() -> int {\n    true\n}\n");
    let dir = one.parent().unwrap().to_path_buf();

    let (ok, stderr) = compile_stderr(&dir, None);
    assert!(!ok, "a project with type errors must fail the compile");
    assert_both_errors(&stderr, "skull project");

    let (ok, stderr) = compile_stderr(&dir, Some("bone"));
    assert!(!ok, "a project with type errors must fail the compile");
    assert_both_errors(&stderr, "bone project");
}

#[test]
fn project_compile_reports_every_syntax_error() {
    // Recoverable syntax errors in two files: both must surface, same rule as
    // type errors (the old gate was `first_syntax_error`).
    let one = write_tmp(
        "syn_proj",
        "one.rock",
        "fn syn_one() {\n    call_one(1))\n}\nfn call_one(x: int) {}\n",
    );
    write_tmp(
        "syn_proj",
        "two.rock",
        "fn syn_two() {\n    call_two(2))\n}\nfn call_two(x: int) {}\n",
    );
    let dir = one.parent().unwrap().to_path_buf();

    let (ok, stderr) = compile_stderr(&dir, None);
    assert!(!ok, "syntax errors must fail the compile");
    assert!(
        stderr.contains("one.rock") && stderr.contains("two.rock"),
        "both files' syntax errors must be reported:\n{stderr}"
    );
}

#[test]
fn skull_single_file_render_resolves_spans() {
    let f = write_tmp("skull_snippet", "a.rock", TWO_ERRORS);
    let (_, stderr) = compile_stderr(&f, None);
    assert!(
        !stderr.contains("Unable to fetch source"),
        "error spans must resolve against the registered source:\n{stderr}"
    );
    assert!(
        stderr.contains("not an int"),
        "the rendered error must include the offending source line:\n{stderr}"
    );
}
