//! modules-v1 witness (plan M6): three packages driven through the real `rock`
//! binary and real manifests, plus the two CLI defects that building it
//! exposed. `modules_emit.rs` is the M4 sibling; this file is the M6 one.

use std::path::{Path, PathBuf};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_modules_witness_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn rock(args: &[&str]) -> (String, String, bool) {
    let out = crate::command::rock_bin().args(args).output().expect("spawn rock");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

const V1_TESTED_PKG: &str = r#"
[package]
name = "v1tested"
type = "bin"

[language]
modules = "v1"

[build]
channels = ["dev"]
default_channel = "dev"
"#;

/// A v1 `@test` fn's DefKey is `v1tested::doubles` (root) or
/// `v1tested::math::scales` (a child module). `@test` is mangling-EXEMPT, so
/// the fn the artifact defines is the DECLARED segment -- the key itself is not
/// a Lua identifier. Before this was split, the harness wrote the KEY into the
/// `f = <global>` slot and luajit refused the chunk with
/// `'}' expected near '::'`, which the runner reported as "invalid lua emitted
/// (skull bug)" on every v1 package that owns a test.
#[test]
fn a_v1_packages_tests_run_under_the_cli() {
    let dir = scratch("v1_tests");
    write(&dir.join("stick.toml"), V1_TESTED_PKG);
    write(
        &dir.join("main.rock"),
        r#"
pub fn twice(n: int) -> int {
    n * 2
}

fn main() {
    print("ok")
}

@test
fn doubles() {
    assert(twice(21) == 42)
}
"#,
    );
    write(
        &dir.join("math.rock"),
        r#"
mod math;

pub fn scale(n: int) -> int {
    n * 3
}

@test
fn scales() {
    assert(scale(14) == 42)
}
"#,
    );
    let d = dir.to_string_lossy().into_owned();
    let (stdout, stderr, ok) = rock(&["test", &d]);
    assert!(ok, "rock test on a v1 package failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("2/2 @tests passed"),
        "both v1 @tests must run\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `rock test <dir>` walks the directory for `.rock` files and builds each one
/// as its own unit. A file that belongs to a NESTED package is not a standalone
/// unit: its manifest supplies its dialect and its dependencies, so compiling it
/// alone parses a v1 source in the legacy dialect (`unsupported item start:
/// Ident("pub")`) and reports an emit failure that says nothing about the
/// package. Spec 6.3 says a nested manifest starts a different package and is
/// not absorbed by its parent; discovery has to agree.
#[test]
fn a_nested_package_under_a_walked_dir_is_one_unit() {
    let dir = scratch("nested_pkg");
    write(
        &dir.join("plain.rock"),
        r#"
@test
fn a_loose_file_is_still_its_own_unit() {
    assert(1 + 1 == 2)
}
"#,
    );
    write(&dir.join("pkg").join("stick.toml"), V1_TESTED_PKG);
    write(
        &dir.join("pkg").join("main.rock"),
        r#"
pub fn twice(n: int) -> int {
    n * 2
}

fn main() {
    print("ok")
}

@test
fn the_nested_package_compiles_through_its_manifest() {
    assert(twice(21) == 42)
}
"#,
    );
    let d = dir.to_string_lossy().into_owned();
    let (stdout, stderr, ok) = rock(&["test", &d]);
    assert!(
        ok,
        "rock test over a dir holding a nested package failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("2/2 @tests passed"),
        "the loose file and the nested package are two units, both run\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --- The three-package witness (plan M6.1) ---------------------------------
//
// The packages live at `tests/modules/witness/` and are ordinary source, not
// string constants: `witness-lib` is a v1 library with a private child module,
// `witness-v1` a v1 consumer of it, `witness-legacy` a legacy consumer of the
// same library through the compatibility projection (spec 3.2). Each has its
// own `stick.toml`, so every assertion below goes through the real dependency
// graph rather than the in-process fixture harness.

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // repo root
    p
}

fn witness_root() -> PathBuf {
    repo_root().join("tests").join("modules").join("witness")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// The witness packages copied into a scratch tree for mutation tests. The
/// production filter is package-root-relative, so the unmodified witness can
/// also run where it lives under the repository's `tests/` tree.
fn staged_witness(tag: &str) -> PathBuf {
    let dir = scratch(tag);
    copy_tree(&witness_root(), &dir);
    dir
}

fn run_pkg(staged: &Path, pkg: &str) -> (String, String, bool) {
    rock(&["run", &staged.join(pkg).to_string_lossy()])
}

/// Stage the witness, replace one consumer's `main.rock` with a body that
/// reaches for something it may not have, and assert the compile is refused
/// for the named reason.
fn rejects(tag: &str, pkg: &str, main: &str, expect: &str) {
    let staged = staged_witness(tag);
    write(&staged.join(pkg).join("main.rock"), main);
    let (stdout, stderr, ok) = run_pkg(&staged, pkg);
    let all = format!("{stdout}{stderr}");
    assert!(!ok, "expected `{pkg}` to be refused\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        all.contains(expect),
        "expected the rejection to say `{expect}`\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&staged);
}

#[test]
fn the_v1_consumer_reaches_the_librarys_public_exports() {
    let staged = staged_witness("v1_consumer");
    let (stdout, stderr, ok) = run_pkg(&staged, "witness-v1");
    assert!(ok, "the v1 consumer failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    // An imported name, a qualified path, and a public wrapper over the
    // private child module -- the three shapes a v1 consumer has.
    for line in ["v1 greeting: hello, v1", "v1 answer: 42", "v1 sauce: sauce"] {
        assert!(stdout.contains(line), "missing `{line}`\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }
    let _ = std::fs::remove_dir_all(&staged);
}

#[test]
fn the_v1_consumer_runs_in_place_under_repository_tests_tree() {
    let root = witness_root();
    let (stdout, stderr, ok) = run_pkg(&root, "witness-v1");
    assert!(ok, "the in-place v1 consumer failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    for line in ["v1 greeting: hello, v1", "v1 answer: 42", "v1 sauce: sauce"] {
        assert!(stdout.contains(line), "missing `{line}`\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }
}

#[test]
fn the_legacy_consumer_sees_the_public_root_exports_by_bare_name() {
    let staged = staged_witness("legacy_consumer");
    let (stdout, stderr, ok) = run_pkg(&staged, "witness-legacy");
    assert!(ok, "the legacy consumer failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    // No imports and no paths: the compatibility projection puts the v1
    // library's public ROOT exports in the legacy unit under their own names.
    for line in ["legacy greeting: hello, legacy", "legacy answer: 42", "legacy sauce: sauce"] {
        assert!(stdout.contains(line), "missing `{line}`\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }
    let _ = std::fs::remove_dir_all(&staged);
}

#[test]
fn the_v1_consumer_cannot_name_a_private_root_item() {
    rejects(
        "v1_private_root",
        "witness-v1",
        r#"
fn main() {
    print("{witness_lib::root_secret()}")
}
"#,
        "`root_secret` is not accessible from this module",
    );
}

/// The point of the private child module: `recipe` is `pub` INSIDE `secret`,
/// but `secret` itself is private to its declaring module, so the consumer is
/// stopped at the module segment and never reaches the `pub`.
#[test]
fn the_private_child_module_is_unreachable_from_the_v1_consumer() {
    rejects(
        "v1_private_module",
        "witness-v1",
        r#"
fn main() {
    print("{witness_lib::secret::recipe()}")
}
"#,
        "module `secret` is not accessible from this module",
    );
}

/// The projection carries the v1 library's PUBLIC ROOT exports and nothing
/// else, so a private root item is simply not a name in the legacy unit.
#[test]
fn the_legacy_consumer_is_not_projected_a_private_root_item() {
    rejects(
        "legacy_private_root",
        "witness-legacy",
        r#"
fn main() {
    print("{root_secret()}")
}
"#,
        "Function `root_secret` doesn't exist",
    );
}

/// Nor anything owned by a private child module, however public it is there.
#[test]
fn the_private_child_modules_export_is_not_projected_to_the_legacy_consumer() {
    rejects(
        "legacy_private_module",
        "witness-legacy",
        r#"
fn main() {
    print("{recipe()}")
}
"#,
        "Function `recipe` doesn't exist",
    );
}

/// The witness is a real part of the repository, not only a staging source:
/// `rock test` reads it where it lives, builds each package through its own
/// manifest, and executes the library's `@test` -- which calls the private
/// child module from inside the package, the other half of the rule the two
/// rejection tests above prove from outside.
#[test]
fn the_witness_library_builds_and_tests_in_place() {
    let (stdout, stderr, ok) = rock(&["test", &witness_root().to_string_lossy()]);
    assert!(ok, "rock test on the witness tree failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("1/1 @tests passed") && stdout.contains("across 3 files"),
        "the three witness packages are three units and the library's @test runs\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// The type closure (spec 11) is what decides how far a conversion's `pub`
/// has to reach: exporting `greet` is not a choice about `greet` alone. Take
/// `pub` off the returned record and the library stops building, with the
/// reason named.
#[test]
fn the_api_closure_demands_the_return_type_of_an_exported_fn() {
    let staged = staged_witness("api_closure");
    let lib = staged.join("witness-lib").join("lib.rock");
    let src = std::fs::read_to_string(&lib).unwrap();
    write(&lib, &src.replace("pub struct Greeting {", "struct Greeting {"));
    let (stdout, stderr, ok) = run_pkg(&staged, "witness-v1");
    let all = format!("{stdout}{stderr}");
    assert!(!ok, "a closed-over type left private must refuse\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        all.contains("type `Greeting` in public API is not accessible")
            && all.contains("reason: public function"),
        "the closure failure must name the type and the reason\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&staged);
}

// --- The scoped consumer of the converted chess-core (plan M6.2) -----------

/// `tests/modules/chess-scoped` is the V1 -> V1 row on real code: a v1 package
/// consuming `libs/chess-core`, which M6.2 converted, through an import and
/// through qualified paths. It builds where it lives (its dependency does not
/// live under `tests/`, so only `rock test` reads it) and its `@test`s run the
/// real movegen.
#[test]
fn the_scoped_chess_consumer_builds_and_tests_in_place() {
    let dir = repo_root().join("tests").join("modules").join("chess-scoped");
    let (stdout, stderr, ok) = rock(&["test", &dir.to_string_lossy()]);
    assert!(ok, "rock test on the scoped consumer failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("2/2 @tests passed"),
        "the scoped consumer's @tests must run against the real chess-core\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// The other half of the conversion: the 19 functions M6.2 left private are
/// private to consumers too, including a v1 one. `ch_at` is a real chess-core
/// helper, and `ch_king_square` in the same file is a real export -- so this
/// rejects for its visibility, not because the name is missing.
#[test]
fn a_v1_consumer_of_chess_core_cannot_reach_a_private_helper() {
    let dir = scratch("chess_private");
    let core = repo_root().join("libs").join("chess-core");
    write(
        &dir.join("stick.toml"),
        &format!(
            r#"
[package]
name = "chess-private-probe"
type = "lib"

[language]
modules = "v1"

[dependencies]
chess-core = {{ path = "{}" }}

[build]
channels = ["dev"]
default_channel = "dev"
"#,
            core.display()
        ),
    );
    write(
        &dir.join("lib.rock"),
        r#"
use chess_core::Position;

pub fn reach(pos: Position) -> int {
    return chess_core::ch_at(pos, 0)
}
"#,
    );
    let (stdout, stderr, ok) = rock(&["test", &dir.to_string_lossy()]);
    let all = format!("{stdout}{stderr}");
    assert!(!ok, "reaching a private chess-core helper must be refused\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        all.contains("`ch_at` is not accessible from this module"),
        "the rejection must name the helper and the reason\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
