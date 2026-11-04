use crate::common::{run_luajit, ws_with_stdlib};
use skull::mir_emit::emit_full;

#[test]
fn first_function_value_exposure_initializes_runtime_slots_without_narrowing() {
    check_function_value_exposure("@pure ");
}

pub(crate) fn check_function_value_exposure(annotation: &str) {
    let _inline = crate::common::InlineOffGuard::on();
    let before = r#"
nobox value DemandRecord { x: int, y: int }
fn demand_leaf(v: DemandRecord) -> int { return v.y }
@nomangle fn caller() -> int { return demand_leaf(DemandRecord { x: 11, y: 23 }) }
@nomangle fn expose() -> int { return caller() }
"#;
    let before = before.replace("fn demand_leaf", &format!("{annotation}fn demand_leaf"));
    let after = before.replace(
        "return caller()",
        "let f = demand_leaf; return f(DemandRecord { x: 31, y: 47 })",
    );
    let ws1 = ws_with_stdlib(&before);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full(&ws1, mid).unwrap();
    let (_, mut overlay_cache) = emit_full(&ws1, mid).unwrap();
    let ws2 = ws_with_stdlib(&after);
    let f = crate::common::fn_ref(&ws2, "expose");
    let edits = [(f.module, bone_semantic::hir::BodyOwner::Function(f.func))];
    let incremental = skull::mir_emit::emit_incremental(&ws2, &mut cache, &edits).unwrap();
    assert_eq!(incremental.lua, emit_full(&ws2, mid).unwrap().0.lua);
    assert_eq!(
        run_luajit(
            &format!("{}\nprint(caller(), expose())", incremental.lua),
            "demand_function_value"
        ),
        "23\t47"
    );
    match skull::mir_emit::emit_overlay(&ws2, &mut overlay_cache, &edits) {
        Ok(overlay) => {
            assert_eq!(
                run_luajit(
                    &format!(
                        "{}\nlocal saved = caller\n{}\nprint(saved(), expose())",
                        full.lua, overlay.lua
                    ),
                    "demand_exposure_overlay"
                ),
                "23\t47"
            );
        }
        Err(error) => assert!(
            error.message.contains("compatibility") && error.message.contains("restart"),
            "{}",
            error.message
        ),
    }
}
