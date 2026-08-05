//! `rock lint` + the build-integrated lint pass, driven through the real binary.
//!
//! Covers the surface contract: findings reach the author from `rock lint` AND
//! from an ordinary build, `--no-lints`/`--lint-summary` change only what is
//! printed, a `Warning`-tier finding never fails a build, and `--union` judges a
//! lib against every consumer at once. The Error-gate logic is unit-tested in
//! `lint_pass`.

use std::path::Path;
use std::process::{Command, Output};

/// A project whose `main.rock` has one unused local and one dead function.
fn project(dir: &Path) {
    std::fs::write(dir.join("stick.toml"), "[package]\nname = \"lintee\"\n").unwrap();
    std::fs::write(
        dir.join("main.rock"),
        "fn never_called() -> int {\n    1\n}\n\nfn main() {\n    let unread = 5\n    print(\"hi\")\n}\n",
    )
    .unwrap();
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rock")).args(args).output().expect("spawn rock")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn lint_reports_findings_and_exits_zero_on_warnings() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let out = run(&["lint", dir.path().to_str().unwrap()]);
    let err = stderr(&out);
    assert!(out.status.success(), "a Warning-tier finding must not fail `rock lint`: {err}");
    assert!(err.contains("unused variable `unread`"), "{err}");
    assert!(err.contains("unused function `never_called`"), "{err}");
    assert!(err.contains("2 finding(s)"), "the tally is reported: {err}");
}

#[test]
fn a_build_runs_the_lint_pass_and_still_succeeds() {
    // Lints are a compiler pass: an ordinary `rock compile` reports them. The
    // build still succeeds -- only `Error` severity gates.
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let out = run(&[
        "compile",
        dir.path().to_str().unwrap(),
        dir.path().join("out.lua").to_str().unwrap(),
    ]);
    let err = stderr(&out);
    assert!(out.status.success(), "warnings never fail a build: {err}");
    assert!(err.contains("unused variable `unread`"), "the build renders findings: {err}");
    assert!(dir.path().join("out.lua").exists(), "the build still produced output");
}

#[test]
fn no_lints_silences_the_soft_tiers_in_a_build() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let out = run(&[
        "compile",
        dir.path().to_str().unwrap(),
        dir.path().join("out.lua").to_str().unwrap(),
        "--no-lints",
    ]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(!err.contains("unused variable"), "--no-lints drops warnings: {err}");
}

#[test]
fn lint_summary_compacts_the_soft_tiers_to_one_line() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let out = run(&[
        "compile",
        dir.path().to_str().unwrap(),
        dir.path().join("out.lua").to_str().unwrap(),
        "--lint-summary",
    ]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("lint warning(s)"), "the count line is printed: {err}");
    assert!(!err.contains("unused variable"), "no per-finding render: {err}");
}

#[test]
fn allow_comment_suppresses_a_finding_on_every_surface() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    std::fs::write(
        dir.path().join("main.rock"),
        "// rock: allow(unused_fn)\nfn never_called() -> int {\n    1\n}\n\n\
         fn main() {\n    // rock: allow(unused_var)\n    let unread = 5\n    print(\"hi\")\n}\n",
    )
    .unwrap();
    let out = run(&["lint", dir.path().to_str().unwrap()]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("no findings"), "both findings are suppressed: {err}");
}

#[test]
fn lint_rejects_a_program_that_does_not_typecheck() {
    // A broken program has no trustworthy HIR to lint -- report the real error.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"broken\"\n").unwrap();
    std::fs::write(dir.path().join("main.rock"), "fn main() {\n    let x: int = \"nope\"\n}\n")
        .unwrap();
    let out = run(&["lint", dir.path().to_str().unwrap()]);
    assert!(!out.status.success(), "a type error fails `rock lint`");
    assert!(stderr(&out).contains("does not type-check"), "{}", stderr(&out));
}

/// A library plus two consumers that each call a different half of it, and one
/// function nobody calls anywhere.
fn union_repo(dir: &Path) {
    let engine = dir.join("engine");
    std::fs::create_dir_all(&engine).unwrap();
    std::fs::write(engine.join("stick.toml"), "[package]\nname = \"engine\"\ntype = \"lib\"\n")
        .unwrap();
    std::fs::write(
        engine.join("e.rock"),
        "fn used_by_a() -> int {\n    1\n}\n\nfn used_by_b() -> int {\n    2\n}\n\nfn dead_everywhere() -> int {\n    3\n}\n",
    )
    .unwrap();

    for (name, call) in [("game_a", "used_by_a"), ("game_b", "used_by_b")] {
        let g = dir.join(name);
        std::fs::create_dir_all(&g).unwrap();
        std::fs::write(
            g.join("stick.toml"),
            format!("[package]\nname = \"{name}\"\ntype = \"bin\"\n\n[dependencies]\nengine = {{ path = \"../engine\" }}\n"),
        )
        .unwrap();
        std::fs::write(
            g.join("main.rock"),
            format!("fn main() {{\n    print(\"{{{call}()}}\")\n}}\n"),
        )
        .unwrap();
    }
}

// A lib's reachability is only meaningful against ALL of its consumers at once.
// Per-consumer it is meaningless: `used_by_a` is uncalled in game_b and vice
// versa, yet neither is dead. Only the intersection is a real finding.
#[test]
fn union_reports_only_what_is_dead_in_every_consumer() {
    let dir = tempfile::tempdir().unwrap();
    union_repo(dir.path());
    let out = run(&["lint", "--union", dir.path().join("engine").to_str().unwrap()]);
    let err = stderr(&out);
    assert!(
        err.contains("unused function `dead_everywhere`"),
        "a fn no consumer calls is dead: {err}"
    );
    assert!(
        !err.contains("`used_by_a`") && !err.contains("`used_by_b`"),
        "a fn ANY consumer calls is live -- per-consumer absence is not death: {err}"
    );
}

// The intersection is only sound if every consumer was actually analyzed: a
// consumer that failed to typecheck might have been the sole caller. Silently
// dropping it would turn live code into a "finding".
#[test]
fn union_names_the_consumers_it_could_not_analyze() {
    let dir = tempfile::tempdir().unwrap();
    union_repo(dir.path());
    let broken = dir.path().join("game_c");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(
        broken.join("stick.toml"),
        "[package]\nname = \"game_c\"\ntype = \"bin\"\n\n[dependencies]\nengine = { path = \"../engine\" }\n",
    )
    .unwrap();
    std::fs::write(broken.join("main.rock"), "fn main() { this_does_not_typecheck() }\n").unwrap();

    let out = run(&["lint", "--union", dir.path().join("engine").to_str().unwrap()]);
    let err = stderr(&out);
    assert!(err.contains("game_c"), "an unanalyzable consumer must be named: {err}");
}

/// A project whose `main.rock` declares one prefix-form fn over a user struct.
fn method_project(dir: &Path) {
    std::fs::write(dir.join("stick.toml"), "[package]\nname = \"methodee\"\n").unwrap();
    std::fs::write(
        dir.join("main.rock"),
        "struct Mob { hp: int }\n\nfn damage(m: Mob, n: int) {\n    m.hp -= n\n}\n\nfn main() {\n    let mob = Mob { hp: 3 }\n    damage(mob, 1)\n}\n",
    )
    .unwrap();
}

#[test]
fn method_syntax_is_off_until_the_invocation_names_it() {
    // The opt-in contract end to end: a migration lint reaches the author only
    // through `--lints=`, so a build and a bare `rock lint` stay quiet while a
    // project is converted a file at a time.
    let dir = tempfile::tempdir().unwrap();
    method_project(dir.path());
    let path = dir.path().to_str().unwrap();

    let bare = stderr(&run(&["lint", path]));
    assert!(!bare.contains("method_syntax"), "bare `rock lint` must not run it: {bare}");

    let out = run(&["lint", path, "--lints=method_syntax"]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("declare it as `fn Mob.damage(m, n: int)`"), "{err}");
    assert!(err.contains("1 method_syntax"), "the tally names the lint: {err}");
    assert!(!err.contains("unused"), "a selection silences every other lint: {err}");
}

#[test]
fn an_unknown_lint_name_is_rejected_rather_than_running_nothing() {
    let dir = tempfile::tempdir().unwrap();
    method_project(dir.path());
    let out = run(&["lint", dir.path().to_str().unwrap(), "--lints=method_sytnax"]);
    assert!(!out.status.success(), "a typo must not report a clean project");
    assert!(stderr(&out).contains("unknown lint(s): method_sytnax"), "{}", stderr(&out));
}

#[test]
fn ignore_types_narrows_the_sweep_and_names_a_miss() {
    // Converting happens one type at a time, so the sweep narrows from the
    // invocation. A misspelled entry fails OPEN -- the type keeps showing up --
    // so it is named rather than silently ignored.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"methodee\"\n").unwrap();
    std::fs::write(
        dir.path().join("main.rock"),
        "struct Mob { hp: int }\nstruct Color { r: int }\n\nfn damage(m: Mob, n: int) {\n    m.hp -= n\n}\n\nfn tint(c: Color, n: int) -> int {\n    n\n}\n\nfn main() {\n    let mob = Mob { hp: 3 }\n    damage(mob, 1)\n    let _ = tint(Color { r: 1 }, 2)\n}\n",
    )
    .unwrap();
    let path = dir.path().to_str().unwrap();

    let all = stderr(&run(&["lint", path, "--lints=method_syntax"]));
    assert!(all.contains("2 method_syntax"), "{all}");

    let narrowed = stderr(&run(&["lint", path, "--lints=method_syntax", "--ignore-types=Color"]));
    assert!(narrowed.contains("1 method_syntax"), "{narrowed}");
    assert!(narrowed.contains("fn Mob.damage"), "{narrowed}");
    assert!(!narrowed.contains("fn Color.tint"), "{narrowed}");

    let typo =
        stderr(&run(&["lint", path, "--lints=method_syntax", "--ignore-types=Color,Colour"]));
    assert!(typo.contains("names no type in this project: Colour"), "{typo}");
}
