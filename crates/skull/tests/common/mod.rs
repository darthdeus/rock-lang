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

/// Full MIR emit for `src` (the `<test>` module) through the skull pipeline
/// (lower -> inline -> cleanup -> emit). Stdlib is loaded so int/float casts and
/// `%`/arithmetic ops resolve to their stdlib helpers -- else the using fns fail
/// to lower and are silently skipped.
pub fn emit_mir(src: &str) -> skull::mir_emit::MirEmit {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit")
}

/// Emitted Lua for `src` (the common case; `emit_mir(src).lua`).
pub fn emit_module(src: &str) -> String {
    emit_mir(src).lua
}

/// The body of `function <name>(...) ... end` in emitted module Lua. `name` is
/// the rock identifier; the emitted definition is mangled
/// (`hot__int_int_int_ret_float`), so match the `function <name>` prefix. The
/// first top-level `\nend\n` closes the function (nested blocks are indented
/// `end`s, never column-0).
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
