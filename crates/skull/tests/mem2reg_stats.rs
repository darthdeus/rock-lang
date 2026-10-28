use crate::common::{self, function_body, run_luajit};

#[test]
fn mem2reg_stats_replays_cleaned_concrete_bodies() {
    let ws = common::ws_with_stdlib(
        r#"
@lifecycle(phase="update", owner="process") deco init tick {}

fn choose(flag: bool) -> int {
    let retained = 7
    if flag { print("taken") }
    return retained
}

@tick fn update_system() { print(choose(true)) }

fn main() { print(choose(true)) }
"#,
    );
    let mid = ws.module_ids[&ustr::ustr("<test>")];
    let report = skull::mem2reg_stats::report(&ws, mid).expect("mem2reg report");
    assert!(report.bodies_scanned > 0);
    let choose = report
        .rows
        .iter()
        .find(|row| row.name.contains("choose"))
        .expect("choose concrete body row");
    assert_eq!(
        choose.census.store_promotable_cells, 0,
        "{:#?}",
        choose.census
    );
    let update = report
        .lifecycle_routes
        .iter()
        .find(|route| route.name == "frame.update")
        .expect("update mem2reg route");
    assert!(update.bodies_scanned > 0);
    assert!(
        update.rows.iter().any(|row| row.name.contains("choose")),
        "the update route must retain the reachable cross-block body",
    );
}

#[test]
fn direct_promotion_preserves_branch_and_loop_results() {
    let lua = common::emit_module(
        r#"
fn choose(flag: bool) -> int {
    let retained = if flag { 7 } else { 11 }
    let total = retained + 1
    if flag { return total }
    return total + 1
}

fn looped(limit: int) -> int {
    let retained = limit + 3
    let i = 0
    let total = 0
    while i < limit {
        total += retained
        i += 1
    }
    return total
}

fn main() {
    print(choose(true), choose(false), looped(4))
}
"#,
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "direct_mem2reg"),
        "8\t13\t28"
    );
}

#[test]
fn immutable_parameter_loads_render_at_their_uses() {
    let lua = common::emit_module(
        r#"
fn param_route(value: int, flag: bool) -> int {
    if flag { return value + value }
    return value - 1
}

fn parameter_snapshot(value: int) -> int {
    let old = value
    value = 9
    return old
}

fn main() { print(param_route(7, true), param_route(7, false), parameter_snapshot(7)) }
"#,
    );
    let body = function_body(&lua, "param_route");
    let copied_parameter_loads = body
        .lines()
        .filter(|line| line.trim_start().starts_with('r') && line.contains(" = c"))
        .count();
    assert_eq!(copied_parameter_loads, 0, "{body}");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "parameter_loads"),
        "14\t6\t7"
    );
}
