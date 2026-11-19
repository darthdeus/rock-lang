use crate::common::{
    body_id, edit, full_lua, function_params, inline_off_guard, overlay_or_restart, run_luajit,
    ws_with_stdlib,
};

#[test]
fn compact_arguments_keep_discarded_errors_before_later_argument_effects() {
    let _inline = inline_off_guard();
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
    let lua = full_lua(&ws);
    let program = format!(
        "{lua}\nlocal ok = pcall(probe, -1); print(ok, observed(), probe(1), observed())\n"
    );
    assert_eq!(run_luajit(&program), "false\t0\t7\t1");
}

#[test]
fn compact_arguments_keep_discarded_field_errors() {
    let _inline = inline_off_guard();
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
    let lua = full_lua(&ws);
    let program = format!("{lua}\nprint(pcall(probe))\n");
    assert!(run_luajit(&program).starts_with("false\t"));
}

#[test]
fn match_unit_enum_result_preserves_payload_variants_and_absence() {
    let _inline = inline_off_guard();
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
    let lua = full_lua(&ws);
    assert_eq!(
        run_luajit(&format!(
            "{lua}\nprint(unit_match_probe(0), unit_match_probe(1), unit_match_probe(2), unit_match_probe(3))"
        )),
        "-1\t11\t23\t23"
    );
}

#[test]
fn unused_parameters_are_not_passed_and_their_arguments_still_run() {
    let _inline = inline_off_guard();
    let ws = ws_with_stdlib(
        r#"
global marks: int = 0
fn mark() -> int { marks += 1; return 0 }
fn skip_middle(a: int, unused: int, b: string) -> string { return "{a}{b}" }
@nomangle fn probe() -> string { return skip_middle(4, mark(), "x") }
@nomangle fn observed() -> int { return marks }
"#,
    );
    let lua = full_lua(&ws);
    assert_eq!(function_params(&lua, "skip_middle").len(), 2);
    let program = format!("{lua}\nprint(probe(), observed())\n");
    assert_eq!(run_luajit(&program), "4x\t1");
}

#[test]
fn a_function_value_keeps_its_unused_parameters() {
    let _inline = inline_off_guard();
    let ws = ws_with_stdlib(
        r#"
fn skip_middle(a: int, unused: int, b: int) -> int { return a + b }
@nomangle fn probe() -> int {
    let f = skip_middle
    return f(1, 2, 3) + skip_middle(10, 20, 30)
}
"#,
    );
    let lua = full_lua(&ws);
    assert_eq!(function_params(&lua, "skip_middle").len(), 3);
    assert_eq!(run_luajit(&format!("{lua}\nprint(probe())\n")), "44");
}

#[test]
fn a_lambda_capture_keeps_the_whole_parameter() {
    let _inline = inline_off_guard();
    let ws = ws_with_stdlib(
        r#"
value Pair { a: int, b: int }
fn reader(p: Pair, unused: int) -> fn() -> int { return () => p.a }
@nomangle fn probe() -> int { return reader(Pair { a: 5, b: 6 }, 7)() }
"#,
    );
    let lua = full_lua(&ws);
    assert_eq!(function_params(&lua, "reader").len(), 2);
    assert_eq!(run_luajit(&format!("{lua}\nprint(probe())\n")), "5");
}

#[test]
fn a_demanded_boxed_value_child_keeps_the_whole_parameter() {
    let _inline = inline_off_guard();
    let ws = ws_with_stdlib(
        r#"
value In { c: int, s: string }
value MaybeIn { x: int, b: In? }
fn child(i: In?) -> int { return i?.c ?? 0 }
fn read_child(a: MaybeIn) -> int { return child(a.b) }
@nomangle fn probe() -> int { return read_child(MaybeIn { x: 1, b: In { c: 4, s: "a" } }) }
"#,
    );
    let lua = full_lua(&ws);
    assert_eq!(run_luajit(&format!("{lua}\nprint(probe())\n")), "4");
}

struct DemandEdit {
    before: String,
    after: String,
    edited: &'static str,
    probe: &'static str,
    before_result: &'static str,
    after_result: &'static str,
    save: &'static str,
    saved_result: &'static str,
}

const PARAMETER_DEMAND: &str = r#"
nobox value Sample { x: int, y: int, z: int }
fn forward(n: int) -> int { return n }
fn demand_read(v: Sample) -> int { return forward(v.y) }
@nomangle fn caller() -> int { return demand_read(Sample { x: 11, y: 23, z: 47 }) }
@nomangle fn save_caller() -> fn() -> int { return caller }
"#;

const MATCH_GUARD_DEMAND: &str = r#"
nobox value MatchPayload { a: int, b: int, c: int, d: int }
enum MatchSubject { Present(MatchPayload), Empty }
nobox value MatchEvent { unused: int, subject: MatchSubject, guard_value: int }
fn demand_match_read(event: MatchEvent) -> int {
    return match event.subject {
        MatchSubject::Present(payload) => payload.c,
        MatchSubject::Empty => 0,
    }
}
@nomangle fn caller() -> int {
    return demand_match_read(MatchEvent {
        unused: 97,
        subject: MatchSubject::Present(MatchPayload { a: 11, b: 23, c: 47, d: 71 }),
        guard_value: 5,
    })
}
@nomangle fn save_caller() -> fn() -> int { return caller }
"#;

const UNUSED_PARAMETER: &str = r#"
fn pick(a: int, b: int) -> int { return a }
@nomangle fn caller() -> int { return pick(3, 4) }
@nomangle fn save_caller() -> fn() -> int { return caller }
"#;

const FUNCTION_VALUE_EXPOSURE: &str = r#"
nobox value DemandRecord { x: int, y: int }
fn demand_leaf(v: DemandRecord) -> int { return v.y }
@nomangle fn caller() -> int { return demand_leaf(DemandRecord { x: 11, y: 23 }) }
@nomangle fn expose() -> int { return caller() }
"#;

#[test]
fn demand_edits_rebuild_residual_callers_and_live_callables() {
    let _inline = inline_off_guard();
    assert_eq!(
        function_params(&full_lua(&ws_with_stdlib(UNUSED_PARAMETER)), "pick").len(),
        1
    );
    let exposure = |annotation: &str| {
        let before = FUNCTION_VALUE_EXPOSURE
            .replace("fn demand_leaf", &format!("{annotation}fn demand_leaf"));
        let after = before.replace(
            "return caller()",
            "let f = demand_leaf; return f(DemandRecord { x: 31, y: 47 })",
        );
        DemandEdit {
            before,
            after,
            edited: "expose",
            probe: "caller(), expose()",
            before_result: "23\t23",
            after_result: "23\t47",
            save: "caller",
            saved_result: "23",
        }
    };
    let cases = [
        DemandEdit {
            before: PARAMETER_DEMAND.to_string(),
            after: PARAMETER_DEMAND.replace("forward(v.y)", "forward(v.x + v.y)"),
            edited: "demand_read",
            probe: "caller()",
            before_result: "23",
            after_result: "34",
            save: "save_caller()",
            saved_result: "34",
        },
        DemandEdit {
            before: MATCH_GUARD_DEMAND.to_string(),
            after: MATCH_GUARD_DEMAND.replace(
                "MatchSubject::Present(payload) => payload.c,",
                "MatchSubject::Present(payload) if event.guard_value > 10 => payload.c,\n        MatchSubject::Present(_) => event.guard_value,",
            ),
            edited: "demand_match_read",
            probe: "caller()",
            before_result: "47",
            after_result: "5",
            save: "save_caller()",
            saved_result: "5",
        },
        DemandEdit {
            before: UNUSED_PARAMETER.to_string(),
            after: UNUSED_PARAMETER.replace("return a }", "return a + b }"),
            edited: "pick",
            probe: "caller()",
            before_result: "3",
            after_result: "7",
            save: "save_caller()",
            saved_result: "7",
        },
        exposure(""),
        exposure("@pure "),
    ];
    for case in cases {
        let after = ws_with_stdlib(&case.after);
        let edit = edit(
            &ws_with_stdlib(&case.before),
            &after,
            &[body_id(&after, case.edited)],
        );
        let probe = format!("\nprint({})", case.probe);
        assert_eq!(
            run_luajit(&format!("{}{probe}", edit.base.lua)),
            case.before_result
        );
        assert_eq!(
            run_luajit(&format!("{}{probe}", edit.incremental.lua)),
            case.after_result
        );
        if let Some(overlay) = overlay_or_restart(edit.overlay) {
            let program = format!(
                "{}\nlocal saved = {}\n{}\nprint({}, saved())",
                edit.base.lua, case.save, overlay.lua, case.probe
            );
            assert_eq!(
                run_luajit(&program),
                format!("{}\t{}", case.after_result, case.saved_result),
                "{}",
                case.edited
            );
        }
    }
}
