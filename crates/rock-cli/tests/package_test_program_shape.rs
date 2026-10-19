//! A package's `rock test` program is loaded once per child and never
//! reloaded, so it omits the reload-only Artifact binding plan and Session-cell
//! joins; a game build of the same package keeps both.
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap().canonicalize().unwrap()
}

const MAIN: &str = r#"
nobox value Probe { x: int, y: int }
global SPOT: Probe = Probe { x: 3, y: 4 }
global counter: int = 0
pub fn bump() -> int {
    counter = counter + 1
    return counter
}
fn main() { print(bump()) }
"#;

// The needles are split so this source's own text never matches them.
const TEST: &str = r#"
inline(lua) fn program_shape() -> string = """
local source = debug.getinfo(1, "S").source
local text = source
if source:sub(1, 1) == "@" then
  local f = assert(io.open(source:sub(2), "rb"))
  text = f:read("*a")
  f:close()
end
local function has(needle) return text:find(needle, 1, true) ~= nil end
return table.concat({
  "read=" .. tostring(has("__rock_state_transaction" .. "_begin()")),
  "plan_stage=" .. tostring(has("__rock_artifact_binding_plan_stage" .. "(")),
  "cell_join=" .. tostring(has(string.char(10) .. "__rock_bind_state_cells" .. "(")),
  "published=" .. tostring(rawget(_G, "__ROCK_ARTIFACT_BINDING_PLAN") ~= nil),
}, " ")
"""

inline(lua) fn logical_probe_x() -> int = """
local entry = assert(__rock_state_entry_for_name("SPOT"))
return __rock_state_entry_get(entry).x
"""

inline(lua) fn logical_probe_set_x(value: int) = """
local entry = assert(__rock_state_entry_for_name("SPOT"))
local probe = __rock_state_entry_get(entry)
probe.x = value
__rock_state_entry_set(entry, probe)
"""

@test fn package_test_program_has_no_reload_only_state() {
    assert(bump() == 1, "fixture state")
    assert(logical_probe_x() == SPOT.x, "package tests expose the logical nobox value")
    let next = SPOT.x + SPOT.y
    logical_probe_set_x(next)
    assert(SPOT.x == next, "package tests write the logical value through its compiler adapter")
    let shape = program_shape()
    assert(shape == "read=true plan_stage=false cell_join=false published=false", shape)
}
"#;

#[test]
fn package_test_program_omits_reload_only_state_a_game_build_keeps() {
    let dir = tempfile::tempdir().expect("fixture package");
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join("tests")).unwrap();
    std::fs::write(project.join("stick.toml"),
        "[package]\nname = \"test-program-shape\"\ntype = \"bin\"\n\
         [language]\nmodules = \"v1\"\n").unwrap();
    std::fs::write(project.join("main.rock"), MAIN).unwrap();
    std::fs::write(project.join("tests/shape.rock"), TEST).unwrap();

    let test = std::process::Command::new(env!("CARGO_BIN_EXE_rock-plain"))
        .arg("test").arg(project.join("tests"))
        .current_dir(root())
        .env("ROCK_TEST_VERBOSE", "1")
        .env_remove("ROCK_TEST_FILTER")
        .output().expect("run fixture package tests");
    let text = format!("{}{}", String::from_utf8_lossy(&test.stdout),
        String::from_utf8_lossy(&test.stderr));
    assert!(test.status.success(), "package test failed:\n{text}");
    assert!(text.contains("1/1 @tests passed"), "expected one passing test:\n{text}");

    let lua = dir.path().join("game.lua");
    let compile = std::process::Command::new(env!("CARGO_BIN_EXE_rock-plain"))
        .arg("compile").arg(&project).arg("--output").arg(&lua)
        .current_dir(root())
        .output().expect("compile fixture game");
    assert!(compile.status.success(), "game build failed:\n{}",
        String::from_utf8_lossy(&compile.stderr));
    let game = std::fs::read_to_string(&lua).expect("read game build");
    assert!(game.contains("__rock_artifact_binding_plan_stage("),
        "game build lost its Artifact binding plan");
    assert!(game.contains("\n__rock_bind_state_cells("), "game build lost its Session-cell joins");
}
