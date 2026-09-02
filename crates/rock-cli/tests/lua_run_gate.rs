//! Desktop LuaJIT controls for run and emission. Each fixture remains a
//! separate process so its output is the load-bearing native-backend contract.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
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

fn run(path: &std::path::Path) -> (String, bool) {
    let out = Command::new(rock_bin())
        .args(["run", path.to_str().unwrap()])
        .output()
        .expect("spawn rock");
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success())
}

fn luajit_only(name: &str, src: &str, expected: &str) {
    let dir = std::env::temp_dir().join(format!("rock_lua_run_gate_{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.rock"));
    std::fs::write(&path, src).unwrap();

    let (lua_out, lua_ok) = run(&path);
    assert!(lua_ok, "luajit run failed:\n{lua_out}");
    assert_eq!(lua_out, expected, "luajit output");
}

const COALESCE_VALUE_FIELD: &str = r#"
struct Thing { at: Vec2 = v(0, 0) }

fn main() {
    let fallback: Vec2? = null
    let t = Thing { at: fallback ?? v(7, 8) }
    print("{t.at.x as int} {t.at.y as int}")
}
"#;

#[test]
fn scalarized_coalesce_field_runs_on_luajit() {
    let dir = std::env::temp_dir().join("rock_optional_run_gate_coalesce_value_field");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("coalesce_value_field.rock");
    std::fs::write(&path, COALESCE_VALUE_FIELD).unwrap();

    let (lua_out, lua_ok) = run(&path);
    assert!(lua_ok, "luajit run failed:\n{lua_out}");
    assert_eq!(lua_out, "7 8\n", "luajit must preserve the selected scalarized field value");
}

#[test]
fn lua_run_matches_the_desktop_contract() {
    let dir = std::env::temp_dir().join("rock_optional_run_gate");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("gate.rock");
    std::fs::write(&path, GATE).unwrap();

    let (lua_out, lua_ok) = run(&path);
    assert!(lua_ok, "luajit run failed:\n{lua_out}");
    assert_eq!(lua_out, EXPECTED, "luajit output drifted from the pinned expectation");
}

// A multi-file project (main + a sibling module) emits via the deterministic
// module walk and runs `main` end to end.
#[test]
fn lua_run_project_dir_reaches_the_desktop_main() {
    let dir = std::env::temp_dir().join("rock_optional_run_gate_proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stick.toml"), "[package]\nname = \"proj\"\n").unwrap();
    std::fs::write(dir.join("math.rock"), "fn square(x: int) -> int { return x * x }\n").unwrap();
    std::fs::write(
        dir.join("main.rock"),
        "fn main() {\n  let t = 0\n  for i in 0..5 { t = t + square(i) }\n  print(\"sum={t}\")\n}\n",
    )
    .unwrap();

    let (lua_out, lua_ok) = run(&dir);
    assert!(lua_ok, "luajit project run failed:\n{lua_out}");
    assert_eq!(lua_out.trim(), "sum=30", "luajit project output drifted");
}

// A generic `co fn` at a column-stored element must be emitted concretely, one
// body per instantiation. The shared erased body reads a `_data` that a column
// vec does not have, so it iterates zero times.
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
// It keeps the shared body correct, so a green above cannot come from generic
// coroutines stopping altogether.
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
// instantiation when inlining is disabled, which is what the ablation mode
// does and what E1 does by construction.
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
fn without_census(out: &str) -> String {
    out.lines()
        .take_while(|l| !l.contains("Stage-0e totality ablation"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_ablated(path: &std::path::Path) -> (String, bool) {
    let out = Command::new(rock_bin())
        .args(["run", path.to_str().unwrap()])
        .env("ROCK_MONO_ABLATION", "1")
        .output()
        .expect("spawn rock");
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success())
}

#[test]
fn a_bounded_body_runs_on_luajit_with_inlining_disabled() {
    let dir = std::env::temp_dir().join("rock_optional_run_gate_bounded");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bounded.rock");
    std::fs::write(&path, BOUNDED_STANDALONE).unwrap();

    let (spliced, ok) = run(&path);
    assert!(ok, "luajit run (inlining ON) failed:\n{spliced}");
    assert_eq!(spliced, "6 2\n", "the control's own answer drifted");

    let (lua_out, _) = run_ablated(&path);
    assert_eq!(
        without_census(&lua_out).trim(),
        "6 2",
        "luajit emitted the bounded bodies standalone but got the wrong answer"
    );
}

#[test]
fn a_generic_coroutine_at_a_column_element_runs_on_luajit_only() {
    luajit_only("gen_co_column", GENERIC_CO_COLUMN, "2 3\n");
}

#[test]
fn a_generic_coroutine_at_a_boxed_element_runs_on_luajit_only() {
    luajit_only("gen_co_boxed", GENERIC_CO_BOXED, "2 3\n");
}

// A specialized coroutine still goes through the frame machinery: the lambda
// captures a frame cell and must preserve that capture.
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
fn a_specialized_coroutine_with_a_capturing_lambda_runs_on_luajit_only() {
    luajit_only("gen_co_lambda", GENERIC_CO_LAMBDA, "12\n");
}

// A direct UFCS call through safe navigation, where a value receiver is
// scalarized through the guarded call path. The controls below hold each factor
// fixed in turn; all three arms print the same two lines.
const SAFE_NAV_VALUE_RECEIVER: &str = r#"
value Handle<T> { slot: int, payload: T }

fn Handle<T>.fetch(h) -> T? {
    if h.slot < 0 { return null }
    return h.payload
}

fn find(slot: int) -> Handle<int>? {
    if slot == 0 { return null }
    return Handle { slot: slot, payload: slot * 100 }
}

fn main() {
    print("{find(3)?.fetch() ?? -1}")
    print("{find(0)?.fetch() ?? -1}")
}
"#;

// Control on the RECEIVER: a boxed `struct`, safe navigation unchanged.
const SAFE_NAV_STRUCT_RECEIVER: &str = r#"
struct Handle<T> { slot: int, payload: T }

fn Handle<T>.fetch(h) -> T? {
    if h.slot < 0 { return null }
    return h.payload
}

fn find(slot: int) -> Handle<int>? {
    if slot == 0 { return null }
    return Handle { slot: slot, payload: slot * 100 }
}

fn main() {
    print("{find(3)?.fetch() ?? -1}")
    print("{find(0)?.fetch() ?? -1}")
}
"#;

// Control on the CALL FORM: the same `value` receiver reached through a
// refinement instead of `?.`, which is the ordinary direct UFCS call the
// safe-nav one has to agree with.
const REFINED_VALUE_RECEIVER: &str = r#"
value Handle<T> { slot: int, payload: T }

fn Handle<T>.fetch(h) -> T? {
    if h.slot < 0 { return null }
    return h.payload
}

fn find(slot: int) -> Handle<int>? {
    if slot == 0 { return null }
    return Handle { slot: slot, payload: slot * 100 }
}

fn probe(slot: int) -> int? {
    let h = find(slot)
    if h != null { return h.fetch() }
    return null
}

fn main() {
    print("{probe(3) ?? -1}")
    print("{probe(0) ?? -1}")
}
"#;

#[test]
fn safe_nav_on_a_value_receiver_runs_on_luajit_only() {
    luajit_only("safe_nav_value", SAFE_NAV_VALUE_RECEIVER, "300\n-1\n");
}

#[test]
fn safe_nav_on_a_struct_receiver_runs_on_luajit_only() {
    luajit_only("safe_nav_struct", SAFE_NAV_STRUCT_RECEIVER, "300\n-1\n");
}

#[test]
fn a_refined_value_receiver_runs_on_luajit_only() {
    luajit_only("safe_nav_refined", REFINED_VALUE_RECEIVER, "300\n-1\n");
}

// A generic call from a coroutine lifecycle hook. The hook body is detached from
// the step entry but still needs its specialization demand recorded.
const CO_HOOK_GENERIC: &str = r#"
global LOG: [string] = []

co fn worker() -> Coroutine<int> {
    on_killed {
        LOG.push("killed")
    }
    yield 1
    yield 2
}

fn main() {
    let c = worker()
    let _ = c.resume()
    c.kill()
    print("{LOG.len()} {LOG[0]}")
}
"#;

// The same demand reached through a coroutine lambda rather than a hook.
const CO_LAMBDA_GENERIC: &str = r#"
global LOG: [string] = []

co fn worker() -> Coroutine<int> {
    let add = () => { LOG.push("lam") }
    add()
    yield 1
}

fn main() {
    let c = worker()
    let _ = c.resume()
    print("{LOG.len()} {LOG[0]}")
}
"#;

// Control on the region: the same generic call from the step's own body.
const CO_BODY_GENERIC: &str = r#"
global LOG: [string] = []

co fn worker() -> Coroutine<int> {
    LOG.push("body")
    yield 1
}

fn main() {
    let c = worker()
    let _ = c.resume()
    print("{LOG.len()} {LOG[0]}")
}
"#;

// Control on a detached defer body: detachment alone does not remove its demand.
const CO_DEFER_GENERIC: &str = r#"
global LOG: [string] = []

co fn worker() -> Coroutine<int> {
    defer {
        LOG.push("defer")
    }
    yield 1
    yield 2
}

fn main() {
    let c = worker()
    let _ = c.resume()
    c.kill()
    print("{LOG.len()} {LOG[0]}")
}
"#;

// Control on the callee: a non-generic call from the same hook.
const CO_HOOK_NON_GENERIC: &str = r#"
global N: int = 0

fn bump() { N = N + 1 }

co fn worker() -> Coroutine<int> {
    on_killed { bump() }
    yield 1
    yield 2
}

fn main() {
    let c = worker()
    let _ = c.resume()
    c.kill()
    print("{N}")
}
"#;

#[test]
fn a_generic_call_from_a_coroutine_hook_runs_on_luajit_only() {
    luajit_only("co_hook_generic", CO_HOOK_GENERIC, "1 killed\n");
}

#[test]
fn a_generic_call_from_a_coroutine_lambda_runs_on_luajit_only() {
    luajit_only("co_lambda_generic", CO_LAMBDA_GENERIC, "1 lam\n");
}

#[test]
fn a_generic_call_from_a_coroutine_body_runs_on_luajit_only() {
    luajit_only("co_body_generic", CO_BODY_GENERIC, "1 body\n");
}

#[test]
fn a_generic_call_from_a_coroutine_defer_runs_on_luajit_only() {
    luajit_only("co_defer_generic", CO_DEFER_GENERIC, "1 defer\n");
}

#[test]
fn a_non_generic_call_from_a_coroutine_hook_runs_on_luajit_only() {
    luajit_only("co_hook_nongeneric", CO_HOOK_NON_GENERIC, "1\n");
}

// A NON-GENERIC `next` witness reached through a generic combinator --
// `CharsIter.next` inside `count`. The producer must answer "no instantiation"
// for it rather than an empty key, because an empty key filed as a delivery
// record names a specialization nothing defines.
//
const NON_GENERIC_WITNESS: &str = r#"
fn main() {
    let n = "hello".chars().count()
    print("{n}")
}
"#;

#[test]
fn a_non_generic_iterator_witness_runs_on_luajit_only() {
    luajit_only("non_generic_witness", NON_GENERIC_WITNESS, "5\n");
}
