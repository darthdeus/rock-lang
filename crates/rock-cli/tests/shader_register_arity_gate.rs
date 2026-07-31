//! The two `__rock_shader_register` drivers must agree with its DECLARATION.
//!
//! `rock_shader` registration lines are emitted twice by hand: skull's
//! `shader_registration_lines` (Lua, `skull/src/mir_emit.rs`) and the web
//! driver's `shader_register_lines_js` (`rock-cli/src/web.rs`). They are raw
//! driver text, not MIR, so nothing type-checks them against the engine fn they
//! call -- and on JS a short call is SILENT, because the missing argument reads
//! `undefined` rather than throwing. That is exactly how `@textures` adding a
//! third param left `rock web` dead for every game while the Lua side was fine
//! (`9d3cac70c`): the engine's `bind_texture_slots` died on `.len` frames later.
//!
//! This gate reads the arity off the real declaration in watcher.rock and holds
//! both drivers to it. It does NOT need a game with a `@textures` group -- an
//! empty slot list is still an argument. Slot CONTENT is pinned separately by
//! `web_build_gate::web_shader_registration_carries_texture_slots`.

use std::path::PathBuf;
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

/// Params in the engine's `__rock_shader_register` declaration.
fn declared_arity() -> usize {
    let src = std::fs::read_to_string(repo_root().join("stone/engine/debug/watcher.rock"))
        .expect("read watcher.rock");
    let at = src
        .find("fn __rock_shader_register(")
        .expect("watcher.rock declares __rock_shader_register");
    let open = src[at..].find('(').unwrap() + at + 1;
    count_args(&src[open..], Syntax::Rock).expect("declaration has a closing paren")
}

#[derive(Clone, Copy, PartialEq)]
enum Syntax {
    Rock,
    Lua,
    Js,
}

/// Top-level arguments in `s`, which starts just past an opening `(`. Returns
/// `None` if the paren never closes. Skips string literals (and Lua's `[[ ]]`
/// long brackets, which is how the GLSL source argument is emitted) so their
/// contents cannot be miscounted as separators.
fn count_args(s: &str, syn: Syntax) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut depth = 0usize;
    let mut args = 1usize;
    let mut seen_any = false;
    while i < b.len() {
        match b[i] {
            b'[' if syn == Syntax::Lua && b.get(i + 1) == Some(&b'[') => {
                let close = s[i..].find("]]")? + i;
                i = close + 2;
                seen_any = true;
                continue;
            }
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                seen_any = true;
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                seen_any = true;
            }
            b')' if depth == 0 => {
                // `f()` is zero args, not one.
                return Some(if seen_any { args } else { 0 });
            }
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                args += 1;
                seen_any = true;
            }
            c if !c.is_ascii_whitespace() => seen_any = true,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Arity of every driver-emitted registration call in `text`. Driver lines are
/// emitted at column 0; a `__rock_shader_register(...)` inside a function body
/// is indented and is not a driver line.
fn driver_call_arities(text: &str, syn: Syntax) -> Vec<usize> {
    let needle = "__rock_shader_register(";
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find(needle) {
        let at = from + rel;
        let col0 = at == 0 || text.as_bytes()[at - 1] == b'\n';
        from = at + needle.len();
        if !col0 {
            continue;
        }
        out.push(count_args(&text[from..], syn).expect("registration call closes"));
    }
    out
}

#[test]
fn both_shader_register_drivers_match_the_declared_arity() {
    let want = declared_arity();
    assert!(want >= 2, "declaration parse looks wrong: {want} params");

    let dir = tempfile::tempdir().expect("tempdir");

    let lua_out = dir.path().join("game.lua");
    let status = Command::new(rock_bin())
        .current_dir(repo_root())
        .args(["compile", "games/match3", "--output", lua_out.to_str().unwrap()])
        .status()
        .expect("spawn rock compile");
    assert!(status.success(), "rock compile failed");
    let lua = std::fs::read_to_string(&lua_out).unwrap();

    let web_dir = dir.path().join("web");
    let status = Command::new(rock_bin())
        .current_dir(repo_root())
        .args(["web", "games/match3", "--out", web_dir.to_str().unwrap()])
        .status()
        .expect("spawn rock web");
    assert!(status.success(), "rock web failed");
    let js = std::fs::read_to_string(web_dir.join("game.js")).unwrap();

    let lua_arities = driver_call_arities(&lua, Syntax::Lua);
    let js_arities = driver_call_arities(&js, Syntax::Js);

    // A driver that emits nothing would pass every arity assertion vacuously.
    assert!(!lua_arities.is_empty(), "no Lua registration lines -- gate is vacuous");
    assert!(!js_arities.is_empty(), "no JS registration lines -- gate is vacuous");
    assert_eq!(
        lua_arities.len(),
        js_arities.len(),
        "drivers disagree on how many pairs to register: lua {} vs js {}",
        lua_arities.len(),
        js_arities.len()
    );

    for (i, got) in lua_arities.iter().enumerate() {
        assert_eq!(*got, want, "Lua registration {i} passes {got} args, declaration takes {want}");
    }
    for (i, got) in js_arities.iter().enumerate() {
        assert_eq!(*got, want, "JS registration {i} passes {got} args, declaration takes {want}");
    }
}
