//! End-to-end gate: `rock run <file> --backend js` emits the whole file as a
//! node program (module ordering + `__G` ginit topo + decorator APPLICATIONS at
//! module load + a co fn) and runs `main` end to end, producing byte-identical
//! output to the luajit `rock run` of the same file.
//!
//! Drives the real binary on both backends so the whole driver path (capability
//! gate -> emit_program -> node stream) is exercised, and pins the cross-backend
//! parity `rock run` promises.

// Exercises: a registry global (SYSTEMS) populated by a `deco init` at module
// load, two `@system`-applied fns (alpha with a literal `stage = 5`, beta
// defaulted), a ginit dependency chain (DERIVED reads BASE), a co fn, and a
// `main` that reads a derived global then iterates the registry calling each
// registered fn value.
const GATE: &str = r#"
struct Sys {
    func: fn() -> (),
    name: string,
    stage: int,
}

global SYSTEMS: [Sys] = []
global BASE: int = 10
global DERIVED: int = BASE * 2

deco init system(stage: int = 100) {
    SYSTEMS.push(Sys {
        func: transmute(target.func),
        name: transmute(target.name),
        stage: stage,
    })
}

@system(stage = 5)
fn alpha() {
    print("alpha")
}

@system
fn beta() {
    print("beta")
}

co fn nums() -> Coroutine<int> {
    yield 1
    yield 2
}

fn main() {
    print("derived={DERIVED}")
    for s in SYSTEMS {
        print("{s.name} stage={s.stage}")
        s.func()
    }
    let c = nums()
    print(c.resume())
    print(c.resume())
}
"#;

const EXPECTED: &str = "derived=20\nalpha stage=5\nalpha\nbeta stage=100\nbeta\n1\n2\n";

fn run(backend: Option<&str>, path: &std::path::Path) -> (String, bool) {
    let mut cmd = crate::command::rock_bin();
    cmd.args(["run", path.to_str().unwrap()]);
    if let Some(b) = backend {
        cmd.args(["--backend", b]);
    }
    let out = cmd.output().expect("spawn rock");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

// `rock run <dir> --backend js` (project mode) -- a multi-file
// project (main + a sibling module) emits via the deterministic module walk and
// runs `main` end to end, byte-identical to the luajit `rock run` of the dir.
const PROJECT_FILES: &[(&str, &str)] = &[
    ("stick.toml", "[package]\nname = \"proj\"\n"),
    ("math.rock", "fn square(x: int) -> int { return x * x }\n"),
    (
        "main.rock",
        "fn main() {\n  let t = 0\n  for i in 0..5 { t = t + square(i) }\n  print(\"sum={t}\")\n}\n",
    ),
];

// A modules-v1 package names its functions by qualified DefKey
// (`<package>::main`), so the entry must be found by its declared name.
const MODULES_V1_FILES: &[(&str, &str)] = &[
    (
        "stick.toml",
        "[package]\nname = \"semantics_probe\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n",
    ),
    ("main.rock", "fn main() { print(17) }\n"),
];

// A file with no nullary `main` runs module-load effects only (ginit + deco
// applications), matching luajit -- and prints nothing, exiting cleanly.
const NO_MAIN: &str = "global X: int = 7\nfn helper() -> int { X + 1 }\n";

fn assert_backends_agree(path: &std::path::Path, expected: &str) {
    let (lua_out, lua_ok) = run(None, path);
    assert!(lua_ok, "luajit run failed:\n{lua_out}");
    assert_eq!(lua_out, expected, "luajit output");

    let (js_out, js_ok) = run(Some("js"), path);
    assert!(js_ok, "js run failed:\n{js_out}");
    assert_eq!(js_out, expected, "js output");
}

fn both_backends(source: &str, expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gate.rock");
    std::fs::write(&path, source).unwrap();
    assert_backends_agree(&path, expected);
}

fn both_backends_project(files: &[(&str, &str)], expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    for (name, contents) in files {
        std::fs::write(dir.path().join(name), contents).unwrap();
    }
    assert_backends_agree(dir.path(), expected);
}

macro_rules! parity_tests {
    ($($test:ident: $check:ident($input:expr, $expected:expr);)*) => {
        $(
            #[test]
            fn $test() {
                if crate::command::require_node() {
                    $check($input, $expected);
                }
            }
        )*
    };
}

parity_tests! {
    js_run_matches_luajit_end_to_end: both_backends(GATE, EXPECTED);
    js_run_project_dir_matches_luajit: both_backends_project(PROJECT_FILES, "sum=30\n");
    js_run_modules_v1_bin_package_runs_main: both_backends_project(MODULES_V1_FILES, "17\n");
    js_run_no_main_is_load_effects_only: both_backends(NO_MAIN, "");
}
