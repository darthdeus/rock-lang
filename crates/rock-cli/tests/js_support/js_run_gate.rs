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

const COALESCE_VALUE_FIELD: &str = r#"
struct Thing { at: Vec2 = v(0, 0) }

fn main() {
    let fallback: Vec2? = null
    let t = Thing { at: fallback ?? v(7, 8) }
    print("{t.at.x as int} {t.at.y as int}")
}
"#;

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

// A `value` row's borrow is stored flat and crosses calls as `vec, slot`, a
// two-lane wire no declaration ABI plan describes; the program's return bank
// must still be sized for it.
const VALUE_ROW_BORROW: &str = r#"
value Cell { n: int }
fn main() {
    let a: Arena<Cell> = arena_new()
    let h = a.insert(Cell { n: 10 })
    let cell: ref Cell = a.borrow(h).unwrap()
    cell.n = 11
    print(cell.n)
    print(h.get().unwrap().n)
}
"#;

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

// A generic `co fn` at a COLUMN-stored element must be emitted concretely,
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

// An `Iterator`-bounded body with two instantiations must give the same answer
// on both backends. Two different element types force the placeholder `next`
// to resolve per instantiation.
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

// A direct UFCS call through safe navigation, where the receiver is a `value`.
// Both factors are required: a `value` receiver scalarizes through the call, and
// `?.` builds the call on the guarded path, and only the pair loses the
// instantiation. Three `?.get()` sites on `Entity<T>` in a downstream project
// are this shape, and they are why that project stopped compiling.
//
// The backends DIVERGED here, luajit rejecting what JS emitted correctly, so a
// single-backend fixture would have read as either "correctly rejected" or
// "works". The controls below hold each factor fixed in turn; all three arms
// print the same two lines, so a green cannot come from a different plausible
// answer.
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

// A generic call from a `co fn`'s lifecycle HOOK. The hook body is peeled to
// the constructor and left detached -- unreachable from the step entry and never
// hoisted into `nested_fns` -- so the walk that files specialization demands saw
// neither, while the walk that finds ordinary callees (every arena block) saw it
// fine. The result was a call to a specialization nothing defined.
//
// Both backends fail this without the fix, differently: luajit dies on a nil
// global at runtime, JS refuses to emit. Hence a cross-backend arm.
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

// The same defect reached through a `co fn`'s LAMBDA rather than a hook. It is
// the quieter half: the step runs under `__resume`'s pcall, so the missing
// definition lands in `s.__error` and surfaces as a wrong answer rather than a
// crash.
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

// Control on the REGION: the same generic call from the step's own body, which
// is entry-reachable and always filed its demand.
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

// Control on the REGION, detached arm: a `defer` body is also detached, and it
// did NOT lose its demand. So "detached" alone is not the discriminant, and a
// fix that keys on reachability alone would be keying on the wrong thing.
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

// Control on the CALLEE: a non-generic call from the same hook. It always
// worked, because ordinary callee discovery walks every arena block -- which is
// what localises the defect to the demand walk rather than to hook emission.
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

// A NON-GENERIC `next` witness reached through a generic combinator --
// `CharsIter.next` inside `count`. The producer must answer "no instantiation"
// for it rather than an empty key, because an empty key filed as a delivery
// record names a specialization nothing defines.
//
// Pinned by name because the alternative gate is invisible: deleting the
// producer's guard leaves the suite GREEN and only shrinks its DENOMINATOR,
// since a file whose compile panics contributes zero tests rather than a
// failure.
const NON_GENERIC_WITNESS: &str = r#"
fn main() {
    let n = "hello".chars().count()
    print("{n}")
}
"#;

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
    scalarized_coalesce_field_runs_on_both_backends: both_backends(COALESCE_VALUE_FIELD, "7 8\n");
    js_run_project_dir_matches_luajit: both_backends_project(PROJECT_FILES, "sum=30\n");
    js_run_value_row_borrow_runs_on_both_backends: both_backends(VALUE_ROW_BORROW, "11\n11\n");
    js_run_modules_v1_bin_package_runs_main: both_backends_project(MODULES_V1_FILES, "17\n");
    js_run_no_main_is_load_effects_only: both_backends(NO_MAIN, "");
    a_bounded_body_runs_on_both_backends: both_backends(BOUNDED_STANDALONE, "6 2\n");
    a_generic_coroutine_at_a_column_element_runs_on_both_backends: both_backends(GENERIC_CO_COLUMN, "2 3\n");
    a_generic_coroutine_at_a_boxed_element_runs_on_both_backends: both_backends(GENERIC_CO_BOXED, "2 3\n");
    a_specialized_coroutine_with_a_capturing_lambda_runs_on_both_backends: both_backends(GENERIC_CO_LAMBDA, "12\n");
    safe_nav_on_a_value_receiver_runs_on_both_backends: both_backends(SAFE_NAV_VALUE_RECEIVER, "300\n-1\n");
    safe_nav_on_a_struct_receiver_runs_on_both_backends: both_backends(SAFE_NAV_STRUCT_RECEIVER, "300\n-1\n");
    a_refined_value_receiver_runs_on_both_backends: both_backends(REFINED_VALUE_RECEIVER, "300\n-1\n");
    a_generic_call_from_a_coroutine_hook_runs_on_both_backends: both_backends(CO_HOOK_GENERIC, "1 killed\n");
    a_generic_call_from_a_coroutine_lambda_runs_on_both_backends: both_backends(CO_LAMBDA_GENERIC, "1 lam\n");
    a_generic_call_from_a_coroutine_body_runs_on_both_backends: both_backends(CO_BODY_GENERIC, "1 body\n");
    a_generic_call_from_a_coroutine_defer_runs_on_both_backends: both_backends(CO_DEFER_GENERIC, "1 defer\n");
    a_non_generic_call_from_a_coroutine_hook_runs_on_both_backends: both_backends(CO_HOOK_NON_GENERIC, "1\n");
    a_non_generic_iterator_witness_runs_on_both_backends: both_backends(NON_GENERIC_WITNESS, "5\n");
}
