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
    assert!(
        reported
            .allocation_report
            .bodies
            .values()
            .all(|body| body.mem2reg_census.is_none()),
        "ordinary allocation reporting must not pay for the separate mem2reg census",
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
fn allocation_report_accumulates_local_and_call_edge_loop_depth() {
    let _inline = InlineOffGuard::on();
    let _report = AllocationReportGuard::on();
    let emit = emit_mir(
        r#"
fn direct_leaf() -> [int] {
    return [1]
}

fn loop_leaf(i: int) -> [int] {
    return [i]
}

fn update_root() -> int {
    let total = direct_leaf().len()
    for i in 0..3 {
        total += loop_leaf(i).len()
        for j in 0..2 {
            let pair = [i, j]
            total += pair.len()
        }
    }
    return total
}
"#,
    );

    let route = emit
        .allocation_report
        .trace_from_diagnostic_names("manual.update", &["update_root"]);
    let depth_for = |line: &str| {
        route
            .findings
            .iter()
            .find(|finding| {
                finding
                    .site
                    .span
                    .is_some_and(|span| span.format_site() == line)
            })
            .map(|finding| finding.structural_loop_depth)
            .unwrap_or_else(|| panic!("missing allocation at {line}: {route:#?}"))
    };

    assert_eq!(depth_for("<test>:3"), 0, "straight-line call stays scalar");
    assert_eq!(depth_for("<test>:7"), 1, "a looped call amplifies its callee");
    assert_eq!(depth_for("<test>:15"), 2, "nested local construction keeps both loops");
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
fn allocation_report_classifies_aggregate_result_uses_without_calling_them_safe() {
    let _report = AllocationReportGuard::on();
    let emit = emit_mir(
        r#"
struct Carrier { value: int }

global RETAINED: Carrier? = null

fn consume(carrier: Carrier) -> int {
    return carrier.value
}

fn direct() -> int {
    return consume(Carrier { value: 1 })
}

fn projected() -> int {
    return Carrier { value: 2 }.value
}

fn returned() -> Carrier {
    return Carrier { value: 3 }
}

fn retained() {
    RETAINED = Carrier { value: 4 }
}
"#,
    );

    let disposition = |name: &str| {
        let body = emit
            .allocation_report
            .bodies
            .values()
            .find(|body| body.diagnostic_name == name)
            .unwrap_or_else(|| panic!("missing body {name}"));
        let sites = body
            .direct_sites
            .iter()
            .filter(|site| site.kind.as_str() == "struct-construction")
            .collect::<Vec<_>>();
        assert_eq!(sites.len(), 1, "{name} must own exactly one struct site: {sites:#?}");
        let site = sites[0];
        assert_eq!(site.aggregate_type.as_deref(), Some("Carrier"));
        assert_eq!(site.aggregate_fields, 1);
        assert_eq!(site.result_uses, 1);
        site.result_use.as_str()
    };

    assert_eq!(disposition("direct"), "direct-call");
    assert_eq!(disposition("projected"), "field-only");
    assert_eq!(disposition("returned"), "returned");
    assert_eq!(disposition("retained"), "global-store");

    let manifest = emit.allocation_report.render_text();
    assert!(
        manifest.lines().any(|line| {
            line.contains("\tstruct-construction\t")
                && line.contains("\tdirect-call\t1\tCarrier\t1")
        }),
        "the serialized manifest must retain the direct-use and aggregate-shape facts: {manifest}",
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
            .contains(&format!("(__rock_alloc_site[\"{:016x}\"] and ", concat.site.id)),
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
    assert!(
        emit.lua
            .contains("function __rock_allocation_activation_frame_begin()"),
        "the runtime must distinguish active frames from outside-frame hits",
    );
    assert!(
        emit.lua
            .contains("function __rock_allocation_activation_frame_end()"),
        "the runtime must close the frame envelope after drawing",
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

#[test]
fn allocation_report_registers_and_instruments_transformed_coroutine_bodies() {
    let _activation = AllocationActivationGuard::on();
    let emit = emit_mir(
        r#"
enum Event { Text(string) }
global SINK: string = ""

co fn worker(seed: int) -> Coroutine<Event> {
    defer {
        SINK = "cleanup={seed}"
    }
    on_killed(() => {
        SINK = "killed={seed}"
    })
    yield Event::Text("value={seed}")
}

fn make_worker() -> Coroutine<Event> {
    return worker(7)
}
"#,
    );

    let coroutine_bodies = emit
        .allocation_report
        .bodies
        .values()
        .filter(|body| body.emitted_name.contains("worker_int"))
        .collect::<Vec<_>>();
    assert!(
        coroutine_bodies
            .iter()
            .any(|body| body.emitted_name.ends_with("__step")),
        "the transformed coroutine step must be a reported body: {coroutine_bodies:#?}",
    );
    assert!(
        coroutine_bodies
            .iter()
            .any(|body| body.emitted_name.contains("__defer")),
        "the detached defer must be a reported body: {coroutine_bodies:#?}",
    );
    assert!(
        coroutine_bodies
            .iter()
            .any(|body| body.emitted_name.contains("__on_killed")),
        "the detached lifecycle hook must be a reported body: {coroutine_bodies:#?}",
    );

    let sites = coroutine_bodies
        .iter()
        .flat_map(|body| body.direct_sites.iter())
        .filter(|site| site.kind.as_str() != "opaque-call")
        .collect::<Vec<_>>();
    assert!(sites.len() >= 3, "step, defer, and hook sites must be visible: {sites:#?}");
    for site in sites {
        assert!(
            emit.lua.contains(&format!("(__rock_alloc_site[\"{:016x}\"] and ", site.id)),
            "coroutine site {:016x} must share its ID with runtime activation",
            site.id,
        );
    }

    let resumes = emit
        .allocation_report
        .lifecycle_routes
        .iter()
        .find(|route| route.name == "coroutine.resume")
        .expect("coroutine resume route");
    assert!(
        resumes.roots.iter().any(|root| root.ends_with("__step")),
        "transformed steps are the resume roots: {resumes:#?}",
    );
    assert!(
        resumes.findings.iter().any(|finding| {
            finding.body.ends_with("__step") && finding.site.kind.as_str() == "string-concat"
        }),
        "the resume route must reach allocations in transformed steps: {resumes:#?}",
    );

    let route = emit
        .allocation_report
        .trace_from_diagnostic_names("manual.make", &["make_worker"]);
    assert!(
        route
            .opaque_boundaries
            .iter()
            .any(|finding| finding.site.detail.as_deref().is_some_and(|detail| detail.contains("worker"))),
        "constructing a coroutine must remain an explicit opaque boundary rather than falsely tracing a future resume",
    );
}

fn wide_value_fixture(fields: usize, pads: usize) -> String {
    // Bind the complete value in step to retain a wide call for the keeper.
    let pad_params = (0..pads).map(|i| format!(", p{i}: int")).collect::<String>();
    let pad_sum = (0..pads).map(|i| format!(" + p{i}")).collect::<String>();
    let decl = (0..fields).map(|i| format!("f{i}: int")).collect::<Vec<_>>().join(", ");
    let init = (0..fields).map(|i| format!("f{i}: seed + {i}")).collect::<Vec<_>>().join(", ");
    format!(
        r#"
nobox value Wide {{ {decl} }}

fn make_wide(seed: int) -> Wide {{
    return Wide {{ {init} }}
}}

fn step(value: Wide, tag: [int]) -> Wide {{
    let w = value
    return make_wide(w.f0 + tag.len())
}}

fn run(seed: int{pad_params}) -> int {{
    let a = make_wide(seed)
    let b = step(a, [seed])
    let c = step(b, [seed, 1])
    return a.f1 + b.f2 + c.f3{pad_sum}
}}
"#
    )
}

/// The largest `stackslots` over every prototype in `lua`, or LuaJIT's load
/// error. The chunk is compiled, never run.
fn max_stack_slots(lua: &str, tag: &str) -> Result<u32, String> {
    use std::process::Command;
    let path = std::env::temp_dir().join(format!("skull_slots_{}_{tag}.lua", std::process::id()));
    std::fs::write(&path, lua).expect("write chunk");
    let probe = r#"
local f, err = loadfile(arg[1])
if not f then io.write("LOAD ", err) os.exit(0) end
local util = require("jit.util")
local best = 0
local function walk(fn)
  local slots = util.funcinfo(fn).stackslots
  if slots > best then best = slots end
  local i = -1
  while true do
    local k = util.funck(fn, i)
    if k == nil then break end
    if type(k) == "proto" then walk(k) end
    i = i - 1
  end
end
walk(f)
io.write("SLOTS ", best)
"#;
    let probe_path = std::env::temp_dir().join(format!("skull_slots_{}_{tag}_probe.lua", std::process::id()));
    std::fs::write(&probe_path, probe).expect("write probe");
    let out = Command::new("luajit").arg(&probe_path).arg(&path).output().expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&probe_path);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "probe failed: {stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    if let Some(slots) = stdout.strip_prefix("SLOTS ") {
        Ok(slots.parse().expect("slot count"))
    } else {
        Err(stdout)
    }
}

#[test]
fn allocation_activation_adds_no_register_slots_to_a_wide_frame() {
    // 60 nobox lanes live twice across a call that passes them plus a vec
    // literal, padded with live parameters until the ordinary frame is the
    // widest LuaJIT loads.
    let _inline = InlineOffGuard::on();
    let source = wide_value_fixture(60, 123);
    let ordinary = emit_mir(&source);
    let instrumented = {
        let _activation = AllocationActivationGuard::on();
        emit_mir(&source)
    };
    assert_eq!(
        max_stack_slots(&ordinary.lua, "ordinary"),
        Ok(249),
        "the fixture must sit exactly at LJ_MAX_SLOTS - 1 to guard the activated build",
    );
    assert_eq!(
        max_stack_slots(&instrumented.lua, "activated"),
        Ok(249),
        "allocation-site counters must not widen any emitted frame",
    );
}
