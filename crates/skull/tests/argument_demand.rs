use crate::common::{
    body_id, edit, full_lua, function_params, inline_off_guard, overlay_or_restart, run_luajit,
    ws_with_stdlib,
};

#[test]
fn noalloc_preserves_parameter_demand() {
    let source = r#"
value Sample { kept: int, unused: int }
@noalloc fn read(v: Sample) -> int { return v.kept }
fn main() { let _ = read(Sample { kept: 1, unused: 2 }) }
"#;
    let unannotated = source.replace("@noalloc ", "         ");
    let plain = full_lua(&ws_with_stdlib(&unannotated));
    assert_eq!(function_params(&plain, "read").len(), 1);
    assert_eq!(full_lua(&ws_with_stdlib(source)), plain);
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
