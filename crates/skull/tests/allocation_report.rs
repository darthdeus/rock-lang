use crate::common::{emit_mir, AllocationActivationGuard, AllocationReportGuard, InlineOffGuard};

#[test]
fn allocation_report_is_off_for_an_ordinary_compile() {
    let emit = emit_mir("fn root() -> [int] { return [1] }");
    assert!(emit.allocation_report.bodies.is_empty());
    assert!(!emit.lua.contains("__rock_alloc_hit"));
    assert!(emit
        .allocation_report
        .lifecycle_routes
        .iter()
        .all(|route| route.findings.is_empty()));
}

#[test]
fn static_allocation_reporting_does_not_change_the_emitted_program() {
    let source = r#"
fn render(a: int) -> string {
    return "value={a}"
}
"#;
    let ordinary = emit_mir(source);
    let reported = {
        let _report = AllocationReportGuard::on();
        emit_mir(source)
    };

    assert_eq!(ordinary.lua, reported.lua);
    assert!(
        reported
            .allocation_report
            .bodies
            .values()
            .any(|body| !body.direct_sites.is_empty()),
        "the byte-identical reported build still needs a populated manifest",
    );
}

#[test]
fn allocation_report_traces_final_mir_sites_from_a_named_root() {
    let _inline = InlineOffGuard::on();
    let _report = AllocationReportGuard::on();
    let emit = emit_mir(
        r#"
fn alloc_leaf() -> [int] {
    return [1, 2, 3]
}

fn update_root() -> int {
    return alloc_leaf().len()
}
"#,
    );

    let route = emit
        .allocation_report
        .trace_from_diagnostic_names("manual.update", &["update_root"]);

    assert_eq!(
        route.roots.len(),
        1,
        "the named root must resolve exactly once"
    );
    assert_eq!(
        route.reachable_bodies, 4,
        "the route must include the residual stdlib len chain"
    );
    assert_eq!(
        route.findings.len(),
        1,
        "the route must expose the one Vec construction"
    );
    let finding = &route.findings[0];
    assert_eq!(finding.site.kind.as_str(), "vec-construction");
    assert!(
        finding.site.lua_body_line > 0,
        "the allocation site must name its line inside the emitted Lua body",
    );
    assert_eq!(
        finding
            .site
            .span
            .expect("source construction keeps its span")
            .format_site(),
        "<test>:3"
    );
    assert_eq!(
        finding.call_chain.len(),
        2,
        "the finding must retain its causal call chain"
    );
    assert!(finding.call_chain[0].contains("update_root"));
    assert!(finding.call_chain[1].contains("alloc_leaf"));
}

#[test]
fn allocation_report_retains_every_reachable_direct_caller() {
    let _inline = InlineOffGuard::on();
    let _report = AllocationReportGuard::on();
    let emit = emit_mir(
        r#"
fn alloc_leaf() -> [int] {
    return [1, 2, 3]
}

fn left() -> int {
    return alloc_leaf().len()
}

fn right() -> int {
    return alloc_leaf().len()
}

fn update_root() -> int {
    return left() + right()
}
"#,
    );

    let route = emit
        .allocation_report
        .trace_from_diagnostic_names("manual.update", &["update_root"]);
    let finding = route
        .findings
        .iter()
        .find(|finding| finding.site.kind.as_str() == "vec-construction")
        .expect("shared Vec construction");

    assert_eq!(
        finding.direct_callers.len(),
        2,
        "a shared allocation body must not collapse to one arbitrary caller",
    );
    assert!(finding
        .direct_callers
        .iter()
        .any(|caller| caller.contains("left")));
    assert!(finding
        .direct_callers
        .iter()
        .any(|caller| caller.contains("right")));
}

#[test]
fn allocation_report_counts_the_flat_emitted_concat_not_its_folded_mir_children() {
    let _report = AllocationReportGuard::on();
    let emit = emit_mir(
        r#"
fn render(a: int, b: int) -> string {
    return "left={a}, right={b}"
}
"#,
    );
    let route = emit
        .allocation_report
        .trace_from_diagnostic_names("manual.draw", &["render"]);

    assert_eq!(
        route
            .findings
            .iter()
            .filter(|finding| finding.site.kind.as_str() == "string-concat")
            .count(),
        1,
        "one flat emitted BC_CAT expression is one allocation-capable site",
    );
}

#[test]
fn allocation_activation_wraps_the_same_manifest_site_and_installs_its_runtime() {
    let _activation = AllocationActivationGuard::on();
    let emit = emit_mir(
        r#"
fn render(a: int) -> string {
    return "value={a}"
}
"#,
    );
    let route = emit
        .allocation_report
        .trace_from_diagnostic_names("manual.draw", &["render"]);
    let concat = route
        .findings
        .iter()
        .find(|finding| finding.site.kind.as_str() == "string-concat")
        .expect("emitted concat site");

    assert!(
        emit.lua
            .contains(&format!("__rock_alloc_hit(\"{:016x}\", ", concat.site.id)),
        "the runtime hit must use the manifest's exact artifact-local ID",
    );
    let function_line = emit
        .lua
        .lines()
        .position(|line| line.starts_with(&format!("function {}", concat.body)))
        .expect("instrumented emitted body")
        + 1;
    let emitted_line = emit
        .lua
        .lines()
        .position(|line| line.contains(&format!("{:016x}", concat.site.id)))
        .expect("instrumented allocation expression line")
        + 1;
    assert_eq!(
        concat.site.lua_body_line,
        (emitted_line - function_line + 1) as u32,
        "the manifest line must address the instrumented emitted body",
    );
    assert!(
        emit.lua.contains("function __rock_alloc_hit(id, value)"),
        "an activated artifact must install the counter runtime",
    );
    assert!(
        emit.lua
            .contains("function __rock_allocation_activation_begin()"),
        "the runner needs an explicit boundary that excludes initialization",
    );
}

#[test]
fn allocation_report_builds_update_and_draw_routes_from_lifecycle_metadata() {
    let _report = AllocationReportGuard::on();
    let emit = emit_mir(
        r#"
@lifecycle(phase="update", owner="session") deco init tick {}
@lifecycle(phase="draw", owner="session") deco init paint {}

global FRAME_VALUE: int = 0

@tick fn update_system() {
    let values = [1, 2]
    FRAME_VALUE = values.len()
}

@paint fn draw_system() {
    FRAME_VALUE = FRAME_VALUE + 1
}
"#,
    );

    let update = emit
        .allocation_report
        .lifecycle_routes
        .iter()
        .find(|route| route.name == "frame.update")
        .expect("update route");
    let draw = emit
        .allocation_report
        .lifecycle_routes
        .iter()
        .find(|route| route.name == "frame.draw")
        .expect("draw route");

    assert_eq!(
        update.roots.len(),
        1,
        "the update application is the exact root population"
    );
    assert_eq!(
        draw.roots.len(),
        1,
        "the draw application is the exact root population"
    );
    assert_eq!(
        update
            .findings
            .iter()
            .filter(|finding| finding.site.kind.as_str() == "vec-construction")
            .count(),
        1,
        "the update route reaches its allocation-capable Vec site",
    );
    assert_eq!(
        draw.findings.len(),
        0,
        "the scalar draw route has no allocation-capable site"
    );
}
