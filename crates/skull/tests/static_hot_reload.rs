//! `static` locals across a hot reload. The private value cell + done-flag live
//! in the global storage, so they survive a re-`dofile`; the done-flag guard then skips
//! re-init and the value is PRESERVED. Editing the initializer changes the
//! compiler's initializer identity, so the overlay clears both slots and the
//! next reach RE-INITS with the new value.
//!
//!
//! No arithmetic here on purpose: the no-stdlib emit harness can't resolve `+`
//! (it routes through a stdlib op fn), so the static is observed via a bool
//! toggle + assignment instead of a counter.

use crate::common::{body_id, run_luajit};

fn emit_program(src: &str) -> String {
    let ws = crate::common::ws_of(src);
    let mid = skull::user_module_id(&ws).unwrap();
    skull::mir_emit::emit_workspace_mir(&ws, mid)
        .expect("emit")
        .lua
}

/// Two in-process emits of the same string/int statics must be byte-identical.
/// A process-seeded Ustr hash in the init key made consecutive `rock compile`
/// runs reset live static cells on every hot reload.
const STRING_STATIC: &str = "\
global RESULT: string = \"\"
fn probe() -> string {
    static label = \"hello\"
    static n = -1
    return label
}
fn main() { RESULT = probe() }
";

#[test]
fn static_initializer_hash_is_reproducible_across_two_emits() {
    let a = emit_program(STRING_STATIC);
    let b = emit_program(STRING_STATIC);
    assert_eq!(
        a, b,
        "two in-process emits of the same static inits must be byte-identical"
    );
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
         local result_key = __rock_global_entry(\"RESULT\").slots[1]\n\
         local do_set_key = __rock_global_entry(\"DO_SET\").slots[1]\n\
         __rock_globals[do_set_key] = true; main()   -- n inits 42, set -> 99, RESULT = 99\n\
         _G.HOT_RELOAD = true\n{prog}\n\
         __rock_globals[do_set_key] = false; main()  -- key unchanged: n preserved (99), RESULT = 99\n\
         if __rock_globals[result_key] == 99 then print(\"PASS\")\n\
         else print(\"FAIL RESULT=\"..tostring(__rock_globals[result_key])); os.exit(1) end\n"
    );
    assert!(
        run_luajit(&script).trim().starts_with("PASS"),
        "expected preserved static"
    );
}

/// RE-INIT: edit the initializer (42 -> 7) -> the key's init-hash changes, the
/// reset preamble clears the cell + done-flag, and the next reach re-inits.
#[test]
fn static_reinits_when_initializer_edited() {
    let before = emit_program(PROBE);
    let after = emit_program(&PROBE.replace("static n = 42", "static n = 7"));
    let script = format!(
        "_G.HOT_RELOAD = false\n{before}\n\
         local result_key = __rock_global_entry(\"RESULT\").slots[1]\n\
         local do_set_key = __rock_global_entry(\"DO_SET\").slots[1]\n\
         __rock_globals[do_set_key] = false; main()  -- n inits 42, RESULT = 42\n\
         _G.HOT_RELOAD = true\n{after}\n\
         __rock_globals[do_set_key] = false; main()  -- init edited: n resets, re-inits 7, RESULT = 7\n\
         if __rock_globals[result_key] == 7 then print(\"PASS\")\n\
         else print(\"FAIL RESULT=\"..tostring(__rock_globals[result_key])); os.exit(1) end\n"
    );
    assert!(
        run_luajit(&script).trim().starts_with("PASS"),
        "expected re-init on edit"
    );
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
         local result_key = __rock_global_entry(\"RESULT\").slots[1]\n\
         local do_set_key = __rock_global_entry(\"DO_SET\").slots[1]\n\
         __rock_globals[do_set_key] = true; main()   -- RESULT = 99\n\
         _G.HOT_RELOAD = true\n{after}\n\
         __rock_globals[do_set_key] = false; main()  -- key unchanged despite edit: n preserved, RESULT = 99\n\
         if __rock_globals[result_key] == 99 then print(\"PASS\")\n\
         else print(\"FAIL RESULT=\"..tostring(__rock_globals[result_key])); os.exit(1) end\n"
    );
    assert!(
        run_luajit(&script).trim().starts_with("PASS"),
        "expected key stable under unrelated edit"
    );
}

/// A body edit that adds or removes a `static` changes no global's storage, so
/// it reloads as an overlay: the live globals survive, a new static initializes
/// on its first reach, and a removed one's cell is dropped from the storage.
const COUNTER: &str = "\
global TOTAL: int = 0
@nomangle fn bump() -> int {
    TOTAL = TOTAL + 1
    return TOTAL
}
fn main() {}
";

const COUNTER_WITH_STATIC: &str = "\
global TOTAL: int = 0
@nomangle fn bump() -> int {
    static step = 10
    TOTAL = TOTAL + step
    step = step + 1
    return TOTAL
}
fn main() {}
";

fn overlay_run(before: &str, after: &str, script: impl Fn(&str, &str) -> String) -> String {
    overlay_run_editing("bump", before, after, script)
}

fn overlay_run_editing(
    edited: &str,
    before: &str,
    after: &str,
    script: impl Fn(&str, &str) -> String,
) -> String {
    let ws1 = crate::common::ws_with_stdlib(before);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = skull::mir_emit::emit_full(&ws1, mid).expect("full emit");
    let ws2 = crate::common::ws_with_stdlib(after);
    let overlay = skull::mir_emit::emit_overlay(&ws2, &mut cache, &[body_id(&ws2, edited)])
        .expect("a body edit that adds or removes a static must overlay");
    crate::common::run_luajit(&script(&full.lua, &overlay.lua))
}

#[test]
fn a_body_edit_that_adds_a_static_overlays_over_live_globals() {
    let out = overlay_run(COUNTER, COUNTER_WITH_STATIC, |full, overlay| {
        format!(
            "{full}\nassert(bump() == 1)\nassert(bump() == 2)\n{overlay}\n\
             assert(bump() == 12)\nassert(bump() == 23)\nprint('ok')"
        )
    });
    assert_eq!(out, "ok");
}

#[test]
fn a_body_edit_that_removes_a_static_overlays_over_live_globals() {
    let out = overlay_run(COUNTER_WITH_STATIC, COUNTER, |full, overlay| {
        format!(
            "{full}\nassert(bump() == 10)\nassert(bump() == 21)\n\
             local function static_cells()\n\
               local n = 0\n\
               for key in pairs(__rock_globals) do\n\
                 if type(key) == 'string' and key:find('::step#', 1, true) then n = n + 1 end\n\
               end\n\
               return n\n\
             end\n\
             assert(static_cells() == 2, 'the static value and done cells are live')\n\
             {overlay}\n\
             assert(bump() == 22)\n\
             assert(static_cells() == 0, 'the removed static still holds ' .. static_cells() .. ' cells')\n\
             print('ok')"
        )
    });
    assert_eq!(out, "ok");
}

const SPECIALIZED: &str = "\
value V2 { x: float, y: float }
global TOTAL: float = 0.0
fn tagged<T>(v: T) -> T {
    static seen = 0
    seen = seen + 1
    return v
}
@nomangle fn bump() -> float {
    TOTAL = TOTAL + tagged(V2 { x: 1.0, y: 2.0 }).x
    return TOTAL
}
fn main() {}
";

/// A generic's static lives in its specialization's chunk, which the cache
/// builds apart from the ordinary bodies; removing it drops its cells all the same.
#[test]
fn a_body_edit_that_removes_a_specialized_generics_static_drops_its_cells() {
    let after = SPECIALIZED.replace("    static seen = 0\n    seen = seen + 1\n", "");
    let out = overlay_run_editing("tagged", SPECIALIZED, &after, |full, overlay| {
        format!(
            "{full}\nassert(bump() == 1)\n\
             local function static_cells()\n\
               local n = 0\n\
               for key in pairs(__rock_globals) do\n\
                 if type(key) == 'string' and key:find('::seen#', 1, true) then n = n + 1 end\n\
               end\n\
               return n\n\
             end\n\
             assert(static_cells() == 2, 'the specialized static holds ' .. static_cells() .. ' cells')\n\
             {overlay}\n\
             assert(bump() == 2)\n\
             assert(static_cells() == 0, 'the removed static still holds ' .. static_cells() .. ' cells')\n\
             print('ok')"
        )
    });
    assert_eq!(out, "ok");
}
