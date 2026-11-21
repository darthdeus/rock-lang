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

const PROBE: &str = "\
global RESULT: int = 0
global DO_SET: bool = false
fn probe() -> int {
    static n = {
        let (initial, _) = (42, 0)
        initial
    }
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
        ("initializer edit", PROBE.replace("(42, 0)", "(7, 0)"), "7"),
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
