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
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
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

/// Full MIR emit for `src` (the `<test>` module) through the skull pipeline
/// (lower -> inline -> cleanup -> emit). Stdlib is loaded so int/float casts and
/// `%`/arithmetic ops resolve to their stdlib helpers -- else the using fns fail
/// to lower and are silently skipped.
pub fn emit_mir(src: &str) -> skull::mir_emit::MirEmit {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit")
}

/// A workspace of `src` (the `<test>` module) over the real stdlib -- for tests
/// that inspect stdlib declarations (e.g. `@intrinsic` annotations).
pub fn ws_with_stdlib(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
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
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
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

/// Single-module workspace (no stdlib) for incremental / hot-reload tests.
pub fn ws_of(src: &str) -> Workspace {
    let module = skull_parser::parse_module(src, Ustr::from("<t>")).expect("parse");
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
