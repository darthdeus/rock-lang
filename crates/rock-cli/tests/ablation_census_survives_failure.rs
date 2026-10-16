//! The Stage-0e ablation COLLECTS rather than aborting
//! (`docs/monomorphization-plan.md` S 0e, `skull_mir::ablation`), so its census
//! is evidence about a whole run and must print however the build ended.
//!
//! `std::process::exit` runs no destructors and returns to no caller, so a path
//! that calls it skips the drain in `main`. The paths that call it are the ones
//! a build under the mode DIES on, which inverted the mode: a project that
//! happened to compile reported a full taxonomy, and a project that failed
//! reported nothing at all. The `Iterator`-bounded surface is entirely in the
//! second group, so its category read zero on every project in the corpus while
//! the fatal was being recorded correctly the whole time.
//!
//! Drives the real CLI, because the defect is in how the process exits and
//! nothing below that layer can observe it.

fn write_tmp(tag: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_ablation_census_{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("a.rock");
    std::fs::write(&f, src).unwrap();
    f
}

/// The census banner. Counted rather than matched, because "a census printed"
/// and "exactly one census printed" are different claims: a duplicate is what a
/// `fork` child reporting its inherited registry looks like.
const BANNER: &str = "Stage-0e totality ablation";

/// Compile with the mode on or off, returning (exit_ok, combined output).
///
/// The emitted Lua goes to a real file beside the fixture rather than to
/// `/dev/null`, so the census -- which is on STDOUT, the same stream a
/// `--output -` would take -- is read without the artifact mixed into it.
fn compile(path: &std::path::Path, ablated: bool) -> (bool, String) {
    let mut cmd = crate::command::rock_bin();
    cmd.args([
        "compile",
        path.to_str().unwrap(),
        "--output",
        path.with_extension("lua").to_str().unwrap(),
    ]);
    // Set or REMOVE it: the ambient environment decides this otherwise, and a
    // mode-off control that inherited the flag would assert nothing.
    if ablated {
        cmd.env("ROCK_MONO_ABLATION", "1");
    } else {
        cmd.env_remove("ROCK_MONO_ABLATION");
    }
    let out = cmd.output().unwrap();
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

fn census_count(out: &str) -> usize {
    out.matches(BANNER).count()
}

/// Polymorphic recursion instantiates one level deeper per call, so under the
/// mode the build FAILS -- and the census must still print, naming the
/// mechanism.
///
/// This fixture is the whole point: it is the shape whose build dies. A fixture
/// that compiled would pass against the defect, because the drain in `main` runs
/// on that path.
///
/// It is a `polymorphic-growth` shape because that is an S 0h REJECT, permanent
/// by decision. The `Iterator`-bounded shape that used to stand here stopped
/// failing the moment D-1 gave a bounded body a standalone emission -- a fixture
/// chosen from the open gap matrix disarms itself when the gap closes.
const DYING: &str = r#"
fn depth<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + depth(wrapped, n - 1)
}

fn main() {
    print("{depth(1, 12)}")
}
"#;

/// A generic call that specializes cleanly: the build SUCCEEDS under the mode.
/// The admission control -- it says the census prints on the surviving path too,
/// so a failure above is about the exit route and not about the mode being off.
const CLEAN: &str = r#"
fn ident<T>(x: T) -> T { return x }

fn main() {
    let v = Vec2 { x: 1, y: 2 }
    let a = ident(v)
    print("{a.x}")
}
"#;

#[test]
fn a_failed_build_still_prints_its_census() {
    let path = write_tmp("dying", DYING);
    let (ok, out) = compile(&path, true);
    assert!(
        !ok,
        "the fixture must FAIL to build under the mode, or it cannot witness the \
         exiting path; output:\n{out}"
    );
    assert_eq!(
        census_count(&out),
        1,
        "a build that died must print EXACTLY ONE census -- zero loses every fatal \
         it recorded, and more than one is an inherited registry being reported by \
         something that does not own it (a `fork` child); output:\n{out}"
    );
    assert!(
        out.contains("polymorphic-growth"),
        "the census printed but does not name the mechanism the run recorded; \
         output:\n{out}"
    );
}

#[test]
fn a_surviving_build_prints_its_census() {
    let path = write_tmp("clean", CLEAN);

    // MODE OFF FIRST, and asserted rather than described. This is what makes
    // the ablated run below a statement about the mode: the fixture compiles,
    // so its failure under the mode is the mode's doing and not a broken
    // fixture. It was previously only a manual measurement in a comment, which
    // is not a control -- nothing would have caught the fixture rotting.
    let (plain_ok, plain_out) = compile(&path, false);
    assert!(
        plain_ok,
        "the fixture must BUILD with the mode off, or its ablated failure says \
         nothing about the mode; output:\n{plain_out}"
    );
    assert_eq!(
        census_count(&plain_out),
        0,
        "the mode is off and a census printed anyway; output:\n{plain_out}"
    );

    let (ok, out) = compile(&path, true);
    assert_eq!(
        census_count(&out),
        1,
        "a completed build must print EXACTLY ONE census; output:\n{out}"
    );
    assert!(
        !out.contains("polymorphic-growth"),
        "a fixture with no polymorphic recursion reported that mechanism, so the \
         assertion above cannot distinguish; output:\n{out}"
    );
    // S 0e's fatality is carried by the STATUS, so a build that SUCCEEDS (just
    // asserted) must still leave a failing run.
    //
    // This arm rides `main`'s own drain, not `exit_with_census` -- a build that
    // succeeds returns to `main` rather than exiting early, which is the whole
    // reason the surviving path always worked. `census_exit_code`'s unit tests
    // are the coverage for the exiting path; naming this one as that coverage
    // would be claiming a control I have not got.
    assert!(
        !ok,
        "the run recorded fatals and exited 0, so a green suite would hide \
         them; output:\n{out}"
    );
}
