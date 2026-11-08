use std::process::Command;

#[test]
fn stone_attributes_source_and_stripped_programs_to_their_loaded_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"),
        "[package]\nname = \"identity-probe\"\ntype = \"bin\"\n[language]\nmodules = \"v1\"\n[build.vars]\nGIT_SHA = \"fixture\"\n[dependencies]\nstone = { package = \"stone\" }\n").unwrap();
    std::fs::write(
        dir.path().join("main.rock"),
        r#"
inline(lua) fn check_identity() = """
    local registry = __stone_jit_program_identity()
    local info = require("jit.util").funcinfo
    assert(registry.id(info(_G.expected_root)) == 0, "loaded program root must have ID zero")
    assert(registry.id(info(_G.host_wrapper)) == nil, "host wrapper must not belong to the program")
"""
fn main() { check_identity() }
"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["compile", ".", "main.lua", "--lua-line-directives"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let driver = dir.path().join("driver.lua");
    std::fs::write(
        &driver,
        r#"
_G.host_wrapper = debug.getinfo(1, "f").func
local chunk = assert(loadfile("main.lua"))
if arg[1] == "stripped" then
    chunk = assert(loadstring(string.dump(chunk, true), "@program.bc"))
end
_G.expected_root = chunk
chunk()
"#,
    )
    .unwrap();
    for mode in ["source", "stripped"] {
        let output = rock_engine_host::runtime::build_luajit_command(&driver, &[mode.into()])
            .current_dir(dir.path())
            .env("STONE_HEADLESS", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
