//! `rock run --backend js` prints byte for byte what the luajit `rock run` of
//! the same file or project prints.
#![cfg(feature = "backend-js")]

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

// Without a nullary `main` only module-load effects run.
const NO_MAIN: &str = "global X: int = 7\nfn helper() -> int { X + 1 }\n";

/// `rock run <target>` prints `expected` on luajit and on node.
fn assert_backends_agree(dir: &std::path::Path, target: &str, expected: &str) {
    for backend in [&[][..], &["--backend", "js"]] {
        let out = crate::support::invoke(dir, &[&["run", target][..], backend].concat());
        assert_eq!(crate::support::success(out), expected, "{backend:?} output");
    }
}

fn both_backends(source: &str, expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("gate.rock"), source).unwrap();
    assert_backends_agree(dir.path(), "gate.rock", expected);
}

fn both_backends_project(files: &[(&str, &str)], expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    crate::support::write_files(dir.path(), files);
    assert_backends_agree(dir.path(), ".", expected);
}

macro_rules! parity_tests {
    ($($test:ident: $check:ident($input:expr, $expected:expr);)*) => {
        $(
            #[test]
            fn $test() {
                if crate::support::require_node() {
                    $check($input, $expected);
                }
            }
        )*
    };
}

parity_tests! {
    js_run_matches_luajit_end_to_end: both_backends(GATE, EXPECTED);
    js_run_modules_v1_bin_package_runs_main: both_backends_project(MODULES_V1_FILES, "17\n");
    js_run_no_main_is_load_effects_only: both_backends(NO_MAIN, "");
}
