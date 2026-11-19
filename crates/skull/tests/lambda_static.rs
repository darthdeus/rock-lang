use crate::common::{emit_module, run_luajit};

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

#[test]
fn lambda_body_static_folds_into_enclosing_chunk_statics() {
    let lua = emit_module(PROBE);
    assert!(lua.contains("[\"static:named:<test>::setup"));
    assert!(
        lua.lines().any(
            |l| l.contains("static:named:<test>::setup") && l.contains("__rock_static_prepare")
        )
    );
}

#[test]
fn lambda_body_static_reinits_only_on_initializer_edit() {
    let before = emit_module(PROBE);
    for (after, expected) in [
        (PROBE.replace("static n = 100", "static n = 7"), "8"),
        (PROBE.to_string(), "102"),
    ] {
        let after = emit_module(&after);
        let script = format!(
            "_G.HOT_RELOAD = false\n{before}\nmain()\n\
             _G.HOT_RELOAD = true\n{after}\nmain()\n\
             print(__rock_globals[__rock_global_entry(\"RESULT\").slots[1]])\n"
        );
        assert_eq!(run_luajit(&script), expected);
    }
}
