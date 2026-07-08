//! `static` locals across a hot reload. The private cell + done-flag are plain
//! Lua globals, so they survive a re-`dofile`; the done-flag guard then skips
//! re-init and the value is PRESERVED. Editing the initializer changes the
//! `__rock_known_globals` key (type + init-hash), so the reset preamble nils the
//! cell + clears the done-flag and the next reach RE-INITS with the new value.
//! Design: docs/static-locals-plan.md, docs/static-locals-implementation.md.
//!
//! No arithmetic here on purpose: the no-stdlib emit harness can't resolve `+`
//! (it routes through a stdlib op fn), so the static is observed via a bool
//! toggle + assignment instead of a counter.

use bone_semantic::inference::Workspace;

fn ws_of(src: &str) -> Workspace {
    let module = skull_parser::parse_module(src, ustr::Ustr::from("<t>")).expect("parse");
    bone_semantic::semantic_passes::semantic_pass(&mut [module]).expect("semantic_pass")
}

fn emit_program(src: &str) -> String {
    let ws = ws_of(src);
    let mid = skull::user_module_id(&ws).unwrap();
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit").lua
}

fn run_luajit(prog: &str) -> String {
    use std::process::Command;
    let path = std::env::temp_dir().join(format!("skull_static_reload_{}.lua", std::process::id()));
    std::fs::write(&path, prog).expect("write program");
    let out = Command::new("luajit").arg(&path).output().expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "luajit exited non-zero.\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// `probe()` reads a static `n` (init 42); when the DO_SET global is set it
/// overwrites `n` to 99. So after a reload we can tell PRESERVE (still 99) from
/// RESET (back to the initializer).
const PROBE: &str = "\
global RESULT: int = 0
global DO_SET: bool = false
fn probe() -> int {
    static n = 42
    if DO_SET { n = 99 }
    return n
}
fn main() { RESULT = probe() }
";

/// PRESERVE: same source reloaded, key unchanged -> the live value (99, written
/// pre-reload) survives; the initializer does NOT re-run.
#[test]
fn static_survives_hot_reload_no_reinit() {
    let prog = emit_program(PROBE);
    let script = format!(
        "_G.HOT_RELOAD = false\n{prog}\n\
         DO_SET = true; main()   -- n inits 42, set -> 99, RESULT = 99\n\
         _G.HOT_RELOAD = true\n{prog}\n\
         DO_SET = false; main()  -- key unchanged: n preserved (99), RESULT = 99\n\
         if RESULT == 99 then print(\"PASS\")\n\
         else print(\"FAIL RESULT=\"..tostring(RESULT)); os.exit(1) end\n"
    );
    assert!(run_luajit(&script).trim().starts_with("PASS"), "expected preserved static");
}

/// RE-INIT: edit the initializer (42 -> 7) -> the key's init-hash changes, the
/// reset preamble clears the cell + done-flag, and the next reach re-inits.
#[test]
fn static_reinits_when_initializer_edited() {
    let before = emit_program(PROBE);
    let after = emit_program(&PROBE.replace("static n = 42", "static n = 7"));
    let script = format!(
        "_G.HOT_RELOAD = false\n{before}\n\
         DO_SET = false; main()  -- n inits 42, RESULT = 42\n\
         _G.HOT_RELOAD = true\n{after}\n\
         DO_SET = false; main()  -- init edited: n resets, re-inits 7, RESULT = 7\n\
         if RESULT == 7 then print(\"PASS\")\n\
         else print(\"FAIL RESULT=\"..tostring(RESULT)); os.exit(1) end\n"
    );
    assert!(run_luajit(&script).trim().starts_with("PASS"), "expected re-init on edit");
}

/// UNRELATED edit in the same body must NOT change the static's key (the hash
/// covers the initializer expr, not arena indices or sibling statements), so the
/// value is preserved across that reload.
#[test]
fn static_preserved_when_unrelated_body_edited() {
    let before = emit_program(PROBE);
    let edited = PROBE.replace("    return n\n", "    let unused = 0\n    return n\n");
    let after = emit_program(&edited);
    let script = format!(
        "_G.HOT_RELOAD = false\n{before}\n\
         DO_SET = true; main()   -- RESULT = 99\n\
         _G.HOT_RELOAD = true\n{after}\n\
         DO_SET = false; main()  -- key unchanged despite edit: n preserved, RESULT = 99\n\
         if RESULT == 99 then print(\"PASS\")\n\
         else print(\"FAIL RESULT=\"..tostring(RESULT)); os.exit(1) end\n"
    );
    assert!(run_luajit(&script).trim().starts_with("PASS"), "expected key stable under unrelated edit");
}
