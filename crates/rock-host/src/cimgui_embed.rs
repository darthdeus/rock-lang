// Embedded cimgui Lua bindings.
//
// The .lua files in prebuilt/lua/cimgui/ are platform-independent text and
// version-controlled. Embedding them via include_str! and emitting a prelude
// at the top of every game's main.lua eliminates filesystem lookup, the
// previous package.path patching in stone/engine/lib.rock, and the
// per-output-dir cimgui/ sync that leaked into game source trees.

pub struct CimguiFile {
    pub module: &'static str,
    pub contents: &'static str,
}

// Order matters: init.lua require()s the others, but as package.preload
// entries they're lazily loaded, so listing order is purely cosmetic.
pub const CIMGUI_FILES: &[CimguiFile] = &[
    CimguiFile { module: "cimgui.cdef",      contents: include_str!("../../../prebuilt/lua/cimgui/cdef.lua") },
    CimguiFile { module: "cimgui.master",    contents: include_str!("../../../prebuilt/lua/cimgui/master.lua") },
    CimguiFile { module: "cimgui.enums",     contents: include_str!("../../../prebuilt/lua/cimgui/enums.lua") },
    CimguiFile { module: "cimgui.wrap",      contents: include_str!("../../../prebuilt/lua/cimgui/wrap.lua") },
];

pub const CIMGUI_INIT: &str = include_str!("../../../prebuilt/lua/cimgui/init.lua");

/// Build the Lua prelude that registers cimgui submodules in package.preload,
/// then runs init.lua under pcall and assigns the result (or stub) to _G.imgui.
///
/// The prelude is self-contained and idempotent: re-running it is a no-op
/// because init.lua's first guard is `if _G.imgui then return end` — but we
/// also gate the whole block on `_G.imgui == nil` to avoid the pcall cost on
/// hot-reload re-entry.
pub fn build_prelude() -> String {
    let mut out = String::with_capacity(300_000);
    out.push_str("-- cimgui bindings (embedded by rock; see rock/src/cimgui_embed.rs)\n");
    out.push_str("if _G.imgui == nil then\n");

    for file in CIMGUI_FILES {
        // Long-bracket [==[ ... ]==] avoids escaping issues. The Lua sources
        // don't contain `]==]` (verified at compile time below).
        out.push_str(&format!(
            "package.preload[\"{}\"] = assert(loadstring([==[\n{}\n]==], \"{}\"))\n",
            file.module, file.contents, file.module
        ));
    }

    // Run init.lua inline. Pass "cimgui" as the chunk arg so its
    // `(...):gsub(".init$","")` produces the "cimgui." prefix used for
    // internal requires.
    out.push_str("local _ok, _result = xpcall(function(...)\n");
    out.push_str(CIMGUI_INIT);
    out.push_str("\nend, debug.traceback, \"cimgui\")\n");
    out.push_str("if _ok then\n");
    out.push_str("    _G.imgui = _result\n");
    out.push_str(PIN_DYLIB);
    out.push_str("else\n");
    out.push_str("    io.stderr:write(\"[cimgui prelude] init failed, installing stub. Error:\\n\" .. tostring(_result) .. \"\\n\")\n");
    out.push_str(STUB_TABLE);
    out.push_str("end\n");
    out.push_str("end\n");
    out
}

// Pin libcimgui_complete for process lifetime. Its GL loader
// (cimgui's vendored imgui_impl_opengl3_loader.h, imgl3wInit) registers
// atexit(close_libgl), which assumes the library lives until exit().
// LuaJIT's clib GC dlcloses every ffi.load'ed library at lua_close, so
// without the pin exit() jumps through a stale handler into unmapped
// memory — SIGSEGV in __run_exit_handlers after every headless run
// (sims passed all expects, then died with status -1).
// dlopen(RTLD_NOLOAD|RTLD_NODELETE) promotes the already-loaded
// library to never-unload without loading anything new; LuaJIT's later
// dlclose then only drops a refcount. Windows needs no pin: ffi.load
// uses LoadLibrary refcounts and the loader's FreeLibrary handler can't
// dangle the same way.
const PIN_DYLIB: &str = r#"    do
        local ffi = require("ffi")
        if ffi.os == "Linux" or ffi.os == "OSX" then
            -- pcall: dlopen may already be cdef'd by another binding;
            -- redefinition throws but ffi.C.dlopen works either way.
            pcall(ffi.cdef, "void *dlopen(const char *filename, int flags);")
            local RTLD_NOW, RTLD_NOLOAD, RTLD_NODELETE, name
            if ffi.os == "OSX" then
                RTLD_NOW, RTLD_NOLOAD, RTLD_NODELETE = 2, 0x10, 0x80
                name = "libcimgui_complete.dylib"
            else
                RTLD_NOW, RTLD_NOLOAD, RTLD_NODELETE = 2, 4, 0x1000
                name = "libcimgui_complete.so"
            end
            -- NULL result (lib not loaded under this name) is fine: there
            -- is nothing to pin, and nothing registered an atexit handler.
            ffi.C.dlopen(name, RTLD_NOW + RTLD_NOLOAD + RTLD_NODELETE)
        end
    end
"#;

// Stub installed when cimgui_complete fails to load (missing C library).
// Mirrors the table previously hand-written in stone/engine/lib.rock; methods
// not listed fall through to a no-op via __index.
const STUB_TABLE: &str = r#"    local _noop = function() end
    local _true = function() return true end
    local _false = function() return false end
    _G.imgui = setmetatable({
        Begin = _true, BeginChild_Str = _true,
        Button = _false, TreeNode_Str = _false,
        InputText = _false, SliderFloat = _false, SliderFloat2 = _false, SliderFloat4 = _false,
        ColorEdit4 = _false, ColorPicker4 = _false, ImageButton = _false,
        IsWindowHovered = _false, IsAnyItemHovered = _false,
        GetMousePos = function() return {x=0, y=0} end,
        ImVec2_Float = function(x, y) return {x=x or 0, y=y or 0} end,
        ImVec4_Float = function(r, g, b, a) return {x=r or 0, y=g or 0, z=b or 0, w=a or 0} end,
        ImGuiHoveredFlags_AnyWindow = 0,
    }, {__index = function() return _noop end})
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_long_bracket_collision() {
        // Long-bracket level [==[ ... ]==] requires no `]==]` in body.
        for f in CIMGUI_FILES {
            assert!(!f.contents.contains("]==]"), "{} contains ]==] — bump bracket level", f.module);
        }
        assert!(!CIMGUI_INIT.contains("]==]"));
    }

    #[test]
    fn prelude_contains_all_modules() {
        let p = build_prelude();
        for f in CIMGUI_FILES {
            assert!(p.contains(&format!("package.preload[\"{}\"]", f.module)), "missing {}", f.module);
        }
        assert!(p.contains("_G.imgui = _result"));
        assert!(p.contains("_G.imgui = setmetatable"));
    }

    /// The prelude must pin libcimgui_complete with RTLD_NODELETE on the
    /// success path. The lib's GL loader registers atexit(close_libgl);
    /// if LuaJIT's clib GC unmaps the lib at lua_close, every process
    /// exit segfaults in __run_exit_handlers (all sims red with
    /// status -1). See PIN_DYLIB.
    #[test]
    fn prelude_pins_cimgui_dylib() {
        let p = build_prelude();
        assert!(p.contains("RTLD_NODELETE"), "prelude lost the NODELETE pin");
        let pin_at = p.find("RTLD_NODELETE").unwrap();
        let success_at = p.find("_G.imgui = _result").unwrap();
        assert!(
            pin_at > success_at,
            "pin must run after init succeeds (it pins by the load-time name)"
        );
    }

    /// Regression fence for the cimgui_embed silent-error incident
    /// (commit `2bce075d`, 2026-05-02): every `pcall`/`xpcall` we
    /// emit into the cimgui prelude MUST be paired with an
    /// `io.stderr:write` (or `error(...)`) on its failure branch.
    /// The pre-fix prelude used a bare `pcall(...)` and silently
    /// installed a no-op imgui stub on dlopen failure — every
    /// editor/debug-overlay window was invisible on macOS dev mode
    /// for >6 months with no log line.
    ///
    /// This is a textual fence rather than a parser-aware lint
    /// because the prelude generator is a single file with a fixed
    /// shape; richer analysis would be over-engineering for one
    /// call site. If a future generator adds another `pcall`/
    /// `xpcall`, extend the assertion to scan that output too.
    /// Silent-errors audit, 2026-05-03 (deferred item 4).
    #[test]
    fn every_pcall_in_prelude_has_a_stderr_writer() {
        let p = build_prelude();
        let calls = p.matches("pcall(").count(); // also catches xpcall(
        assert!(
            calls > 0,
            "prelude has no pcall/xpcall — test premise broken; \
             update if the prelude no longer wraps init.lua"
        );
        // Each pcall/xpcall site in this prelude must surface its
        // error. Today there's exactly one xpcall wrapping the
        // init.lua chunk; the failure branch writes to stderr
        // before installing the stub. A regression that drops the
        // stderr write turns this red.
        assert!(
            p.contains("io.stderr:write"),
            "cimgui prelude has {} pcall/xpcall site(s) but no \
             `io.stderr:write` — failure branch is silently\n\
             swallowing errors. See silent-errors-audit.md C0/C2 \
             and the cimgui_embed incident in 2bce075d.",
            calls
        );
        // And the stderr writer must come from the prelude itself,
        // not the embedded init.lua content. Sanity-check by
        // confirming the literal string from build_prelude's else
        // branch is present (this is the line that the 2bce075d
        // fix introduced).
        assert!(
            p.contains("[cimgui prelude] init failed"),
            "cimgui prelude lost its `[cimgui prelude] init failed` \
             stderr message — the fix from 2bce075d has been \
             reverted or the message changed. Update this test \
             alongside the prelude generator if the wording moved \
             on purpose."
        );
    }

    /// Every `imgui.<Name>` the engine calls must exist in the cimgui export
    /// table. cimgui suffixes overloaded functions (`IsMouseDown_Nil`,
    /// `BeginChild_Str`, `ImVec2_Float`), so the bare ImGui name a binding is
    /// written from is often not what the table holds — and the real table has
    /// no `__index` fallback, so the miss is a nil call at the first frame that
    /// reaches it, not at load. `imgui_is_mouse_down` shipped this way and
    /// crashed only once a pointer actually dragged a debug window, which no
    /// headless sim can do.
    ///
    /// Occurrence rule: `imgui.Name` counts when it is immediately followed by
    /// `(` or `.` (a call, or a reach through `imgui.C`), or immediately
    /// preceded by `(` or `,` (an enum passed as an argument). That skips the
    /// prose and filename uses (`imgui.ini`, `"imgui.layout-version"`).
    #[test]
    fn every_imgui_binding_the_engine_calls_exists() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

        let mut exported: std::collections::HashSet<String> = std::collections::HashSet::new();
        // `M.C = lib` is assigned in init.lua, not the generated tables.
        exported.insert("C".to_string());
        for f in CIMGUI_FILES {
            for line in f.contents.lines() {
                let t = line.trim_start();
                let Some(rest) = t.strip_prefix("M.") else { continue };
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    exported.insert(name);
                }
            }
        }
        assert!(
            exported.contains("IsWindowFocused"),
            "export scrape found {} names but not IsWindowFocused — \
             the generated table's shape changed, fix the scrape",
            exported.len()
        );

        let mut sources = Vec::new();
        collect_rock_files(&root.join("stone"), &mut sources);
        assert!(
            sources.len() > 50,
            "expected to walk stone's .rock tree, found {} files",
            sources.len()
        );

        let mut missing: Vec<String> = Vec::new();
        for path in &sources {
            let Ok(text) = std::fs::read_to_string(path) else { continue };
            for (lineno, line) in text.lines().enumerate() {
                let b = line.as_bytes();
                for (i, _) in line.match_indices("imgui.") {
                    let start = i + "imgui.".len();
                    let name: String = line[start..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if name.is_empty() {
                        continue;
                    }
                    let after = b.get(start + name.len()).copied();
                    let before = if i == 0 { None } else { b.get(i - 1).copied() };
                    let is_use = matches!(after, Some(b'(') | Some(b'.'))
                        || matches!(before, Some(b'(') | Some(b','));
                    if !is_use || exported.contains(&name) {
                        continue;
                    }
                    missing.push(format!(
                        "{}:{}: imgui.{}",
                        path.strip_prefix(&root).unwrap_or(path).display(),
                        lineno + 1,
                        name
                    ));
                }
            }
        }
        missing.sort();
        assert!(
            missing.is_empty(),
            "these names are not in the cimgui export table (check for an \
             overload suffix such as _Nil / _Str / _Float in \
             prebuilt/lua/cimgui/wrap.lua):\n  {}",
            missing.join("\n  ")
        );
    }

    fn collect_rock_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_rock_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rock") {
                out.push(p);
            }
        }
    }

    #[test]
    fn embedded_files_nonempty() {
        for f in CIMGUI_FILES {
            assert!(!f.contents.is_empty(), "{} empty", f.module);
        }
        assert!(!CIMGUI_INIT.is_empty());
    }

    /// End-to-end test for the libcimgui-missing fallback path:
    /// monkey-patch ffi.load to always fail, run the prelude under luajit,
    /// and verify the stub _G.imgui is installed and behaves correctly.
    #[test]
    fn stub_installs_when_ffi_load_fails() {
        let luajit = match find_luajit() {
            Some(p) => p,
            None => {
                eprintln!("skipping: no luajit on PATH or in prebuilt/");
                return;
            }
        };

        let mut script = String::new();
        // Force ffi.load to fail unconditionally — simulates libcimgui_complete missing.
        script.push_str(r#"
local ffi = require("ffi")
local _orig_load = ffi.load
ffi.load = function(name, ...) error("simulated: cannot load " .. tostring(name)) end
"#);
        script.push_str(&build_prelude());
        // Verify stub shape and behavior.
        script.push_str(r#"
assert(_G.imgui ~= nil, "imgui stub not installed")
assert(_G.imgui.Begin() == true, "stub Begin should return true")
assert(_G.imgui.Button() == false, "stub Button should return false")
assert(_G.imgui.SliderFloat() == false, "stub SliderFloat should return false")
local mp = _G.imgui.GetMousePos()
assert(type(mp) == "table" and mp.x == 0 and mp.y == 0, "stub GetMousePos shape wrong")
local v = _G.imgui.ImVec2_Float(3, 4)
assert(v.x == 3 and v.y == 4, "stub ImVec2_Float shape wrong")
-- __index fallback: any unknown method returns a no-op callable
local r = _G.imgui.SomeMethodThatDoesNotExist("a", "b")
assert(r == nil, "unknown method should be no-op (returned " .. tostring(r) .. ")")
-- And calling indexed-but-non-method shouldn't error either:
_G.imgui.Render()
io.write("STUB_OK\n")
"#);

        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("stub_test.lua");
        std::fs::write(&script_path, &script).expect("write script");

        let out = std::process::Command::new(&luajit)
            .arg(&script_path)
            .output()
            .expect("spawn luajit");

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "luajit exited non-zero\nstdout: {}\nstderr: {}",
            stdout,
            stderr
        );
        assert!(
            stdout.contains("STUB_OK"),
            "missing STUB_OK marker\nstdout: {}\nstderr: {}",
            stdout,
            stderr
        );
    }

    fn find_luajit() -> Option<std::path::PathBuf> {
        let p = crate::project::luajit_path();
        if p.exists() { Some(p) } else { None }
    }
}
