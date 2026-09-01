//! Tier 1.5 item 1 coverage after moving the cimgui embedder into stone.
//!
//! The source files are deliberately read from their package root here rather
//! than through a Rust constant: these tests must prove the Rock
//! `embed_string` inputs, the generated Lua body, and the JS sibling are the
//! live mechanism after the Rust embedder is deleted.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn prelude_path() -> PathBuf {
    repo_root().join("stone/engine/ui/imgui_prelude.rock")
}

fn prelude_source() -> String {
    std::fs::read_to_string(prelude_path()).expect("stone imgui prelude must exist")
}

fn cimgui_file(name: &str) -> String {
    std::fs::read_to_string(repo_root().join("stone/prebuilt/cimgui").join(name))
        .unwrap_or_else(|e| panic!("read cimgui/{name}: {e}"))
}

fn cimgui_files() -> [(&'static str, &'static str); 5] {
    [
        ("cdef.lua", "cimgui.cdef"),
        ("master.lua", "cimgui.master"),
        ("enums.lua", "cimgui.enums"),
        ("wrap.lua", "cimgui.wrap"),
        ("init.lua", "cimgui.init"),
    ]
}

fn lua_quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn lua_body() -> String {
    let source = prelude_source();
    let start = source
        .find("inline(lua) fn __imgui_prelude_init")
        .expect("Lua prelude declaration");
    let body_start = source[start..]
        .find("= \"\"\"\n")
        .map(|i| start + i + "= \"\"\"\n".len())
        .expect("Lua prelude triple-quoted body");
    let body_end = source[body_start..]
        .find("\n\"\"\"")
        .map(|i| body_start + i)
        .expect("Lua prelude body terminator");
    source[body_start..body_end].to_string()
}

fn run_stub_probe(probe: &str) -> (bool, String, String) {
    let luajit = rock_host::project::luajit_path();
    if !luajit.exists() {
        return (true, "SKIP: no luajit".to_string(), String::new());
    }

    let mut script = String::from(
        r#"
local ffi = require("ffi")
ffi.load = function(name, ...) error("simulated: cannot load " .. tostring(name)) end
local function __imgui_prelude_init(cdef, master, enums, wrap, init)
"#,
    );
    script.push_str(&lua_body());
    script.push_str("\nend\n__imgui_prelude_init(");
    let args: Vec<String> = cimgui_files()
        .iter()
        .map(|(file, _)| lua_quote(&cimgui_file(file)))
        .collect();
    script.push_str(&args.join(", "));
    script.push_str(")\n");
    script.push_str(probe);

    let dir = tempfile::tempdir().expect("probe tempdir");
    let path = dir.path().join("cimgui_stub.lua");
    std::fs::write(&path, script).expect("write cimgui stub probe");
    let output = Command::new(luajit)
        .arg(&path)
        .output()
        .expect("spawn luajit");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn no_long_bracket_collision() {
    let source = prelude_source();
    for (file, _) in cimgui_files() {
        let contents = cimgui_file(file);
        assert!(!contents.contains("]==]"), "{file} contains ]==]");
    }
    assert!(!source.contains("]==]"));
    assert!(!lua_body().contains("[==["));
}

#[test]
fn prelude_contains_all_modules() {
    let source = prelude_source();
    let body = lua_body();
    for (_, module) in cimgui_files().iter().take(4) {
        assert!(
            body.contains(&format!("package.preload[\"{module}\"]")),
            "missing {module}"
        );
    }
    for (file, _) in cimgui_files() {
        assert!(
            source.contains(&format!("embed_string(\"prebuilt/cimgui/{file}\")")),
            "missing embed {file}"
        );
    }
    assert!(body.contains("_G.imgui = _result"));
    assert!(body.contains("_G.imgui = setmetatable"));
}

#[test]
fn prelude_pins_cimgui_dylib() {
    let body = lua_body();
    let pin_at = body.find("RTLD_NODELETE").expect("NODELETE pin");
    let success_at = body.find("_G.imgui = _result").expect("success assignment");
    assert!(pin_at > success_at, "pin must run after init succeeds");
    assert!(body.contains("libcimgui_complete.so"));
    assert!(body.contains("libcimgui_complete.dylib"));
}

#[test]
fn every_pcall_in_prelude_has_a_stderr_writer() {
    let body = lua_body();
    let calls = body.matches("pcall(").count();
    assert!(calls > 0, "prelude has no pcall/xpcall site");
    assert!(
        body.contains("io.stderr:write"),
        "pcall failure has no stderr fence"
    );
    assert!(body.contains("[cimgui prelude] init failed"));
}

#[test]
fn every_imgui_binding_the_engine_calls_exists() {
    let root = repo_root();
    let mut exported = HashSet::from(["C".to_string()]);
    for (file, _) in cimgui_files().iter().take(4) {
        for line in cimgui_file(file).lines() {
            let text = line.trim_start();
            let Some(rest) = text.strip_prefix("M.") else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                exported.insert(name);
            }
        }
    }
    assert!(exported.contains("IsWindowFocused"));

    let mut sources = Vec::new();
    collect_rock_files(&root.join("stone"), &mut sources);
    assert!(
        sources.len() > 50,
        "stone source census is degenerate: {}",
        sources.len()
    );

    let mut missing = Vec::new();
    for path in &sources {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (line_no, line) in text.lines().enumerate() {
            let bytes = line.as_bytes();
            for (offset, _) in line.match_indices("imgui.") {
                let start = offset + "imgui.".len();
                let name: String = line[start..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if name.is_empty() {
                    continue;
                }
                let after = bytes.get(start + name.len()).copied();
                let before = offset.checked_sub(1).and_then(|i| bytes.get(i).copied());
                let use_site = matches!(after, Some(b'(') | Some(b'.'))
                    || matches!(before, Some(b'(') | Some(b','));
                if use_site && !exported.contains(&name) {
                    missing.push(format!(
                        "{}:{}: imgui.{name}",
                        path.strip_prefix(&root).unwrap_or(path).display(),
                        line_no + 1
                    ));
                }
            }
        }
    }
    missing.sort();
    assert!(
        missing.is_empty(),
        "missing cimgui exports:\n{}",
        missing.join("\n")
    );
}

#[test]
fn embedded_files_nonempty() {
    let expected = [
        ("cdef.lua", 247_098, 5_192),
        ("master.lua", 42, 3),
        ("enums.lua", 37_179, 1_033),
        ("wrap.lua", 266_706, 8_326),
        ("init.lua", 1_195, 43),
    ];
    for (file, bytes, lines) in expected {
        let contents = cimgui_file(file);
        assert!(!contents.is_empty(), "{file} empty");
        assert_eq!(contents.len(), bytes, "{file} byte accounting changed");
        assert_eq!(
            contents.bytes().filter(|&byte| byte == b'\n').count(),
            lines,
            "{file} line accounting changed"
        );
    }
    let body = lua_body();
    assert_eq!(
        body.len(),
        3_498,
        "delicate Lua body byte accounting changed"
    );
    assert_eq!(
        body.bytes().filter(|&byte| byte == b'\n').count(),
        61,
        "delicate Lua body line accounting changed"
    );
}

#[test]
fn stub_installs_when_ffi_load_fails() {
    let (ok, stdout, stderr) = run_stub_probe(
        r#"
assert(_G.imgui ~= nil, "imgui stub not installed")
assert(_G.imgui.Begin() == true)
assert(_G.imgui.Button() == false)
assert(_G.imgui.SliderFloat() == false)
local mp = _G.imgui.GetMousePos()
assert(type(mp) == "table" and mp.x == 0 and mp.y == 0)
local v = _G.imgui.ImVec2_Float(3, 4)
assert(v.x == 3 and v.y == 4)
assert(_G.imgui.SomeMethodThatDoesNotExist("a", "b") == nil)
_G.imgui.Render()
io.write("STUB_OK\n")
"#,
    );
    if stdout.starts_with("SKIP:") {
        return;
    }
    assert!(ok, "stub probe failed\nstdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("STUB_OK"),
        "stub marker missing\nstderr: {stderr}"
    );
    assert!(stderr.contains("[cimgui prelude] init failed"));
}

#[test]
fn stub_supports_c_namespace_and_io() {
    let (ok, stdout, stderr) = run_stub_probe(
        r#"
local io_ = _G.imgui.C.igGetIO_Nil()
assert(type(io_) == "table")
assert(io_.WantTextInput == false)
assert(io_.WantCaptureMouse == false)
assert(io_.KeyShift == false)
io_.ConfigFlags = bit.bor(io_.ConfigFlags, 128)
assert(_G.imgui.C.igGetIO_Nil().ConfigFlags == 128)
_G.imgui.C.ImGuiIO_AddMousePosEvent(io_, 1, 2)
assert(_G.imgui.C.ImGuiDockNode_IsEmpty(nil) == nil)
io.write("C_STUB_OK\n")
"#,
    );
    if stdout.starts_with("SKIP:") {
        return;
    }
    assert!(
        ok,
        "C stub probe failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("C_STUB_OK"),
        "C stub marker missing\nstderr: {stderr}"
    );
}

#[test]
fn global_initializer_observes_imgui_before_game_code() {
    let root = repo_root();
    let out = tempfile::tempdir().expect("initializer output dir");
    let main = out.path().join("main.lua");
    let incremental = out.path().join("incremental.lua");
    let _stone = rock_cli::skull_backend::SkullStone::init_with_tests(
        &root.join("games/testbed").to_string_lossy(),
        &main,
        &incremental,
    )
    .expect("testbed with the cimgui global-init fixture must compile");
    let emitted = std::fs::read_to_string(&main).expect("read emitted testbed");
    let ready = emitted
        .find("__skull_ginit___IMGUI_PRELUDE_READY())")
        .or_else(|| emitted.find("then __IMGUI_PRELUDE_READY = __skull_ginit___IMGUI_PRELUDE_READY()"))
        .expect("Rock cimgui initializer must be emitted");
    let game_init = emitted
        .find("__skull_ginit_CIMG_UI_GLOBAL_INIT_VERSION())")
        .or_else(|| emitted.find("then CIMG_UI_GLOBAL_INIT_VERSION = __skull_ginit_CIMG_UI_GLOBAL_INIT_VERSION()"))
        .expect("fixture global initializer must be emitted");
    assert!(
        ready < game_init,
        "imgui must initialize before a game global initializer"
    );

    let result = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(&root)
        .args([
            "sim",
            "games/testbed",
            "cimgui_global_init_order",
            "--jobs",
            "1",
        ])
        .env("STONE_HEADLESS", "1")
        .output()
        .expect("run cimgui global-init sim");
    let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&result.stderr));
    assert!(result.status.success(), "global-init sim failed:\n{text}");
    assert!(
        text.contains("cimgui_global_init_order"),
        "sim result missing:\n{text}"
    );
}

fn collect_rock_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rock_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rock") {
            out.push(path);
        }
    }
}
