use crate::common::{body_id, run_luajit, ws_with_stdlib};
use skull::mir_emit::emit_full;

#[test]
fn compact_arguments_keep_discarded_errors_before_later_argument_effects() {
    let _inline = crate::common::InlineOffGuard::on();
    let ws = ws_with_stdlib(
        r#"
value Sample { kept: int, dropped: uint }
global marks: int = 0
fn forward(n: int) -> int { return n }
fn demand_read(v: Sample, marker: int) -> int { return forward(v.kept) }
fn mark() -> int { marks += 1; return 0 }
@nomangle fn probe(n: int) -> int {
    let value = Sample { kept: 7, dropped: n as uint }
    return demand_read(value, mark())
}
@nomangle fn observed() -> int { return marks }
"#,
    );
    let lua = emit_full(&ws, skull::user_module_id(&ws).unwrap())
        .unwrap()
        .0
        .lua;
    let program = format!(
        "{lua}\nlocal ok = pcall(probe, -1); print(ok, observed(), probe(1), observed())\n"
    );
    assert_eq!(run_luajit(&program), "false\t0\t7\t1");
}

#[test]
fn compact_arguments_keep_discarded_field_errors() {
    let _inline = crate::common::InlineOffGuard::on();
    let ws = ws_with_stdlib(
        r#"
struct Ref { n: int }
value Sample { kept: int, dropped: int }
inline(lua) fn foreign_ref() -> Ref = "return nil"
fn demand_read(v: Sample) -> int { return v.kept }
@nomangle fn probe() -> int {
    let value = Sample { kept: 7, dropped: foreign_ref().n }
    return demand_read(value)
}
"#,
    );
    let lua = emit_full(&ws, skull::user_module_id(&ws).unwrap())
        .unwrap()
        .0
        .lua;
    let program = format!("{lua}\nprint(pcall(probe))\n");
    assert!(run_luajit(&program).starts_with("false\t"));
}

#[test]
fn changed_parameter_demand_rebuilds_residual_callers_and_checks_overlay_compatibility() {
    let _inline = crate::common::InlineOffGuard::on();
    let before = r#"
nobox value Sample { x: int, y: int, z: int }
fn forward(n: int) -> int { return n }
fn demand_read(v: Sample) -> int { return forward(v.y) }
@nomangle fn caller() -> int { return demand_read(Sample { x: 11, y: 23, z: 47 }) }
@nomangle fn save_caller() -> fn() -> int { return caller }
"#;
    let after = before.replace("forward(v.y)", "forward(v.x + v.y)");
    let ws1 = ws_with_stdlib(before);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full1, mut cache) = emit_full(&ws1, mid).unwrap();
    let (_, mut overlay_cache) = emit_full(&ws1, mid).unwrap();
    let ws2 = ws_with_stdlib(&after);
    let edited = [body_id(&ws2, "demand_read")];
    let incremental = skull::mir_emit::emit_incremental(&ws2, &mut cache, &edited).unwrap();
    let fresh = emit_full(&ws2, mid).unwrap().0;
    assert_eq!(incremental.lua, fresh.lua);
    assert_eq!(
        run_luajit(&format!("{}\nprint(caller())", incremental.lua)),
        "34"
    );
    match skull::mir_emit::emit_overlay(&ws2, &mut overlay_cache, &edited) {
        Ok(overlay) => {
            let program = format!(
                "{}\nlocal saved = save_caller()\n{}\nprint(caller(), saved())",
                full1.lua, overlay.lua
            );
            assert_eq!(run_luajit(&program), "34\t34");
        }
        Err(error) => {
            assert!(
                error.message.contains("compatibility") && error.message.contains("restart"),
                "{}",
                error.message
            );
            assert_eq!(
                run_luajit(&format!(
                    "{}\nlocal saved = save_caller(); print(saved())",
                    full1.lua
                )),
                "23"
            );
        }
    }
}

#[test]
fn function_value_exposure_restores_full_transport_in_incremental_and_overlay() {
    crate::overlay_function_values::check_function_value_exposure("");
}

#[test]
fn match_guard_demand_edit_updates_residual_calls_and_live_callables() {
    let _inline = crate::common::InlineOffGuard::on();
    let before = r#"
nobox value MatchPayload { a: int, b: int, c: int, d: int }
enum MatchSubject { Present(MatchPayload), Empty }
nobox value MatchEvent { unused: int, subject: MatchSubject, guard_value: int }
fn demand_match_read(event: MatchEvent) -> int {
    return match event.subject {
        MatchSubject::Present(payload) => payload.c,
        MatchSubject::Empty => 0,
    }
}
@nomangle fn match_demand_caller() -> int {
    return demand_match_read(MatchEvent {
        unused: 97,
        subject: MatchSubject::Present(MatchPayload { a: 11, b: 23, c: 47, d: 71 }),
        guard_value: 5,
    })
}
@nomangle fn match_demand_save() -> fn() -> int { return match_demand_caller }
"#;
    let after = before.replace(
        "MatchSubject::Present(payload) => payload.c,",
        "MatchSubject::Present(payload) if event.guard_value > 10 => payload.c,\n        MatchSubject::Present(_) => event.guard_value,",
    );
    let ws1 = ws_with_stdlib(before);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full1, mut cache) = emit_full(&ws1, mid).unwrap();
    let (_, mut overlay_cache) = emit_full(&ws1, mid).unwrap();
    assert_eq!(
        run_luajit(&format!("{}\nprint(match_demand_caller())", full1.lua)),
        "47"
    );
    let ws2 = ws_with_stdlib(&after);
    let edited = [body_id(&ws2, "demand_match_read")];
    let incremental = skull::mir_emit::emit_incremental(&ws2, &mut cache, &edited).unwrap();
    let fresh = emit_full(&ws2, mid).unwrap().0;
    assert_eq!(incremental.lua, fresh.lua);
    assert_eq!(
        run_luajit(&format!("{}\nprint(match_demand_caller())", fresh.lua)),
        "5"
    );
    match skull::mir_emit::emit_overlay(&ws2, &mut overlay_cache, &edited) {
        Ok(overlay) => {
            let program = format!(
                "{}\nlocal saved = match_demand_save()\n{}\nprint(match_demand_caller(), saved())",
                full1.lua, overlay.lua
            );
            assert_eq!(run_luajit(&program), "5\t5");
        }
        Err(error) => assert!(
            error.message.contains("compatibility") && error.message.contains("restart"),
            "{}",
            error.message
        ),
    }
}

#[test]
fn match_unit_enum_result_preserves_payload_variants_and_absence() {
    let _inline = crate::common::InlineOffGuard::on();
    let ws = ws_with_stdlib(
        r#"
nobox value DirectionIdentity { scene: int, arena: int, index: int, generation: int }
enum DirectionSubject { Absent, Primary, First(DirectionIdentity), Second(DirectionIdentity) }
enum DirectionResult { ToPrimary, ToEntity }
nobox value DirectionEvent { cold: int, subject: DirectionSubject }
fn demand_direction_result(event: DirectionEvent) -> DirectionResult? {
    return match event.subject {
        DirectionSubject::Primary => DirectionResult::ToPrimary,
        DirectionSubject::First(_) => DirectionResult::ToEntity,
        DirectionSubject::Second(_) => DirectionResult::ToEntity,
        DirectionSubject::Absent => null,
    }
}
@nomangle fn unit_match_probe(kind: int) -> int {
    let id = DirectionIdentity { scene: 1, arena: 2, index: 3, generation: 4 }
    let subject = match kind {
        0 => DirectionSubject::Absent,
        1 => DirectionSubject::Primary,
        2 => DirectionSubject::First(id),
        _ => DirectionSubject::Second(id),
    }
    let result = demand_direction_result(DirectionEvent { cold: 97, subject: subject }) ?? return -1
    return match result { DirectionResult::ToPrimary => 11, DirectionResult::ToEntity => 23 }
}
"#,
    );
    let lua = emit_full(&ws, skull::user_module_id(&ws).unwrap())
        .unwrap()
        .0
        .lua;
    assert_eq!(
        run_luajit(&format!(
            "{lua}\nprint(unit_match_probe(0), unit_match_probe(1), unit_match_probe(2), unit_match_probe(3))"
        )),
        "-1\t11\t23\t23"
    );
}
