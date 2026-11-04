//! LuaJIT line directives (`ROCK_SKULL_LINE_DIRECTIVES`): a numbered program
//! reports Rock file:line for its definitions and its own file:line for the
//! generated code around them.

use crate::common;
use skull::mir_emit::ShapeOverrides;

const FILE: &str = "/src/game.rock";

const SRC: &str = "fn fail(n: int) -> int {
    let a = n * 2
    if a > 4 {
        error(\"boom\")
    }
    return a + 1
}

fn main() {
    print(fail(1))
}
";

fn numbered() -> String {
    common::emit_module_shaped(
        SRC,
        FILE,
        ShapeOverrides {
            line_directives: Some(true),
            ..Default::default()
        },
    )
}

#[test]
fn generated_code_after_a_numbered_definition_is_numbered_by_its_place_in_main_lua() {
    let lua = numbered();
    assert!(
        lua.lines()
            .any(|line| line.starts_with('#') && line.contains(FILE)),
        "no directive names the Rock file:\n{lua}"
    );
    let resets: Vec<(usize, &str)> = lua
        .lines()
        .enumerate()
        .filter(|(_, line)| line.ends_with("\"main.lua\""))
        .collect();
    assert!(!resets.is_empty(), "no reset to main.lua:\n{lua}");
    for (index, line) in resets {
        // `index` is zero-based; the directive numbers the line after it.
        assert_eq!(line, format!("# {} \"main.lua\"", index + 2), "{lua}");
    }
    assert!(
        !lua.contains(skull_codegen_lua::LINE_DIRECTIVE_RESET),
        "unresolved reset:\n{lua}"
    );
}

/// Needs a LuaJIT that reads line directives (LuaJIT 2.1 commit c41d727d or
/// later); `ROCK_LINE_DIRECTIVE_LUAJIT` names its executable.
#[test]
#[ignore = "needs ROCK_LINE_DIRECTIVE_LUAJIT: a LuaJIT 2.1 with line directives (c41d727d or later)"]
fn errors_tracebacks_and_jit_util_report_rock_lines() {
    let luajit = std::env::var("ROCK_LINE_DIRECTIVE_LUAJIT")
        .expect("ROCK_LINE_DIRECTIVE_LUAJIT names a LuaJIT with line directives");
    let lua = numbered();
    let fail = common::mangled_name(&lua, "fail");
    let dir = std::env::temp_dir().join(format!("skull_line_directives_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let program = dir.join("main.lua");
    std::fs::write(&program, &lua).expect("write program");
    let driver = format!(
        r#"assert(loadfile({program:?}))()
local _, err = pcall({fail}, 5)
print("error=" .. tostring(err))
local _, trace = xpcall({fail}, debug.traceback, 5)
print("trace=" .. trace:gsub("\n", " "))
local info = require("jit.util").funcinfo({fail})
print("source=" .. info.source)
print("linedefined=" .. info.linedefined)
"#
    );
    let out = std::process::Command::new(&luajit)
        .arg("-e")
        .arg(driver)
        .output()
        .expect("spawn luajit");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let field = |name: &str| -> &str {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            .unwrap_or_else(|| panic!("no {name} in {stdout}"))
    };
    assert!(
        field("error").starts_with(&format!("{FILE}:4: boom")),
        "{stdout}"
    );
    assert!(field("trace").contains(&format!("{FILE}:4:")), "{stdout}");
    assert_eq!(field("source"), format!("@{FILE}"), "{stdout}");
    let linedefined: u32 = field("linedefined").parse().expect("linedefined");
    assert!((1..=7).contains(&linedefined), "{stdout}");
}
