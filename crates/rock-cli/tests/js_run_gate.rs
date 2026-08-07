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

// Arc 2 MA: `rock run <dir> --backend js` (project mode) -- a multi-file
// project (main + a sibling module) emits via the deterministic module walk and
// runs `main` end to end, byte-identical to the luajit `rock run` of the dir.
#[test]
fn js_run_project_dir_matches_luajit() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let dir = std::env::temp_dir().join("rock_js_run_gate_proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stick.toml"), "[package]\nname = \"proj\"\n").unwrap();
    std::fs::write(dir.join("math.rock"), "fn square(x: int) -> int { return x * x }\n").unwrap();
    std::fs::write(
        dir.join("main.rock"),
        "fn main() {\n  let t = 0\n  for i in 0..5 { t = t + square(i) }\n  print(\"sum={t}\")\n}\n",
    )
    .unwrap();

    let (lua_out, lua_ok) = run(None, &dir);
    assert!(lua_ok, "luajit project run failed:\n{lua_out}");
    let (js_out, js_ok) = run(Some("js"), &dir);
    assert!(js_ok, "js project run failed:\n{js_out}");
    assert_eq!(js_out.trim(), "sum=30", "js project output drifted");
    assert_eq!(js_out.trim(), lua_out.trim(), "js and luajit project run must agree");
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

// D-3: a generic `co fn` at a COLUMN-stored element must be emitted concretely,
// one body per instantiation, on BOTH backends. The shared erased body reads a
// `_data` that a column vec does not have, so it iterates zero times.
//
// The two backends used to fail this differently, which is why the gate is here
// rather than in one crate's unit tests: luajit took a stop-gap rejection while
// JS carried the SILENT miscompile and printed `0 0`. Cross-backend parity is
// the arm that sees both.
const GENERIC_CO_COLUMN: &str = r#"
nobox value P { x: float, y: float }
nobox value Q { a: float }

co fn walk<T>(v: [T]) -> Coroutine<int, int, ()> {
    let n = 0
    for _e in v { n = n + 1; yield n }
    return n
}

fn main() {
    let ps: [P] = [P { x: 1.0, y: 0.0 }, P { x: 2.0, y: 0.0 }]
    let qs: [Q] = [Q { a: 1.0 }, Q { a: 2.0 }, Q { a: 3.0 }]
    let cp = walk(ps)
    let tp = 0
    for v in cp { tp = v }
    let cq = walk(qs)
    let tq = 0
    for v in cq { tq = v }
    print("{tp} {tq}")
}
"#;

// The control on the ELEMENT: the same generic `co fn` over BOXED elements. It
// keeps the shared body on both backends and must stay correct, so a green
// above cannot come from "generic coroutines stopped working".
const GENERIC_CO_BOXED: &str = r#"
co fn walk<T>(v: [T]) -> Coroutine<int, int, ()> {
    let n = 0
    for _e in v { n = n + 1; yield n }
    return n
}

fn main() {
    let xs: [int] = [10, 20]
    let ys: [string] = ["a", "b", "c"]
    let cx = walk(xs)
    let tx = 0
    for v in cx { tx = v }
    let cy = walk(ys)
    let ty = 0
    for v in cy { ty = v }
    print("{tx} {ty}")
}
"#;

// D-1: an `Iterator`-bounded body must be emitted STANDALONE at each
// instantiation, on BOTH backends, when inlining is disabled -- which is what
// the ablation mode does, and what E1 does by construction.
//
// Two instantiations on purpose. One would be satisfied by a body that
// hard-codes a single `next`; two different element types force the placeholder
// to resolve per instantiation, which is the substitution D-1 adds.
const BOUNDED_STANDALONE: &str = r#"
fn total<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for v in it { s = s + v }
    return s
}

fn tally<I: Iterator<string>>(it: I) -> int {
    let n = 0
    for _v in it { n = n + 1 }
    return n
}

fn main() {
    let xs: [int] = [1, 2, 3]
    let ys: [string] = ["a", "b"]
    print("{total(xs.iter())} {tally(ys.iter())}")
}
"#;

/// The program's own stdout, with the ablation census stripped.
///
/// The census is on stdout, the same stream the program prints to, so comparing
/// raw output across backends would compare two censuses as well -- and those
/// legitimately differ (the backends examine different call populations).
fn without_census(out: &str) -> String {
    out.lines()
        .take_while(|l| !l.contains("Stage-0e totality ablation"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_ablated(backend: Option<&str>, path: &std::path::Path) -> (String, bool) {
    let mut cmd = Command::new(rock_bin());
    cmd.args(["run", path.to_str().unwrap()]);
    if let Some(b) = backend {
        cmd.args(["--backend", b]);
    }
    cmd.env("ROCK_MONO_ABLATION", "1");
    let out = cmd.output().expect("spawn rock");
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success())
}

#[test]
fn a_bounded_body_runs_on_both_backends_with_inlining_disabled() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let dir = std::env::temp_dir().join("rock_js_run_gate_bounded");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bounded.rock");
    std::fs::write(&path, BOUNDED_STANDALONE).unwrap();

    // The CONTROL, and it runs first: with the mode OFF the inliner splices the
    // bounded body, so this is the answer both backends have always produced.
    // Without it a matching pair below could be two identically-wrong runs.
    let (spliced, ok) = run(None, &path);
    assert!(ok, "luajit run (inlining ON) failed:\n{spliced}");
    assert_eq!(spliced, "6 2\n", "the control's own answer drifted");

    // Exit STATUS is not the oracle here: the mode is deliberately fatal
    // whenever it records anything, and the stdlib records plenty, so a
    // perfectly correct run exits non-zero. The program's own stdout is the
    // result, and it is asserted rather than the status.
    let (lua_out, _) = run_ablated(None, &path);
    let (js_out, _) = run_ablated(Some("js"), &path);

    assert_eq!(
        without_census(&lua_out).trim(),
        "6 2",
        "luajit emitted the bounded bodies standalone but got the wrong answer"
    );
    assert_eq!(
        without_census(&js_out).trim(),
        "6 2",
        "js emitted the bounded bodies standalone but got the wrong answer -- \
         this is the arm a per-backend rejection sentinel cannot see"
    );
}

fn both_backends(name: &str, src: &str, expected: &str) {
    let dir = std::env::temp_dir().join(format!("rock_js_run_gate_{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.rock"));
    std::fs::write(&path, src).unwrap();

    let (lua_out, lua_ok) = run(None, &path);
    assert!(lua_ok, "luajit run failed:\n{lua_out}");
    assert_eq!(lua_out, expected, "luajit output");

    let (js_out, js_ok) = run(Some("js"), &path);
    assert!(js_ok, "js run failed:\n{js_out}");
    assert_eq!(js_out, expected, "js output");
    assert_eq!(js_out, lua_out, "js and luajit must agree");
}

#[test]
fn a_generic_coroutine_at_a_column_element_runs_on_both_backends() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    both_backends("gen_co_column", GENERIC_CO_COLUMN, "2 3\n");
}

#[test]
fn a_generic_coroutine_at_a_boxed_element_runs_on_both_backends() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    both_backends("gen_co_boxed", GENERIC_CO_BOXED, "2 3\n");
}

// A specialized coroutine still goes through the frame machinery: the lambda
// captures a frame cell, so it must collapse to a capture of `s` in the
// specialized copy exactly as in the shared one. The two backends take different
// preparation sequences to get there, which is why this is a parity arm.
const GENERIC_CO_LAMBDA: &str = r#"
nobox value P { x: float, y: float }

co fn walk<T>(v: [T]) -> Coroutine<int, int, ()> {
    let n = v.len
    let bump = () => n + 10
    let k = bump()
    yield k
    return n
}

fn main() {
    let ps: [P] = [P { x: 1.0, y: 0.0 }, P { x: 2.0, y: 0.0 }]
    let c = walk(ps)
    let t = 0
    for v in c { t = v }
    print("{t}")
}
"#;

#[test]
fn a_specialized_coroutine_with_a_capturing_lambda_runs_on_both_backends() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    both_backends("gen_co_lambda", GENERIC_CO_LAMBDA, "12\n");
}
