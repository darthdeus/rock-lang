//! M1 end-to-end gate: `rock run <file> --backend js` emits the whole file as a
//! node program (module ordering + `__G` ginit topo + decorator APPLICATIONS at
//! module load + a co fn) and runs `main` end to end, producing byte-identical
//! output to the luajit `rock run` of the same file.
//!
//! Drives the real binary on both backends so the whole driver path (capability
//! gate -> emit_program -> node stream) is exercised, and pins the cross-backend
//! parity that is M1's contract.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

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
    let mut cmd = Command::new(rock_bin());
    cmd.args(["run", path.to_str().unwrap()]);
    if let Some(b) = backend {
        cmd.args(["--backend", b]);
    }
    let out = cmd.output().expect("spawn rock");
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success())
}

#[test]
fn js_run_matches_luajit_end_to_end() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let dir = std::env::temp_dir().join("rock_js_run_gate");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("gate.rock");
    std::fs::write(&path, GATE).unwrap();

    let (lua_out, lua_ok) = run(None, &path);
    assert!(lua_ok, "luajit run failed:\n{lua_out}");
    assert_eq!(lua_out, EXPECTED, "luajit output drifted from the pinned expectation");

    let (js_out, js_ok) = run(Some("js"), &path);
    assert!(js_ok, "js run failed:\n{js_out}");
    assert_eq!(js_out, EXPECTED, "js output drifted from the pinned expectation");

    assert_eq!(js_out, lua_out, "js and luajit `rock run` output must be byte-identical");
}

// A file with no nullary `main` runs module-load effects only (ginit + deco
// applications), matching luajit -- and prints nothing, exiting cleanly.
#[test]
fn js_run_no_main_is_load_effects_only() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let dir = std::env::temp_dir().join("rock_js_run_gate_nomain");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nomain.rock");
    std::fs::write(&path, "global X: int = 7\nfn helper() -> int { X + 1 }\n").unwrap();

    let (js_out, js_ok) = run(Some("js"), &path);
    assert!(js_ok, "js run (no main) failed:\n{js_out}");
    assert_eq!(js_out, "", "no main -> no stdout");
}
