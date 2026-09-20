//! Memory class declared per package, with a per-module override (plan R2/R3,
//! `docs/state-ownership-plan.md` checkpoint S3).
//!
//! Before this, every unannotated mutable global was Session / world=include /
//! fresh-session, so Stone's 817 host-owned globals each carried an identical
//! 87-character `@state` line to say otherwise. The class is now a property of
//! the compilation unit: `[language] memory = "platform"` in `stick.toml` for
//! the package, `@memory(<class>)` as a file header for one module, and an
//! explicit `@state` on a declaration still wins over both.
//!
//! These compile a real package through the `rock` binary and read the
//! `__ROCK_STATE_IMAGE_PLAN` the emitter publishes, which is the same artifact
//! `stone/engine/state_image.rock` consumes at runtime -- so a test that passes
//! proves the shipped plan, not a compiler-internal view of it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

/// One `__ROCK_STATE_IMAGE_PLAN` entry, as the emitted Lua spells it.
#[derive(Debug, PartialEq, Eq)]
struct PlanEntry {
    kind: String,
    capture_policy: String,
    realization_policy: String,
    world_policy: String,
}

fn field<'a>(entry: &'a str, key: &str) -> &'a str {
    let head = format!("{key}=\"");
    let start = entry
        .find(&head)
        .unwrap_or_else(|| panic!("plan entry has no `{key}`: {entry}"))
        + head.len();
    let rest = &entry[start..];
    let end = rest.find('"').expect("unterminated plan field");
    &rest[..end]
}

/// The plan entry for the global named `name`, read out of an emitted artifact.
fn plan_entry(lua: &str, name: &str) -> PlanEntry {
    let line = lua
        .lines()
        .find(|line| line.starts_with("_G.__ROCK_STATE_IMAGE_PLAN = {"))
        .expect("the emitter publishes a StateImagePlan");
    let needle = format!("::{name}\"");
    let entry = line
        .split("{identity=\"")
        .find(|chunk| chunk.starts_with("global:") && chunk.contains(&needle))
        .unwrap_or_else(|| panic!("no plan entry for global `{name}`"));
    PlanEntry {
        kind: field(entry, "kind").to_string(),
        capture_policy: field(entry, "capture_policy").to_string(),
        realization_policy: field(entry, "realization_policy").to_string(),
        world_policy: field(entry, "world_policy").to_string(),
    }
}

fn game() -> PlanEntry {
    PlanEntry {
        kind: "semantic_session".to_string(),
        capture_policy: "graph".to_string(),
        realization_policy: "fresh-session".to_string(),
        world_policy: "include".to_string(),
    }
}

fn platform() -> PlanEntry {
    PlanEntry {
        kind: "host_projection".to_string(),
        capture_policy: "host-owned".to_string(),
        realization_policy: "rebuild-host-projection".to_string(),
        world_policy: "exclude".to_string(),
    }
}

fn debug() -> PlanEntry {
    PlanEntry {
        kind: "runner_diagnostic".to_string(),
        capture_policy: "exclude".to_string(),
        realization_policy: "runner-rebind".to_string(),
        world_policy: "exclude".to_string(),
    }
}

fn stick(name: &str, memory: Option<&str>) -> String {
    let language = match memory {
        Some(class) => format!("\n[language]\nmemory = \"{class}\"\n"),
        None => String::new(),
    };
    format!("[package]\nname = \"{name}\"\ntype = \"bin\"\n{language}\n[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n")
}

/// Write a package and compile it. Returns the emitted Lua, or the compiler's
/// combined output when the build fails.
fn compile(files: &[(&str, &str)]) -> Result<String, String> {
    let dir = tempfile::tempdir().expect("tempdir");
    write_package(dir.path(), files);
    let out = dir.path().join("out.lua");
    let result = Command::new(rock_bin())
        .arg("compile")
        .arg(dir.path())
        .arg("--output")
        .arg(&out)
        .output()
        .expect("run rock compile");
    if !result.status.success() {
        let mut combined = String::from_utf8_lossy(&result.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&result.stderr));
        return Err(combined);
    }
    Ok(std::fs::read_to_string(&out).expect("emitted artifact"))
}

fn write_package(root: &Path, files: &[(&str, &str)]) {
    for (name, contents) in files {
        std::fs::write(root.join(name), contents).expect("fixture file");
    }
}

fn compile_ok(files: &[(&str, &str)]) -> String {
    compile(files).unwrap_or_else(|err| panic!("fixture must compile:\n{err}"))
}

/// Compile and keep the compiler's own output. A lint at `Warning` does not
/// fail the build, so [`compile`]'s error-only capture cannot see one: the
/// whole point of the level is that the package still builds.
fn compile_diagnostics(files: &[(&str, &str)]) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    write_package(dir.path(), files);
    let out = dir.path().join("out.lua");
    let result = Command::new(rock_bin())
        .arg("compile")
        .arg(dir.path())
        .arg("--output")
        .arg(&out)
        .output()
        .expect("run rock compile");
    let mut combined = String::from_utf8_lossy(&result.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&result.stderr));
    assert!(
        result.status.success(),
        "a redundant annotation must stay a WARNING -- other branches merge these \
         lines in and must get a named message, not a broken build:\n{combined}"
    );
    combined
}

/// Run `rock lint` over a package, selecting exactly `lint`. The rendered
/// warning carries the MESSAGE; only `rock lint`'s summary names the lint, and
/// an unknown name rejects -- so this is what proves the name is real,
/// selectable, and therefore suppressible with an `allow` comment.
fn lint_output(files: &[(&str, &str)], lint: &str) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    write_package(dir.path(), files);
    let result = Command::new(rock_bin())
        .arg("lint")
        .arg(dir.path())
        .arg(format!("--lints={lint}"))
        .output()
        .expect("run rock lint");
    let mut combined = String::from_utf8_lossy(&result.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&result.stderr));
    combined
}

#[test]
fn an_undeclared_package_still_defaults_every_global_to_game_memory() {
    let lua = compile_ok(&[
        ("stick.toml", &stick("memclass-default", None)),
        (
            "main.rock",
            "global COUNTER: int = 0\nfn main() { COUNTER = COUNTER + 1 }\n",
        ),
    ]);
    assert_eq!(
        plan_entry(&lua, "COUNTER"),
        game(),
        "absent [language] memory must leave today's Session default in place"
    );
}

#[test]
fn a_platform_package_owns_its_unannotated_globals_as_host_projections() {
    let lua = compile_ok(&[
        ("stick.toml", &stick("memclass-platform", Some("platform"))),
        (
            "main.rock",
            "global HANDLE: int = 0\nfn main() { HANDLE = HANDLE + 1 }\n",
        ),
    ]);
    assert_eq!(
        plan_entry(&lua, "HANDLE"),
        platform(),
        "a `platform` package's unannotated global must be out of the world plan \
         and present as a host projection"
    );
}

#[test]
fn a_debug_package_owns_its_unannotated_globals_as_runner_diagnostics() {
    let lua = compile_ok(&[
        ("stick.toml", &stick("memclass-debug", Some("debug"))),
        (
            "main.rock",
            "global PROBE: int = 0\nfn main() { PROBE = PROBE + 1 }\n",
        ),
    ]);
    assert_eq!(plan_entry(&lua, "PROBE"), debug());
}

#[test]
fn a_module_header_overrides_the_package_memory_class() {
    let lua = compile_ok(&[
        ("stick.toml", &stick("memclass-override", Some("platform"))),
        (
            "main.rock",
            "global HANDLE: int = 0\nfn main() { HANDLE = HANDLE + 1; bump() }\n",
        ),
        (
            "sim.rock",
            "@memory(game)\n\nglobal TICKS: int = 0\nfn bump() { TICKS = TICKS + 1 }\n",
        ),
    ]);
    assert_eq!(
        plan_entry(&lua, "HANDLE"),
        platform(),
        "the package class still governs the file with no header"
    );
    assert_eq!(
        plan_entry(&lua, "TICKS"),
        game(),
        "`@memory(game)` must override the package class for its own module"
    );
}

#[test]
fn an_explicit_state_annotation_wins_over_the_unit_class() {
    let lua = compile_ok(&[
        ("stick.toml", &stick("memclass-explicit", Some("platform"))),
        (
            "main.rock",
            "@state(ownership=session)\nglobal TICKS: int = 0\n\
             global HANDLE: int = 0\n\
             fn main() { TICKS = TICKS + 1; HANDLE = HANDLE + 1 }\n",
        ),
    ]);
    assert_eq!(
        plan_entry(&lua, "TICKS"),
        game(),
        "a per-declaration `@state` is the exception and must beat the package class"
    );
    assert_eq!(plan_entry(&lua, "HANDLE"), platform());
}

#[test]
fn an_explicit_state_annotation_wins_over_the_module_header() {
    let lua = compile_ok(&[
        ("stick.toml", &stick("memclass-explicit-mod", None)),
        (
            "main.rock",
            "@memory(platform)\n\n\
             @state(ownership=session)\nglobal TICKS: int = 0\n\
             global HANDLE: int = 0\n\
             fn main() { TICKS = TICKS + 1; HANDLE = HANDLE + 1 }\n",
        ),
    ]);
    assert_eq!(plan_entry(&lua, "TICKS"), game());
    assert_eq!(plan_entry(&lua, "HANDLE"), platform());
}

#[test]
fn a_const_is_outside_the_state_system_whatever_the_class_says() {
    let lua = compile_ok(&[
        ("stick.toml", &stick("memclass-const", Some("platform"))),
        (
            "main.rock",
            "const LIMIT: int = 7\nglobal SEEN: int = 0\nfn main() { SEEN = LIMIT }\n",
        ),
    ]);
    assert_eq!(plan_entry(&lua, "SEEN"), platform());
    assert!(
        !lua.lines()
            .find(|line| line.starts_with("_G.__ROCK_STATE_IMAGE_PLAN = {"))
            .expect("plan")
            .contains("::LIMIT\""),
        "a const never enters the StateImagePlan"
    );
}

#[test]
fn an_unknown_package_memory_class_is_rejected() {
    let err = compile(&[
        ("stick.toml", &stick("memclass-bad", Some("wat"))),
        ("main.rock", "fn main() {}\n"),
    ])
    .expect_err("an unknown class must not fall back to `game` silently");
    assert!(
        err.contains("memory") && err.contains("wat"),
        "the rejection must name the key and the value, got:\n{err}"
    );
}

#[test]
fn an_unknown_module_memory_class_is_rejected() {
    let err = compile(&[
        ("stick.toml", &stick("memclass-bad-mod", None)),
        ("main.rock", "@memory(wat)\n\nfn main() {}\n"),
    ])
    .expect_err("an unknown class must not fall back to `game` silently");
    assert!(
        err.contains("unknown memory class `wat`"),
        "the rejection must name the offending class, got:\n{err}"
    );
}

#[test]
fn a_module_memory_header_must_be_the_files_first_item() {
    let err = compile(&[
        ("stick.toml", &stick("memclass-late", None)),
        ("main.rock", "fn main() {}\n\n@memory(platform)\n"),
    ])
    .expect_err("a trailing header would silently govern the items above it");
    assert!(
        err.contains("@memory") && err.contains("first item"),
        "the rejection must say the header comes first, got:\n{err}"
    );
}

// -- R3: an `@state` that only repeats its unit's class ----------------------
//
// The class model only stays clean if a redundant line is NAMED. Stone's 616
// deletions were a one-off sweep; every other branch in flight still writes the
// 87-character line by habit and merges it back in. `redundant_state_annotation`
// is a default-on Warning for exactly that: visible in any gate that compiles,
// never an error, and quiet on a declaration that says something the class does
// not (2 findings tree-wide when it landed).

#[test]
fn an_at_state_that_only_repeats_the_package_class_is_diagnosed() {
    let diags = compile_diagnostics(&[
        ("stick.toml", &stick("memclass-redundant-pkg", Some("platform"))),
        (
            "main.rock",
            "@state(ownership=host_projection, world=exclude, realization=rebuild_host_projection)\n\
             global HANDLE: int = 0\n\
             fn main() { HANDLE = HANDLE + 1 }\n",
        ),
    ]);
    assert!(
        diags.contains("redundant `@state`"),
        "an ordinary compile must SHOW the finding -- that is the gate workers run:\n{diags}"
    );
    assert!(
        diags.contains("HANDLE"),
        "the finding must name the declaration:\n{diags}"
    );
    assert!(
        diags.contains("platform"),
        "the finding must name the class that already said it:\n{diags}"
    );
    assert!(
        // `self_name`, with `-` normalized to `_` (spec 4.1) -- the same
        // spelling every other package-naming diagnostic uses.
        diags.contains("memclass_redundant_pkg") && diags.contains("stick.toml"),
        "the finding must say WHERE the class is declared:\n{diags}"
    );

    let named = lint_output(
        &[
            ("stick.toml", &stick("memclass-redundant-pkg", Some("platform"))),
            (
                "main.rock",
                "@state(ownership=host_projection, world=exclude, realization=rebuild_host_projection)\n\
                 global HANDLE: int = 0\n\
                 fn main() { HANDLE = HANDLE + 1 }\n",
            ),
        ],
        "redundant_state_annotation",
    );
    assert!(
        named.contains("1 redundant_state_annotation"),
        "the lint must be selectable by name, so it can be allowed and searched:\n{named}"
    );
}

#[test]
fn an_at_state_equal_to_the_game_default_is_diagnosed_too() {
    let diags = compile_diagnostics(&[
        ("stick.toml", &stick("memclass-redundant-game", None)),
        (
            "main.rock",
            "@state(ownership=session, world=include, realization=fresh_session)\n\
             global TICKS: int = 0\n\
             fn main() { TICKS = TICKS + 1 }\n",
        ),
    ]);
    assert!(
        diags.contains("redundant `@state`") && diags.contains("TICKS"),
        "an undeclared package is `game`, so a session/include/fresh_session line \
         is just as redundant as a platform one:\n{diags}"
    );
    assert!(
        diags.contains("game"),
        "the finding must name the `game` class:\n{diags}"
    );
}

#[test]
fn an_at_state_that_differs_from_its_unit_class_is_silent() {
    let diags = compile_diagnostics(&[
        ("stick.toml", &stick("memclass-differs", Some("platform"))),
        (
            "main.rock",
            "@state(ownership=session)\nglobal TICKS: int = 0\n\
             @state(ownership=host_projection, world=exclude, realization=font)\n\
             global ATLAS: int = 0\n\
             global HANDLE: int = 0\n\
             fn main() { TICKS = TICKS + 1; ATLAS = ATLAS + 1; HANDLE = HANDLE + 1 }\n",
        ),
    ]);
    assert!(
        !diags.contains("redundant `@state`"),
        "`session` in a platform package and a `font` realization both say \
         something the class cannot; neither is redundant:\n{diags}"
    );
}

#[test]
fn a_module_header_is_what_a_redundant_line_is_measured_against() {
    let diags = compile_diagnostics(&[
        ("stick.toml", &stick("memclass-redundant-hdr", Some("platform"))),
        (
            "main.rock",
            "global HANDLE: int = 0\nfn main() { HANDLE = HANDLE + 1; bump() }\n",
        ),
        (
            "sim.rock",
            "@memory(game)\n\n\
             @state(ownership=session, world=include, realization=fresh_session)\n\
             global TICKS: int = 0\n\
             fn bump() { TICKS = TICKS + 1 }\n",
        ),
    ]);
    assert!(
        diags.contains("redundant `@state`") && diags.contains("TICKS"),
        "the file header, not the package key, is this module's class:\n{diags}"
    );
    assert!(
        diags.contains("@memory"),
        "the finding must point at the header that already said it:\n{diags}"
    );
}
