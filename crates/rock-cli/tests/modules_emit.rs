//! modules-v1 emission (plan M4). A v1 definition's identity is its DefKey
//! (`<package>::<module path>::<name>`), which is not a Lua identifier: it must
//! never reach an identifier position in the emitted artifact. Before M4 the
//! mangler passed the key through, so `rock compile` on a v1 package exited 0
//! and wrote a file LuaJIT could not parse (`function v1emit::bump_...`).
//!
//! These run the program rather than grepping the artifact: a name that is
//! wrong in either the definition or the call site fails to execute.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

const STICK: &str = r#"
[package]
name = "v1emit"
type = "bin"

[language]
modules = "v1"

[build]
channels = ["dev"]
default_channel = "dev"
"#;

const MAIN: &str = r#"
pub struct Counter {
    pub n: int,
}

pub fn bump(c: Counter) -> Counter {
    Counter { n: c.n + 1 }
}

fn main() {
    let c = Counter { n: 1 }
    let d = bump(c)
    print("n={d.n}")
}
"#;

/// Two logical modules each own a `value` of the same signature. Their DefKeys
/// differ only in the module path, so the emitted names must differ too -- this
/// is what the mangling digest is for (plan 2.1).
const SAME_NAME_MAIN: &str = r#"
fn main() {
    print("a={a::value()} b={b::value()}")
}
"#;

const SAME_NAME_A: &str = r#"
mod a;

pub fn value() -> int {
    1
}
"#;

const SAME_NAME_B: &str = r#"
mod b;

pub fn value() -> int {
    2
}
"#;

/// `pkg::m7906` and `pkg::m56555` are a real collision of the 32-bit module
/// path digest (found by search over `DefaultHasher`, which is stable across
/// processes and builds). Two definitions would then share an emitted
/// identifier, so the compile must be refused rather than renamed (plan 2.1).
const COLLIDE_STICK: &str = r#"
[package]
name = "pkg"
type = "bin"

[language]
modules = "v1"

[build]
channels = ["dev"]
default_channel = "dev"
"#;

const COLLIDE_MAIN: &str = r#"
fn main() {
    print("hi")
}
"#;

const COLLIDE_A: &str = r#"
mod m7906;

pub fn value() -> int {
    1
}
"#;

const COLLIDE_B: &str = r#"
mod m56555;

pub fn other() -> int {
    2
}
"#;

fn project(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_modules_emit_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stick.toml"), STICK).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
    }
    dir
}

fn legacy_projection_project() -> PathBuf {
    let root = std::env::temp_dir().join("rock_modules_emit_legacy_projection");
    let _ = std::fs::remove_dir_all(&root);
    let provider = root.join("provider");
    let app = root.join("app");
    std::fs::create_dir_all(&provider).unwrap();
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        provider.join("stick.toml"),
        r#"
[package]
name = "provider"
type = "lib"

[language]
modules = "v1"
"#,
    )
    .unwrap();
    std::fs::write(
        provider.join("lib.rock"),
        "pub fn answer() -> int { 7 }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("stick.toml"),
        r#"
[package]
name = "legacyapp"
type = "bin"

[dependencies]
provider = { path = "../provider" }
"#,
    )
    .unwrap();
    std::fs::write(
        app.join("main.rock"),
        "fn main() { print(\"answer={answer()}\") }\n",
    )
    .unwrap();
    app
}

fn run(dir: &Path) -> (String, String, bool) {
    let out = Command::new(rock_bin())
        .arg("run")
        .arg(dir)
        .output()
        .expect("spawn rock");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn v1_package_emits_runnable_lua() {
    let dir = project("basic", &[("main.rock", MAIN)]);
    let (stdout, stderr, ok) = run(&dir);
    assert!(ok, "rock run failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("n=2"),
        "expected the v1 program to run and print `n=2`\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn colliding_module_digests_are_rejected() {
    let dir = std::env::temp_dir().join("rock_modules_emit_collide");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stick.toml"), COLLIDE_STICK).unwrap();
    std::fs::write(dir.join("main.rock"), COLLIDE_MAIN).unwrap();
    std::fs::write(dir.join("a.rock"), COLLIDE_A).unwrap();
    std::fs::write(dir.join("b.rock"), COLLIDE_B).unwrap();
    let out = Command::new(rock_bin())
        .arg("compile")
        .arg(&dir)
        .arg("--output")
        .arg(dir.join("out.lua"))
        .output()
        .expect("spawn rock");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "colliding module digests must refuse the compile
stderr:
{stderr}"
    );
    assert!(
        stderr.contains("module path digest") && stderr.contains("pkg::m7906"),
        "the rejection must name the colliding prefixes
stderr:
{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v1_same_name_in_two_modules_emits_distinct_symbols() {
    let dir = project(
        "same_name",
        &[
            ("main.rock", SAME_NAME_MAIN),
            ("a.rock", SAME_NAME_A),
            ("b.rock", SAME_NAME_B),
        ],
    );
    let (stdout, stderr, ok) = run(&dir);
    assert!(ok, "rock run failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("a=1 b=2"),
        "same-named fns in two modules must emit distinct symbols\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn legacy_root_can_call_a_v1_dependency_export() {
    let dir = legacy_projection_project();
    let (stdout, stderr, ok) = run(&dir);
    assert!(ok, "legacy/v1 project failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("answer=7"),
        "legacy root should call the projected v1 export\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A legacy root using operator syntax on a v1 dependency's type (spec 3.2,
/// 8.5): `+`, `+=` and a safe-nav `?.p +=` select the projected v1
/// `operator +` and run its body (the `+ 100` marks it), while the legacy
/// root's own `operator +` for its own type and built-in int/float addition
/// keep working in the same file.
#[test]
fn legacy_operator_syntax_runs_a_projected_v1_operator_body() {
    let root = std::env::temp_dir().join("rock_modules_emit_legacy_operator");
    let _ = std::fs::remove_dir_all(&root);
    let provider = root.join("provider");
    let app = root.join("app");
    std::fs::create_dir_all(&provider).unwrap();
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        provider.join("stick.toml"),
        "[package]\nname = \"provider\"\ntype = \"lib\"\n\n[language]\nmodules = \"v1\"\n",
    )
    .unwrap();
    std::fs::write(
        provider.join("lib.rock"),
        "pub struct Pt {\n    pub x: int,\n    pub y: int,\n}\n\n\
         pub operator +(a: Pt, b: Pt) -> Pt {\n    \
         Pt { x: math::plus(math::plus(a.x, b.x), 100), y: math::plus(a.y, b.y) }\n}\n",
    )
    .unwrap();
    std::fs::write(
        provider.join("math.rock"),
        "pub mod math;\n\npub fn plus(a: int, b: int) -> int {\n    a + b\n}\n",
    )
    .unwrap();
    std::fs::write(
        app.join("stick.toml"),
        "[package]\nname = \"legacyapp\"\ntype = \"bin\"\n\n[dependencies]\nprovider = { path = \"../provider\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.rock"),
        r#"struct Money {
    cents: int,
}

operator +(a: Money, b: Money) -> Money {
    Money { cents: a.cents + b.cents + 1000 }
}

struct Holder {
    p: Pt,
}

fn bump_held(h: Holder?) {
    h?.p += Pt { x: 1, y: 1 }
}

fn main() {
    let p = Pt { x: 1, y: 2 } + Pt { x: 3, y: 4 }
    p += Pt { x: 10, y: 20 }
    let h = Holder { p: Pt { x: 0, y: 0 } }
    bump_held(h)
    let m = Money { cents: 1 } + Money { cents: 2 }
    let n = 1 + 2
    n += 4
    let f = 1.5 + 2.0
    print("p={p.x},{p.y} h={h.p.x},{h.p.y} m={m.cents} n={n} f={f}")
}
"#,
    )
    .unwrap();
    let (stdout, stderr, ok) = run(&app);
    assert!(ok, "legacy/v1 operator project failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("p=214,26 h=101,1 m=1003 n=7 f=3.5"),
        "legacy operator syntax should run the v1 operator body\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A legacy root applies a v1 dependency's `pub` decorator through its root
/// projection (spec 3.2), as it calls a projected fn: the application
/// registers a lifecycle hook. `@update` also names a legacy stdlib fn, which
/// does not compete for an annotation. A private v1 decorator is not
/// projected, so applying it stays an unknown annotation.
#[test]
fn legacy_root_applies_a_projected_v1_decorator() {
    let root = std::env::temp_dir().join("rock_modules_emit_legacy_decorator");
    let _ = std::fs::remove_dir_all(&root);
    let provider = root.join("provider");
    let app = root.join("app");
    std::fs::create_dir_all(&provider).unwrap();
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        provider.join("stick.toml"),
        "[package]\nname = \"provider\"\ntype = \"lib\"\n\n[language]\nmodules = \"v1\"\n",
    )
    .unwrap();
    std::fs::write(
        provider.join("lib.rock"),
        "pub @lifecycle(phase=\"update\", owner=\"session\")\ndeco init tick {}\n\n\
         pub @lifecycle(phase=\"update\", owner=\"session\")\ndeco init update {}\n\n\
         @lifecycle(phase=\"draw\", owner=\"session\")\ndeco init hidden {}\n",
    )
    .unwrap();
    std::fs::write(
        app.join("stick.toml"),
        "[package]\nname = \"legacyapp\"\ntype = \"bin\"\n\n[dependencies]\nprovider = { path = \"../provider\" }\n",
    )
    .unwrap();
    let main = |hook: &str| {
        format!(
            "inline(lua) fn update_hooks() -> int = \"return _G.__ROCK_LIFECYCLE_PLAN.update.len\"\n\n\
             @{hook}\nfn app_tick() {{}}\n\n@update\nfn app_update() {{}}\n\n\
             fn main() {{\n    print(\"hooks={{update_hooks()}}\")\n}}\n"
        )
    };
    std::fs::write(app.join("main.rock"), main("tick")).unwrap();
    let (stdout, stderr, ok) = run(&app);
    assert!(ok, "legacy root applying a v1 decorator failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("hooks=2"), "both applications should register a hook\nstdout:\n{stdout}\nstderr:\n{stderr}");

    std::fs::write(app.join("main.rock"), main("hidden")).unwrap();
    let (stdout, stderr, ok) = run(&app);
    assert!(!ok, "a private v1 decorator must not be applicable\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("unknown annotation `@hidden`"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A v1 manifest for a library package named `name`.
fn v1_lib(dir: &Path, name: &str, lib: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("stick.toml"),
        format!("[package]\nname = \"{name}\"\ntype = \"lib\"\n\n[language]\nmodules = \"v1\"\n"),
    )
    .unwrap();
    std::fs::write(dir.join("lib.rock"), lib).unwrap();
}

/// A v1 binary package depending on the sibling packages `deps`.
fn v1_app(dir: &Path, deps: &[&str], main: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let deps: String = deps.iter().map(|d| format!("{d} = {{ path = \"../{d}\" }}\n")).collect();
    std::fs::write(
        dir.join("stick.toml"),
        format!(
            "[package]\nname = \"v1app\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n\n[dependencies]\n{deps}"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("main.rock"), main).unwrap();
}

/// A v1 consumer of a v1 operator (spec 8.5): the provider's own
/// `operator +` body adds ints with `+` (a module's operator family does not
/// hide built-in arithmetic), and the consumer's `+`, `+=` and safe-nav
/// `?.p +=` run that body (the `+ 100` marks it) while int and float `+`
/// stay built-in.
#[test]
fn v1_operator_families_keep_builtins_and_run_the_declared_body() {
    let root = std::env::temp_dir().join("rock_modules_emit_v1_operator");
    let _ = std::fs::remove_dir_all(&root);
    v1_lib(
        &root.join("provider"),
        "provider",
        r#"pub struct Pt {
    pub x: int,
    pub y: int,
}

pub operator +(a: Pt, b: Pt) -> Pt {
    Pt { x: a.x + b.x + 100, y: a.y + b.y }
}

pub struct Holder {
    pub p: Pt,
}
"#,
    );
    let app = root.join("app");
    v1_app(
        &app,
        &["provider"],
        r#"use provider::{Pt, Holder, operator(+)};

fn bump_held(h: Holder?) {
    h?.p += Pt { x: 1, y: 1 }
}

fn main() {
    let p = Pt { x: 1, y: 2 } + Pt { x: 3, y: 4 }
    p += Pt { x: 10, y: 20 }
    let h = Holder { p: Pt { x: 0, y: 0 } }
    bump_held(h)
    let n = 1 + 2
    n += 4
    let f = 1.5 + 2.0
    print("p={p.x},{p.y} h={h.p.x},{h.p.y} n={n} f={f}")
}
"#,
    );
    let (stdout, stderr, ok) = run(&app);
    assert!(ok, "v1 operator project failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("p=214,26 h=101,1 n=7 f=3.5"),
        "v1 operator syntax should run the declared body and keep built-in arithmetic\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `use provider::pick; use other::pick;` in a v1 file compose into one
/// overload family (spec 8.3); each call runs the member its argument type
/// selects (the `+ 1000` and the `!` mark them), through the bare call and
/// UFCS alike.
#[test]
fn v1_explicit_imports_compose_and_run_the_selected_member() {
    let root = std::env::temp_dir().join("rock_modules_emit_v1_composed_imports");
    let _ = std::fs::remove_dir_all(&root);
    v1_lib(&root.join("provider"), "provider", "pub fn pick(n: int) -> int {\n    n + 1000\n}\n");
    v1_lib(&root.join("other"), "other", "pub fn pick(s: string) -> string {\n    s + \"!\"\n}\n");
    let app = root.join("app");
    v1_app(
        &app,
        &["provider", "other"],
        r#"use provider::pick;
use other::pick;

fn main() {
    let u = "u".pick()
    print("i={pick(1)} s={pick("s")} u={u}")
}
"#,
    );
    let (stdout, stderr, ok) = run(&app);
    assert!(ok, "v1 composed import project failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("i=1001 s=s! u=u!"),
        "composed imports should run the member each argument selects\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `use provider::*; use other::*;` compose `pick` and `operator +` across
/// the two modules (spec 8.3, 8.5; author decision: merge): each call and
/// each `+` runs the member its argument types select (`+ 1000`, `!`,
/// `+ 100` and `+ 7` mark them), while int addition stays built-in.
#[test]
fn v1_glob_imports_compose_and_run_the_selected_member() {
    let root = std::env::temp_dir().join("rock_modules_emit_v1_composed_globs");
    let _ = std::fs::remove_dir_all(&root);
    v1_lib(
        &root.join("provider"),
        "provider",
        r#"pub fn pick(n: int) -> int {
    n + 1000
}

pub struct Pt {
    pub x: int,
}

pub operator +(a: Pt, b: Pt) -> Pt {
    Pt { x: a.x + b.x + 100 }
}
"#,
    );
    v1_lib(
        &root.join("other"),
        "other",
        r#"pub fn pick(s: string) -> string {
    s + "!"
}

pub struct Rgb {
    pub r: int,
}

pub operator +(a: Rgb, b: Rgb) -> Rgb {
    Rgb { r: a.r + b.r + 7 }
}
"#,
    );
    let app = root.join("app");
    v1_app(
        &app,
        &["provider", "other"],
        r#"use provider::*;
use other::*;

fn main() {
    let u = "u".pick()
    let p = Pt { x: 1 } + Pt { x: 2 }
    let c = Rgb { r: 1 } + Rgb { r: 2 }
    let n = 1 + 2
    print("i={pick(1)} s={pick("s")} u={u} p={p.x} c={c.r} n={n}")
}
"#,
    );
    let (stdout, stderr, ok) = run(&app);
    assert!(ok, "v1 composed glob project failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("i=1001 s=s! u=u! p=103 c=10 n=3"),
        "composed globs should run the member each argument selects\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// v1 unary operators: `-p` and `!p` run the globbed `__unm` / `__not`
/// bodies (the `- 100` and the `x == 7` mark them) while `-n` and `!b` on
/// scalars stay native.
#[test]
fn v1_unary_operators_run_the_resolved_body() {
    let root = std::env::temp_dir().join("rock_modules_emit_v1_unary");
    let _ = std::fs::remove_dir_all(&root);
    v1_lib(
        &root.join("provider"),
        "provider",
        r#"pub struct Pt {
    pub x: int,
}

pub fn __unm(a: Pt) -> Pt {
    Pt { x: 0 - a.x - 100 }
}

pub fn __not(a: Pt) -> bool {
    a.x == 7
}
"#,
    );
    let app = root.join("app");
    v1_app(
        &app,
        &["provider"],
        r#"use provider::*;

fn main() {
    let p = -Pt { x: 1 }
    let z = !Pt { x: 7 }
    let n = 5
    let b = false
    print("p={p.x} z={z} n={-n} b={!b}")
}
"#,
    );
    let (stdout, stderr, ok) = run(&app);
    assert!(ok, "v1 unary operator project failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("p=-101 z=true n=-5 b=true"),
        "v1 unary operators should run the resolved body and keep scalars native\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// An operator declaration uses the same `__unm` / `__not` families as an
/// ordinary function declaration. An unaliased `operator(-)` import exposes
/// both the prefix and binary arities, while a glob exposes all three
/// operator declarations. Scalar prefix forms remain native.
#[test]
fn v1_operator_spelling_unary_and_binary_run_the_resolved_body() {
    let root = std::env::temp_dir().join("rock_modules_emit_v1_operator_unary");
    let _ = std::fs::remove_dir_all(&root);
    v1_lib(
        &root.join("provider"),
        "provider",
        r#"pub struct Pt {
    pub x: int,
}

pub operator -(a: Pt) -> Pt {
    Pt { x: 0 - a.x - 100 }
}

pub operator -(a: Pt, b: Pt) -> Pt {
    Pt { x: a.x - b.x + 200 }
}

pub operator !(a: Pt) -> bool {
    a.x == 7
}
"#,
    );
    let explicit = root.join("explicit");
    v1_app(
        &explicit,
        &["provider"],
        r#"use provider::{Pt, operator(-), operator(!)};

fn main() {
    let u = -Pt { x: 1 }
    let d = Pt { x: 5 } - Pt { x: 2 }
    let f = !Pt { x: 7 }
    let i = -5
    let r = -1.5
    let n = !true
    let b = ~5
    print("u={u.x} d={d.x} f={f} i={i} r={r} n={n} b={b}")
}
"#,
    );
    let (stdout, stderr, ok) = run(&explicit);
    assert!(ok, "explicit operator import project failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("u=-101 d=203 f=true i=-5 r=-1.5 n=false b=-6"),
        "operator spelling should run both arities and keep scalar prefixes native\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let glob = root.join("glob");
    v1_app(
        &glob,
        &["provider"],
        r#"use provider::*;

fn main() {
    let u = -Pt { x: 1 }
    let d = Pt { x: 5 } - Pt { x: 2 }
    let f = !Pt { x: 7 }
    print("u={u.x} d={d.x} f={f}")
}
"#,
    );
    let (stdout, stderr, ok) = run(&glob);
    assert!(ok, "glob operator import project failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("u=-101 d=203 f=true"),
        "globbed operator declarations should run both arities\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `use provider::operator(+) as add_pt;` (spec 8.5) binds `add_pt` to the
/// declared `operator +` body (the `+ 100` marks it), by call and UFCS.
#[test]
fn v1_aliased_operator_import_runs_the_operator_body() {
    let root = std::env::temp_dir().join("rock_modules_emit_v1_operator_alias");
    let _ = std::fs::remove_dir_all(&root);
    v1_lib(
        &root.join("provider"),
        "provider",
        r#"pub struct Pt {
    pub x: int,
}

pub operator +(a: Pt, b: Pt) -> Pt {
    Pt { x: a.x + b.x + 100 }
}
"#,
    );
    let app = root.join("app");
    v1_app(
        &app,
        &["provider"],
        r#"use provider::{Pt, operator(+) as add_pt};

fn main() {
    let p = add_pt(Pt { x: 1 }, Pt { x: 2 })
    let u = Pt { x: 3 }.add_pt(Pt { x: 4 })
    print("p={p.x} u={u.x} n={1 + 2}")
}
"#,
    );
    let (stdout, stderr, ok) = run(&app);
    assert!(ok, "v1 aliased operator project failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("p=103 u=107 n=3"),
        "an aliased operator import should run the declared body\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Spec 8.4 keeps the field-call precedence: `a.stop()` calls a fn-typed
/// field `stop` even when a same-named function is in scope, and with no such
/// function at all. v1 used to reject the second (`stop` did not resolve to
/// any definition) and silently call the free function in the first, because
/// both inference and MIR looked the field up by the callee's DefKey. Found by
/// migrating `libs/fm-core`.
#[test]
fn v1_method_syntax_calls_a_fn_typed_field_first() {
    let root = std::env::temp_dir().join("rock_modules_emit_v1_field_call");
    let _ = std::fs::remove_dir_all(&root);
    let app = root.join("app");
    v1_app(
        &app,
        &[],
        r#"struct Api {
    stop: fn() -> int,
    go: fn(int) -> int,
}

fn stop(a: Api) -> int {
    100
}

fn call_both(a: Api) -> int {
    a.stop() + a.go(1)
}

fn maybe(a: Api?) -> int {
    a?.stop() ?? -1
}

fn main() {
    let a = Api { stop: () => 7, go: (n) => n + 20 }
    print("both={call_both(a)} free={stop(a)} nav={maybe(a)} none={maybe(null)}")
}
"#,
    );
    let (stdout, stderr, ok) = run(&app);
    assert!(ok, "v1 field-call project failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("both=28 free=100 nav=7 none=-1"),
        "a method call must reach the fn-typed field, the free fn only by name\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A legacy library package named `name` with one `lib.rock`.
fn legacy_lib(dir: &Path, name: &str, lib: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("stick.toml"), format!("[package]\nname = \"{name}\"\ntype = \"lib\"\n")).unwrap();
    std::fs::write(dir.join("lib.rock"), lib).unwrap();
}

const LEGACY_XLIB: &str = r#"struct P {
    x: int,
}

enum Dir {
    Up,
    Down(int),
}

global LIMIT: int = 3

operator +(a: P, b: P) -> P {
    P { x: a.x + b.x + 100 }
}

fn mix(a: int) -> int {
    a + 1000
}

fn mix(a: string) -> string {
    a + "!"
}

fn walk(d: Dir) -> int {
    match d {
        Dir::Up => LIMIT,
        Dir::Down(n) => n,
    }
}
"#;

/// A v1 package over a legacy dependency (spec 3.2, V1 -> Legacy) with an
/// `allow_legacy` edge: paths, a named import, a glob, an overloaded family
/// (`+ 1000` and `!` mark the member each call selects), the provider's
/// operator (`+ 100` marks it) beside built-in int addition, an enum
/// variant, a pattern and a global all run the legacy definitions.
#[test]
fn v1_code_runs_a_legacy_dependency_through_its_view() {
    let root = std::env::temp_dir().join("rock_modules_emit_v1_over_legacy");
    let _ = std::fs::remove_dir_all(&root);
    legacy_lib(&root.join("x_lib"), "x_lib", LEGACY_XLIB);
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("stick.toml"),
        "[package]\nname = \"v1app\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n\n[dependencies]\nx_lib = { path = \"../x_lib\", allow_legacy = true }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.rock"),
        r#"use x_lib::*;
use x_lib::walk as stroll;

fn down(d: x_lib::Dir) -> int {
    match d {
        x_lib::Dir::Down(n) => n,
        x_lib::Dir::Up => x_lib::LIMIT,
    }
}

fn main() {
    let p = P { x: 1 } + x_lib::P { x: 2 }
    let d = x_lib::Dir::Down(5)
    print("p={p.x} i={mix(1)} s={x_lib::mix("s")} w={stroll(d)} u={walk(Dir::Up)} m={down(d)} n={1 + 2}")
}
"#,
    )
    .unwrap();
    let (stdout, stderr, ok) = run(&app);
    assert!(ok, "v1-over-legacy project failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("p=103 i=1001 s=s! w=5 u=3 m=5 n=3"),
        "v1 code should run the legacy definitions\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The same v1 package without `allow_legacy` on its legacy edge is refused
/// by the package graph, naming the fix (spec 3.2).
#[test]
fn a_v1_package_needs_allow_legacy_to_reach_a_legacy_dependency() {
    let root = std::env::temp_dir().join("rock_modules_emit_v1_over_legacy_unflagged");
    let _ = std::fs::remove_dir_all(&root);
    legacy_lib(&root.join("x_lib"), "x_lib", LEGACY_XLIB);
    let app = root.join("app");
    v1_app(&app, &["x_lib"], "fn main() {\n    print(\"i={x_lib::mix(1)}\")\n}\n");
    let (stdout, stderr, ok) = run(&app);
    assert!(!ok, "a v1 package with an unflagged legacy edge must not build\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("allow_legacy = true"), "the refusal should name the fix\nstdout:\n{stdout}\nstderr:\n{stderr}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A legacy game's overload of a std-named function is not in the std
/// family a v1 library reaches with no import (spec 5, 8.3): the library's
/// `smoothstep(0, 10, 5)` runs the standard library's body, not the game's
/// better-fitting int overload (`7777` marks it), and the library's
/// `smoothstep` taken as a value is the std definition; the game's own call
/// still reaches its overload.
#[test]
fn a_v1_library_does_not_reach_its_consumers_overload_of_a_std_function() {
    let root = std::env::temp_dir().join("rock_modules_emit_std_isolation");
    let _ = std::fs::remove_dir_all(&root);
    v1_lib(
        &root.join("geo"),
        "geo",
        "pub fn ease() -> float {\n    smoothstep(0, 10, 5)\n}\n\n\
         pub fn ease_value() -> float {\n    let f = smoothstep\n    f(0.0, 10.0, 5.0)\n}\n",
    );
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("stick.toml"),
        "[package]\nname = \"legacyapp\"\ntype = \"bin\"\n\n[dependencies]\ngeo = { path = \"../geo\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.rock"),
        "fn smoothstep(a: int, b: int, t: int) -> int {\n    7777\n}\n\n\
         fn main() {\n    print(\"lib={ease()} val={ease_value()} own={smoothstep(0, 10, 5)}\")\n}\n",
    )
    .unwrap();
    let (stdout, stderr, ok) = run(&app);
    assert!(ok, "the mixed build failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("lib=0.5 val=0.5 own=7777"),
        "the v1 library must run the std body and the game its own\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `x_lib::mix` names the overloads x_lib's own files define (spec 3.2), not
/// another legacy package's better-fitting `mix(int)` (`5000` marks it).
#[test]
fn a_legacy_providers_path_runs_only_the_providers_overloads() {
    let root = std::env::temp_dir().join("rock_modules_emit_provider_isolation");
    let _ = std::fs::remove_dir_all(&root);
    legacy_lib(&root.join("x_lib"), "x_lib", "fn mix(a: float) -> float {\n    a + 0.25\n}\n");
    legacy_lib(&root.join("y_lib"), "y_lib", "fn mix(a: int) -> int {\n    5000\n}\n");
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("stick.toml"),
        "[package]\nname = \"v1app\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n\n[dependencies]\n\
         x_lib = { path = \"../x_lib\", allow_legacy = true }\ny_lib = { path = \"../y_lib\", allow_legacy = true }\n",
    )
    .unwrap();
    std::fs::write(app.join("main.rock"), "fn main() {\n    print(\"x={x_lib::mix(1)} y={y_lib::mix(1)}\")\n}\n").unwrap();
    let (stdout, stderr, ok) = run(&app);
    assert!(ok, "the v1-over-legacy build failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("x=1.25 y=5000"),
        "each provider path must run that provider's own body\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
