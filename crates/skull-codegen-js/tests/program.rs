use crate::common::{test_mid, try_ws_with_named};

#[test]
fn unsupported_wire_dependencies_reject_on_both_backends() {
    for (annotation, expected) in [
        ("[MUTABLE]", "mutable"),
        ("[callable]", "names a function"),
        ("[UNKNOWN]", "`UNKNOWN` did not resolve to any definition"),
        ("[]", "requires a non-empty dependency list"),
        ("[GOOD.value]", "structured identifier list"),
    ] {
        let source = format!(
            r#"
            @lifecycle(phase="wire", owner="process") deco init wire {{}}
            const GOOD: int = 1
            global MUTABLE: int = 2
            fn callable() {{}}
            @wire(onchange={annotation}) fn apply_game_config() {{}}
            fn main() {{}}
            "#
        );
        let ws = match try_ws_with_named("<test>", &source) {
            Ok(ws) => ws,
            Err(error) => {
                assert!(error.message.contains(expected), "{annotation}: {error}");
                continue;
            }
        };
        let mid = test_mid(&ws);
        let lua = match skull::mir_emit::emit_full(&ws, mid) {
            Ok(_) => panic!("Lua accepted unsupported wire dependency {annotation}"),
            Err(error) => error.message,
        };
        let js = match skull_codegen_js::emit::emit_program(&ws, mid) {
            Ok(_) => panic!("JS accepted unsupported wire dependency {annotation}"),
            Err(error) => error.message,
        };
        assert!(lua.contains(expected), "Lua {annotation}: {lua}");
        assert!(js.contains(expected), "JS {annotation}: {js}");
    }
}
