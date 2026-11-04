//! EXPLICIT_NULL_DEFAULT: an explicit `null` argument overrides a parameter's
//! non-null default. Omitting the argument gives the default; passing `null`,
//! literally or through a variable, gives null, on the Lua and JS backends.

use std::path::Path;

const SHARED: &str = r#"
fn seeded(seed: int? = 42) -> int? {
    return seed
}

fn forward(seed: int?) -> int? {
    return seeded(seed)
}

@test
fn plain_int_default() {
    assert_eq(seeded() ?? -1, 42)
    assert_eq(seeded(7) ?? -1, 7)
    assert_null(seeded(null))
    assert_null(seeded(seed: null))
    assert_eq(seeded(seed: 9) ?? -1, 9)
}

@test
fn forwarded_null_variable() {
    let s: int? = null
    assert_null(seeded(s))
    assert_null(forward(null))
    assert_eq(forward(3) ?? -1, 3)
}

struct Hint { v: int }

fn hinted(hint: Hint? = Hint { v: 1 }) -> int {
    if hint == null {
        return -1
    }
    return hint.unwrap().v
}

@test
fn struct_default() {
    assert_eq(hinted(), 1)
    assert_eq(hinted(null), -1)
    assert_eq(hinted(Hint { v: 5 }), 5)
    let h: Hint? = null
    assert_eq(hinted(h), -1)
}

value V2 { x: int, y: int }

fn placed(at: V2? = V2 { x: 1, y: 2 }) -> int {
    if at == null {
        return -1
    }
    let p = at.unwrap()
    return p.x * 10 + p.y
}

@test
fn value_struct_default() {
    assert_eq(placed(), 12)
    assert_eq(placed(null), -1)
    assert_eq(placed(V2 { x: 3, y: 4 }), 34)
}

struct Counter { c: int }

fn Counter.bump(self, n: int? = 1) -> int {
    return self.c + (n ?? 100)
}

@test
fn method_default() {
    let k = Counter { c: 0 }
    assert_eq(k.bump(), 1)
    assert_eq(k.bump(null), 100)
    assert_eq(k.bump(n: null), 100)
    assert_eq(k.bump(5), 5)
}

// Two defaulted nullable parameters: positional, named and trailing omission
// each reach the right slot.
fn pair(a: int? = 1, b: int? = 2) -> string {
    return "{a ?? -1},{b ?? -1}"
}

@test
fn named_positional_and_trailing() {
    assert_eq(pair(), "1,2")
    assert_eq(pair(null), "-1,2")
    assert_eq(pair(null, null), "-1,-1")
    assert_eq(pair(b: null), "1,-1")
    assert_eq(pair(a: null), "-1,2")
    assert_eq(pair(5), "5,2")
    assert_eq(pair(b: 6), "1,6")
}

fn generic_n<T>(x: T, n: int? = 5) -> int {
    return n ?? -1
}

@test
fn generic_callee() {
    assert_eq(generic_n(1), 5)
    assert_eq(generic_n("a", null), -1)
    assert_eq(generic_n(1.5, n: null), -1)
}

fn flag(on: bool? = true) -> int {
    if on == null {
        return -1
    }
    return if on.unwrap() { 1 } else { 0 }
}

fn label(s: string? = "dflt") -> string {
    return s ?? "none"
}

@test
fn bool_and_string_defaults() {
    assert_eq(flag(), 1)
    assert_eq(flag(false), 0)
    assert_eq(flag(null), -1)
    assert_eq(label(), "dflt")
    assert_eq(label(null), "none")
}

// A null default and a non-nullable default keep their meaning.
fn plain(n: int = 3, m: int? = null) -> int {
    return n + (m ?? 0)
}

@test
fn other_defaults_unchanged() {
    assert_eq(plain(), 3)
    assert_eq(plain(m: 4), 7)
    assert_eq(plain(1, null), 1)
}
"#;

// The Lua-only boundaries: an `inline(lua)` wrapper and a decorator.
const LUA_ONLY: &str = r#"
inline(lua) fn lua_seeded(n: int? = 7) -> int = "return n == nil and -1 or n"

@test
fn inline_lua_default() {
    assert_eq(lua_seeded(), 7)
    assert_eq(lua_seeded(3), 3)
    assert_eq(lua_seeded(null), -1)
}

global REG: [string] = []

deco init register(priority: int? = 100) {
    vec_push(REG, "{target.name}:{priority ?? -1}")
}

@register
fn sys_default() {}

@register(priority = 10)
fn sys_ten() {}

@test
fn decorator_default() {
    let joined = ""
    for e in REG {
        joined = joined + e + ";"
    }
    assert_eq(joined.contains("sys_default:100;"), true)
    assert_eq(joined.contains("sys_ten:10;"), true)
}
"#;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

fn rock_test_lua(src: &str, expected: &str) {
    let dir = tempfile::tempdir().expect("fixture dir");
    let file = dir.path().join("explicit_null.rock");
    std::fs::write(&file, src).expect("write fixture");
    let output = crate::command::rock_bin()
        .args(["test", file.to_str().unwrap()])
        .current_dir(workspace_root())
        .output()
        .expect("spawn rock");
    let text = test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ));
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains(expected),
        "expected {expected} Lua tests to pass:\n{text}"
    );
}

#[test]
fn explicit_null_overrides_default_on_lua() {
    rock_test_lua(SHARED, "9/9");
}

#[test]
fn explicit_null_overrides_default_at_lua_boundaries() {
    rock_test_lua(LUA_ONLY, "2/2");
}

#[test]
fn explicit_null_overrides_default_on_js() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("SKIP explicit_null_overrides_default_on_js: `node` not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("fixture dir");
    let file = dir.path().join("explicit_null.rock");
    std::fs::write(&file, SHARED).expect("write fixture");
    let summary = rock_cli::js_test::js_test_run(&[file.to_string_lossy().into_owned()], false)
        .expect("js test run");
    assert_eq!(summary.unit_failures, 0);
    assert_eq!(summary.tests_failed, 0);
    assert_eq!(summary.tests_skipped, 0);
    assert_eq!(summary.tests_passed, 9);
}
