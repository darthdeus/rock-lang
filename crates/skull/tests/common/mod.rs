//! Shared skull codegen-test harness: parse + semantic_pass, then either a full
//! MIR emit against the real stdlib (`emit_mir` / `emit_module`) or a bare
//! single-module workspace (`ws_of`). Pulled into each `tests/*.rs` via
//! `mod common;`.

#![allow(dead_code)] // every test file includes this module; none uses all of it.

use bone_semantic::inference::Workspace;
use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// Panic if bone recorded any type/lowering/interface error. `semantic_pass`
/// returns `Ok` with per-body diagnostics recorded on the workspace, so a
/// fixture that stops typechecking (e.g. its helper name goes ambiguous
/// against a new stdlib overload) would otherwise flow on with missing
/// resolutions and turn shape assertions vacuously green.
fn assert_no_bone_errors(ws: &Workspace) {
    let errs: Vec<String> = ws
        .inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten())
        .chain(ws.modules.iter().flat_map(|m| m.lowering_errors.values().flatten()))
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .map(|e| e.message.clone())
        .collect();
    assert!(errs.is_empty(), "bone recorded errors: {errs:#?}");
}

/// The messages bone recorded for `src`, without emitting.
///
/// [`try_emit_module`] deliberately PANICS on these (a bone error there means a
/// malformed fixture), so a test whose subject IS a bone-level rejection --
/// annotation-registry violations, for one -- cannot use it.
pub fn bone_rejections(src: &str) -> Vec<String> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    // BOTH channels. `semantic_pass` returns `Err` for a violation it treats as
    // fatal (a struct-field annotation is one) and RECORDS the rest on the
    // workspace; reading only the recorded set turns the fatal ones into a
    // panic inside the helper rather than a rejection the caller can assert.
    let ws = match bone_semantic::semantic_passes::semantic_pass(&mut modules) {
        Ok(ws) => ws,
        Err(e) => return vec![e.message],
    };
    ws.inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten())
        .chain(ws.modules.iter().flat_map(|m| m.lowering_errors.values().flatten()))
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .map(|e| e.message.clone())
        .collect()
}

/// Full MIR emit for `src` (the `<test>` module) through the skull pipeline
/// (lower -> inline -> cleanup -> emit). Stdlib is loaded so int/float casts and
/// `%`/arithmetic ops resolve to their stdlib helpers -- else the using fns fail
/// to lower and are silently skipped.
pub fn emit_mir(src: &str) -> skull::mir_emit::MirEmit {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit")
}

/// A workspace of `src` (the `<test>` module) over the real stdlib -- for tests
/// that inspect stdlib declarations (e.g. `@intrinsic` annotations).
pub fn ws_with_stdlib(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// Emitted Lua for `src` (the common case; `emit_mir(src).lua`).
pub fn emit_module(src: &str) -> String {
    emit_mir(src).lua
}

/// Like [`emit_mir`] but surfaces the codegen `Result` (a rejection such as a
/// reserved synthetic-name collision) instead of `.expect`ing success. Semantic
/// errors still panic (a malformed test, not what these cases probe).
pub fn try_emit_module(src: &str) -> Result<String, skull_parser::CompilerError> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).map(|e| e.lua)
}

/// The body of `function <name>(...) ... end` in emitted module Lua. `name` is
/// the rock identifier; the emitted definition is mangled
/// (`hot_int_int_int_ret_float`), so match the `function <name>` prefix. The
/// first top-level `\nend\n` closes the function (nested blocks are indented
/// `end`s, never column-0). A hoisted def (`<name>__lambda0`) shares the
/// `function <name>` prefix, but the enclosing `<name>` is emitted first, so
/// `.find` returns the enclosing body -- not the hoist.
pub fn function_body<'a>(lua: &'a str, name: &str) -> &'a str {
    let head = format!("function {name}");
    let start = lua.find(&head).unwrap_or_else(|| panic!("no `{head}` in:\n{lua}"));
    let rest = &lua[start..];
    let end = rest.find("\nend\n").map(|e| e + 5).unwrap_or(rest.len());
    &rest[..end]
}

/// The body of the single emitted function whose name contains `needle`.
/// Synthetic callables carry a generated suffix (`<mangled>__lambda0`) or a
/// hash (`__local_go_<hash>_inner_0`), so they are found by pattern rather
/// than spelled out. Panics if it is not unique -- an ambiguous match would
/// silently assert against the wrong body.
pub fn body_of_synthetic<'a>(lua: &'a str, needle: &str) -> &'a str {
    let names: Vec<&str> = lua
        .match_indices("\nfunction ")
        .map(|(i, m)| {
            let rest = &lua[i + m.len()..];
            &rest[..rest.find('(').expect("fn header has `(`")]
        })
        .filter(|n| n.contains(needle))
        .collect();
    assert_eq!(names.len(), 1, "expected exactly one fn containing `{needle}`, got {names:?}");
    function_body(lua, names[0])
}

/// The full emitted (mangled) name of the top-level fn whose SOURCE name is
/// `src`, read out of emitted `lua`. Every non-exempt fn mangles by signature
/// (`free_loop` -> `free_loop_ret_unit`), so this recovers the base symbol from
/// which a test builds the synthetic names it owns (`<mangled>__lambda<k>`,
/// `<mangled>__step`, ...). The base symbol carries NO `__` (that prefix only
/// appears on the synthetic derivations, and for a `co fn` a `__step`/`__resume`
/// header can emit before the constructor), so skip `__`-bearing candidates.
pub fn mangled_name(lua: &str, src: &str) -> String {
    let head = format!("function {src}");
    let mut from = 0;
    while let Some(p) = lua[from..].find(&head) {
        let start = from + p + "function ".len();
        let rest = &lua[start..];
        let end = rest.find('(').expect("fn header has `(`");
        let name = &rest[..end];
        if !name.contains("__") {
            return name.to_string();
        }
        from = start + end;
    }
    panic!("no non-synthetic `function {src}` in:\n{lua}");
}

/// Like [`try_emit_module`] but ALSO loads `lib` as an extra stdlib module. The
/// emit path is deliberately lenient for stdlib bodies (a codegen parity gap
/// there becomes a silent `skipped`), so a case probing that leniency needs a
/// body the module-file check actually classifies as stdlib.
pub fn try_emit_with_stdlib_module(
    lib: &str,
    src: &str,
) -> Result<String, skull_parser::CompilerError> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(lib, Ustr::from("<stdlib:probe>")).expect("parse lib"));
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).map(|e| e.lua)
}

/// Single-module workspace (no stdlib) for incremental / hot-reload tests.
pub fn ws_of(src: &str) -> Workspace {
    let module = skull_parser::parse_module_clean(src, Ustr::from("<t>")).expect("parse");
    bone_semantic::semantic_passes::semantic_pass(&mut [module]).expect("semantic_pass")
}

/// Run `prog` under luajit, returning trimmed stdout (panicking with stderr on a
/// non-zero exit). `tag` disambiguates the temp file so parallel tests never
/// collide (nextest is process-per-test, but be safe under `cargo test` threads).
pub fn run_luajit(prog: &str, tag: &str) -> String {
    use std::process::Command;
    let path = std::env::temp_dir().join(format!("skull_test_{}_{tag}.lua", std::process::id()));
    std::fs::write(&path, prog).expect("write program");
    let out = Command::new("luajit").arg(&path).output().expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "luajit exited non-zero.\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout.trim().to_string()
}

/// THE lock for the Stage-0e ablation mode, which is a process-global flag
/// (`skull_mir::ablation`).
///
/// One lock for the whole test binary. Every `tests/*.rs` here is a `#[path]`
/// module of `main.rs`, so a `static MODE` declared per file locks a DIFFERENT
/// mutex -- two such files serialize against themselves and not against each
/// other, which under `cargo test`'s threads is exactly no serialization at
/// all. The flag then flips under a running test and one fixture's fatals land
/// in another's census.
static ABLATION_MODE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold the ablation mode for one test. `Drop` restores it even on a panicking
/// assert, so a failing test cannot leave the flag on for the next one.
pub struct AblationGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl AblationGuard {
    pub fn on() -> Self {
        Self::set(true)
    }

    pub fn off() -> Self {
        Self::set(false)
    }

    fn set(on: bool) -> Self {
        // `into_inner` on poison: a test that panicked while holding this left
        // the flag restored by `Drop` regardless, so the lock is still usable
        // and refusing here would cascade one failure into every sibling.
        let g = ABLATION_MODE.lock().unwrap_or_else(|e| e.into_inner());
        skull_mir::ablation::clear();
        skull_mir::ablation::set_enabled(on);
        AblationGuard(g)
    }
}

impl Drop for AblationGuard {
    fn drop(&mut self) {
        skull_mir::ablation::set_enabled(false);
        skull_mir::ablation::clear();
    }
}
