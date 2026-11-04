use crate::common;
use skull::instance_graph;

#[test]
fn inline_stats_scans_concrete_body_instances() {
    let ws = common::ws_of(
        r#"
@inline fn ident<T>(x: T) -> T { return x }
@inline fn wrap<T>(x: T) -> T { return ident(x) }

fn main() {
    let a = wrap(1)
    let b = wrap(true)
}
"#,
    );
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let graph = instance_graph::build(&ws, roots, false);
    let bodies = skull::concrete_body::build(&ws, &graph);
    let expected = bodies.plans.len();
    assert!(
        expected > 3,
        "fixture did not produce concrete generic instances: {expected}"
    );

    let report = skull::inline_stats::report(&ws);
    assert_eq!(
        report.funcs_scanned, expected,
        "inline stats must scan each concrete body plan, not source declarations"
    );
    assert!(
        report.total_splices > 0,
        "concrete plans produced no inline population"
    );
}
const EXHAUSTIVE_MATCH: &str = r#"
enum Choice { First, Second }
@inline fn match_value<T>(choice: Choice, first: T, second: T) -> T {
    match choice { Choice::First => first, Choice::Second => second }
}
@inline fn match_statement(choice: Choice) -> int {
    match choice {
        Choice::First => { return 10 }
        Choice::Second => { return 20 }
    }
}
@nomangle fn value_caller(choice: Choice) -> int { match_value(choice, 10, 20) + 1 }
@nomangle fn statement_caller(choice: Choice) -> int { match_statement(choice) + 1 }
"#;

#[test]
fn exhaustive_match_helpers_are_inlined() {
    let ws = common::ws_with_stdlib(EXHAUSTIVE_MATCH);
    let report = skull::inline_stats::report(&ws);
    for name in ["match_value", "match_statement"] {
        assert!(
            report
                .text
                .lines()
                .any(|line| line.split_whitespace().last() == Some(name)),
            "exhaustive match helper {name} was not inlined:\n{}",
            report.text
        );
    }
    let lua = common::emit_module(EXHAUSTIVE_MATCH);
    for (caller, helper) in [
        ("value_caller", "match_value"),
        ("statement_caller", "match_statement"),
    ] {
        let body = common::function_body(&lua, caller);
        assert!(!body.contains(helper), "retained {helper} call:\n{body}");
    }
}

#[test]
fn inlined_exhaustive_match_preserves_fallback_error() {
    common::require_luajit();
    for source in [
        EXHAUSTIVE_MATCH.to_string(),
        EXHAUSTIVE_MATCH.replace("@inline ", ""),
    ] {
        let mut lua = common::emit_module(&source);
        // Raw Lua calls bypass the enum type check to exercise the fallback.
        lua.push_str(
            r#"
for _, caller in ipairs({value_caller, statement_caller}) do
    assert(caller(0) == 11)
    assert(caller(1) == 21)
    local ok, err = pcall(caller, 99)
    assert(not ok, "exhaustive match fallback returned")
    local message = tostring(err):match("rock:.*")
    assert(message == "rock: reached an unreachable block", tostring(err))
end
print("ok")
"#,
        );
        assert_eq!(common::run_luajit(&lua, "exhaustive_match_fallback"), "ok");
    }
}
