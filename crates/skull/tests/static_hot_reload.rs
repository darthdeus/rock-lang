use crate::common::{emit_without_stdlib, run_luajit};

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
