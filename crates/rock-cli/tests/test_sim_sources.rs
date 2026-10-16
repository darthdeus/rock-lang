
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("tests/sims")).unwrap();
    std::fs::write(dir.path().join("stick.toml"),
        "[package]\nname = \"sim-closure\"\ntype = \"lib\"\n").unwrap();
    for (name, source) in [
        ("main.rock", r#"
fn production() -> int { 1 }
@lifecycle(phase="reset", owner="session") deco init fixture_reset {}
@nomangle inline(lua) fn verify_load_effects() = """
assert(_G.SIM_SOURCE_GLOBAL_EFFECT == 23)
assert(_G['external_' .. 'sim_helper']() == 27)
local input = assert(io.open(arg[0], 'rb'))
local source = input:read('*a')
input:close()
assert(source:find('function preserved_' .. 'sim_hook', 1, true))
"""
"#),
        ("tests/reached.rock", "@test fn reached() { assert(sim_helper() == 42); verify_load_effects() }\n"),
        ("tests/sims/helper.rock", "fn sim_helper() -> int { transitive_helper() }\n"),
        ("tests/sims/transitive.rock", "fn transitive_helper() -> int { 42 }\n"),
        ("tests/sims/test.rock", "@test fn test_under_sims() { assert(production() == 1) }\n"),
        ("tests/sims/global.rock", r#"
inline(lua) fn set_effect_marker() -> int = "_G.SIM_SOURCE_GLOBAL_EFFECT = 23; return 0"
global effect_load: int = set_effect_marker()
"#),
        ("tests/sims/external.rock", "@nomangle inline(lua) fn external_sim_helper() -> int = \"return 27\"\n"),
        ("tests/sims/lifecycle.rock", "@fixture_reset fn preserved_sim_hook() {}\n"),
        ("tests/sims/unused.rock", "const UNUSED_POPULATION: int = 10\n@sim fn unused_sim() { missing_unused_sim_name() }\n"),
    ] {
        std::fs::write(dir.path().join(name), source).unwrap();
    }
    dir
}

fn run(dir: &std::path::Path) -> (bool, String) {
    let output = crate::command::rock_bin()
        .arg("test").arg(dir)
        .env("ROCK_TEST_VERBOSE", "1")
        .env_remove("ROCK_TEST_FILTER")
        .output().unwrap();
    (output.status.success(), format!("{}{}", String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)))
}

#[test]
fn package_tests_skip_unused_sims_and_keep_transitive_helpers_and_tests() {
    let fixture = fixture();
    let (success, text) = run(fixture.path());
    assert!(success, "unused sim must not be typechecked:\n{text}");
    assert!(text.contains("2/2 @tests passed"), "{text}");
}

#[test]
fn a_missing_name_in_a_selected_sim_helper_fails_loudly() {
    let fixture = fixture();
    std::fs::write(fixture.path().join("tests/sims/transitive.rock"),
        "fn transitive_helper() -> int { missing_selected_sim_name() }\n").unwrap();
    let (success, text) = run(fixture.path());
    assert!(!success, "missing selected helper must fail:\n{text}");
    assert!(text.contains("missing_selected_sim_name"), "{text}");
}

#[test]
fn uncertain_scanning_logs_a_full_set_fallback_and_rejects_bad_source() {
    let fixture = fixture();
    std::fs::write(fixture.path().join("tests/sims/unused.rock"),
        "fn unused_sim() { \"unterminated\n").unwrap();
    let (success, text) = run(fixture.path());
    assert!(!success, "uncertain source must not silently disappear:\n{text}");
    assert!(text.contains("package test sim closure kept all"), "{text}");
}

#[test]
fn sim_listing_still_typechecks_the_full_sim_source_set() {
    let fixture = fixture();
    let output = crate::command::rock_bin()
        .args(["sim", "--list"]).arg(fixture.path())
        .output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr));
    assert!(!output.status.success(), "sim compilation must keep the unused sim:\n{text}");
    assert!(text.contains("missing_unused_sim_name"), "{text}");
}
