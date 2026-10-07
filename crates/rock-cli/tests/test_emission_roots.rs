use std::process::Command;

#[test]
fn package_tests_emit_only_reachable_functions_and_global_effects() {
    let dir = tempfile::tempdir().unwrap();
    let dependency = dir.path().join("dependency");
    let package = dir.path().join("package");
    std::fs::create_dir_all(dependency.join("tests")).unwrap();
    std::fs::create_dir_all(package.join("tests")).unwrap();
    std::fs::write(dependency.join("stick.toml"),
        "[package]\nname = \"dependency\"\ntype = \"lib\"\n").unwrap();
    std::fs::write(dependency.join("lib.rock"), r#"
fn reached_dependency() -> int { 42 }
fn unreachable_dependency() -> string { "UNREACHABLE_DEPENDENCY_BODY" }
@nomangle fn externally_named() -> int { 7 }
fn register_sim(name: string, body: fn()) {}
@sim fn unused_sim() { print("UNREACHABLE_SIM_BODY") }
@lifecycle(phase="reset", owner="session")
deco init fixture_reset {}
@fixture_reset fn reset_hook() { print("RETAINED_LIFECYCLE_BODY") }
"#).unwrap();
    std::fs::write(dependency.join("tests/unused.rock"), r#"
@test fn dependency_test() { print("UNREACHABLE_DEPENDENCY_TEST") }
"#).unwrap();
    std::fs::write(package.join("stick.toml"),
        "[package]\nname = \"package\"\ntype = \"lib\"\n[dependencies]\ndependency = { path = \"../dependency\" }\n").unwrap();
    std::fs::write(package.join("lib.rock"), r#"
fn unreachable_own() -> string { "UNREACHABLE_OWN_BODY" }
fn initializer() -> int { 9 }
global initialized: int = initializer()
@nomangle inline(lua) fn capture_program() = """
local input = assert(io.open(arg[0], 'rb'))
local output = assert(io.open(os.getenv('ROCK_FIXTURE_PROGRAM'), 'wb'))
output:write(input:read('*a'))
output:close()
input:close()
"""
"#).unwrap();
    std::fs::write(package.join("tests/reached.rock"), r#"
@test fn selected_test() {
    assert(reached_dependency() == 42)
    assert(initialized == 9)
    capture_program()
}
"#).unwrap();
    let program = dir.path().join("program.lua");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("test").arg(&package)
        .env("ROCK_FIXTURE_PROGRAM", &program)
        .env("ROCK_TEST_SHARDS", "1")
        .env_remove("ROCK_TEST_FILTER")
        .output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "{text}");
    assert!(text.contains("1/1 @tests passed"), "{text}");
    let lua = std::fs::read_to_string(program).unwrap();
    for marker in ["UNREACHABLE_DEPENDENCY_BODY", "UNREACHABLE_DEPENDENCY_TEST", "UNREACHABLE_OWN_BODY", "UNREACHABLE_SIM_BODY"] {
        assert!(!lua.contains(marker), "unreachable code emitted: {marker}");
    }
    assert!(lua.contains("function externally_named("), "external entry point pruned");
    assert!(lua.contains("RETAINED_LIFECYCLE_BODY"), "global lifecycle hook pruned");
}
