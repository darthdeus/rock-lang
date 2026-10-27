//! Lambda static locals: a `static` local inside a
//! capturing lambda body is legal (rejected only in `co fn`/`@pure`), and its
//! cell already folds into the ENCLOSING fn's chunk statics -- so it carries the
//! standard hot-reload reset-on-init-edit / preserve-otherwise behavior. This
//! pins that behavior BEFORE P2 step 6 hoists lambda bodies to top-level fns:
//! the static is a module global either way, so hoisting must not change it, and
//! this test goes red if it ever does.
use crate::common::{emit_module, run_luajit};

/// A lambda (stored in a global so it escapes) whose body holds a `static n`.
/// Each call increments; the reload test distinguishes RESET (back to the
/// edited initializer) from PRESERVE (live value survives).
const PROBE: &str = r#"
global RESULT: int = 0
global THUNK: (() -> int)? = null

fn setup() {
    THUNK = () => {
        static n = 100
        n = n + 1
        return n
    }
}

fn call() -> int {
    let f = THUNK
    if f != null { return f() }
    return -1
}

fn main() { setup() RESULT = call() }
"#;

/// The static cell lives in the ENCLOSING fn's statics (named after `setup`,
/// carried in the reload reset preamble) even though it is declared inside the
/// lambda -- so P2 step 6 hoisting needs no static move.
#[test]
fn lambda_body_static_folds_into_enclosing_chunk_statics() {
    let lua = emit_module(PROBE);
    assert!(
        lua.contains("[\"static:named:<test>::setup"),
        "the lambda's `static n` must be stored under the enclosing fn `setup`"
    );
    // The reset preamble (generated from chunk statics) must guard it, so a reload
    // can nil it on an init-edit.
    assert!(
        lua.lines()
            .any(|l| l.contains("static:named:<test>::setup") && l.contains("__rock_static_prepare")),
        "the static must appear in the hot-reload reset preamble"
    );
}

/// RE-INIT on init-edit: editing the lambda static's initializer (100 -> 7)
/// changes its key, the reset preamble clears the cell, and the next call
/// re-inits -> 8 (not the preserved 102).
#[test]
fn lambda_body_static_reinits_on_initializer_edit() {
    let before = emit_module(PROBE);
    let after = emit_module(&PROBE.replace("static n = 100", "static n = 7"));
    let script = format!(
        "_G.HOT_RELOAD = false\n{before}\nmain()\n\
         _G.HOT_RELOAD = true\n{after}\nmain()\n\
         local result_key = __rock_global_entry(\"RESULT\").slots[1]\n\
         if __rock_globals[result_key] == 8 then print(\"PASS\")\n\
         else print(\"FAIL RESULT=\"..tostring(__rock_globals[result_key])) end\n"
    );
    let out = run_luajit(&script, "reinit");
    assert!(out.starts_with("PASS"), "expected lambda static to reset on init-edit; got: {out}");
}

/// PRESERVE otherwise: reload the SAME source, key unchanged -> the live value
/// survives, initializer does NOT re-run. First main -> 101, second call would
/// be 102 (n preserved), NOT 101 (a reset would give 101).
#[test]
fn lambda_body_static_preserved_across_identical_reload() {
    let prog = emit_module(PROBE);
    let script = format!(
        "_G.HOT_RELOAD = false\n{prog}\nmain()\n\
         _G.HOT_RELOAD = true\n{prog}\nmain()\n\
         local result_key = __rock_global_entry(\"RESULT\").slots[1]\n\
         if __rock_globals[result_key] == 102 then print(\"PASS\")\n\
         else print(\"FAIL RESULT=\"..tostring(__rock_globals[result_key])) end\n"
    );
    let out = run_luajit(&script, "preserve");
    assert!(out.starts_with("PASS"), "expected lambda static preserved across identical reload; got: {out}");
}
