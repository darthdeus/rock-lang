use crate::common::{body_id, emit_full_of, emit_without_stdlib, run_luajit};

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
    let a = emit_without_stdlib(STRING_STATIC);
    let b = emit_without_stdlib(STRING_STATIC);
    assert_eq!(a, b);
}

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

fn result_after_reload(before: &str, after: &str) -> String {
    run_luajit(&format!(
        "_G.HOT_RELOAD = false\n{before}\n\
         local result_key = __rock_global_entry(\"RESULT\").slots[1]\n\
         local do_set_key = __rock_global_entry(\"DO_SET\").slots[1]\n\
         __rock_globals[do_set_key] = true; main()\n\
         _G.HOT_RELOAD = true\n{after}\n\
         __rock_globals[do_set_key] = false; main()\n\
         print(__rock_globals[result_key])\n"
    ))
}

#[test]
fn static_reload_preserves_unless_the_initializer_changed() {
    let before = emit_without_stdlib(PROBE);
    for (_case, after, expected) in [
        ("identical reload", PROBE.to_string(), "99"),
        (
            "unrelated body edit",
            PROBE.replace("    return n\n", "    let unused = 0\n    return n\n"),
            "99",
        ),
        (
            "initializer edit",
            PROBE.replace("static n = 42", "static n = 7"),
            "7",
        ),
    ] {
        let after = emit_without_stdlib(&after);
        assert_eq!(result_after_reload(&before, &after), expected);
    }
}

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
    let (full, mut cache) = emit_full_of(&ws1, "full emit");
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
