use bone_semantic::hir::BodyOwner;
use skull_mir::mir::FuncRef;
use crate::common::ws_with;

fn function(ws: &bone_semantic::inference::Workspace, name: &str) -> FuncRef {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, function)| function.name.as_str() == name)
        .expect("function");
    FuncRef {
        module: mid,
        func: fid,
    }
}

fn global(
    ws: &bone_semantic::inference::Workspace,
    name: &str,
) -> (bone_semantic::prelude::ModuleId, bone_semantic::prelude::Idx<bone_semantic::hir::GlobalDef>)
{
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let (gid, _) = ws
        .module(mid)
        .globals
        .iter()
        .find(|(_, global)| global.name.as_str() == name)
        .expect("global");
    (mid, gid)
}

fn node(source: &str) -> String {
    let output = crate::common::node()
        .args(["-e", source])
        .output()
        .expect("node");
    assert!(
        output.status.success(),
        "node failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn js_runtime() -> String {
    format!("{}\n{}", skull_codegen_js::JS_GLOBALS_RUNTIME, skull_codegen_js::JS_CALL_HELPER)
}

#[test]
fn lifecycle_selector_schedules_only_changed_gated_entries_and_plain_entries() {
    let source = format!(
        "{}\nvar __G = {{}};\n{}",
        js_runtime(),
        r#"
var oldPlan = {wire: {len: 4, _data: [
  {key: "plain", qualification: "plain", body_fingerprint: "a", dependencies: {len: 0, _data: []}},
  {key: "stable", qualification: "onchange", body_fingerprint: "b", dependencies: {len: 1, _data: [{key: "DEP", fingerprint: "1"}]}},
  {key: "body", qualification: "onchange", body_fingerprint: "old", dependencies: {len: 1, _data: [{key: "DEP", fingerprint: "1"}]}},
  {key: "dependency", qualification: "onchange", body_fingerprint: "d", dependencies: {len: 1, _data: [{key: "DEP", fingerprint: "1"}]}}
]}};
var candidate = {wire: {len: 5, _data: [
  {key: "plain", qualification: "plain", body_fingerprint: "changed", dependencies: {len: 0, _data: []}},
  {key: "stable", qualification: "onchange", body_fingerprint: "b", dependencies: {len: 1, _data: [{key: "DEP", fingerprint: "1"}]}},
  {key: "body", qualification: "onchange", body_fingerprint: "new", dependencies: {len: 1, _data: [{key: "DEP", fingerprint: "1"}]}},
  {key: "dependency", qualification: "onchange", body_fingerprint: "d", dependencies: {len: 1, _data: [{key: "DEP", fingerprint: "2"}]}},
  {key: "added", qualification: "onchange", body_fingerprint: "e", dependencies: {len: 1, _data: [{key: "OTHER", fingerprint: "1"}]}}
]}};
var due = __rock_lifecycle_wire_due(oldPlan, candidate);
if (!(due.plain && !due.stable && due.body && due.dependency && due.added)) throw new Error("selector changed set");
if (__rock_lifecycle_take_wire("plain") !== false) throw new Error("take read an unset due map");
__G.__ROCK_WIRE_DUE = due;
if (!__rock_lifecycle_take_wire("plain") || __rock_lifecycle_take_wire("plain")) throw new Error("take was not exactly once");
__G.__ROCK_LIFECYCLE_PLAN = candidate;
__rock_lifecycle_schedule_all_wires();
if (!(__rock_lifecycle_take_wire("stable") && __rock_lifecycle_take_wire("added"))) throw new Error("fresh scheduling missed an entry");
console.log("PASS");
"#
    );
    let output = node(&source);
    assert_eq!(output.trim(), "PASS");
}

#[test]
fn dependency_gated_wire_overlay_selects_const_closure_and_calls_runtime_once() {
    let source = |base: i32, direct: i32, unrelated: i32| {
        format!(
            r#"
@lifecycle(phase="wire", owner="session") deco init wire {{}}
const BASE: int = {base}
const DERIVED: int = BASE + 1
const DIRECT: int = {direct}
global PLAIN_RUNS: int = 0
global DIRECT_RUNS: int = 0
global TRANSITIVE_RUNS: int = 0
global PLAIN_VALUE: int = 0
global DIRECT_VALUE: int = 0
global TRANSITIVE_VALUE: int = 0
global DRIVER_RUNS: int = 0
fn unrelated() -> int {{ return {unrelated} }}
@wire fn plain_wire() {{ PLAIN_RUNS = PLAIN_RUNS + 1; PLAIN_VALUE = unrelated() }}
@wire(onchange=[DIRECT]) fn direct_wire() {{ DIRECT_RUNS = DIRECT_RUNS + 1; DIRECT_VALUE = DIRECT }}
@wire(onchange=[DERIVED]) fn transitive_wire() {{ TRANSITIVE_RUNS = TRANSITIVE_RUNS + 1; TRANSITIVE_VALUE = DERIVED }}
@nomangle fn run_wire_systems() {{ DRIVER_RUNS = DRIVER_RUNS + 1 }}
fn main() {{ run_wire_systems() }}
"#
        )
    };
    let before = ws_with(&source(1, 10, 7));
    let unrelated = ws_with(&source(1, 10, 8));
    let direct = ws_with(&source(1, 11, 8));
    let transitive = ws_with(&source(4, 11, 8));
    let mid = function(&before, "main").module;
    let (program, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid)
        .expect("initial JS program");
    let unrelated_fn = function(&unrelated, "unrelated");
    let direct_global = global(&direct, "DIRECT");
    let base_global = global(&transitive, "BASE");
    let overlay_unrelated = skull_codegen_js::emit::emit_overlay(
        &unrelated,
        mid,
        &mut cache,
        &[(unrelated_fn.module, BodyOwner::Function(unrelated_fn.func))],
    )
    .expect("unrelated overlay")
    .expect_overlay();
    let overlay_direct = skull_codegen_js::emit::emit_overlay(
        &direct,
        mid,
        &mut cache,
        &[(direct_global.0, BodyOwner::Global(direct_global.1))],
    )
    .expect("direct overlay")
    .expect_overlay();
    let overlay_transitive = skull_codegen_js::emit::emit_overlay(
        &transitive,
        mid,
        &mut cache,
        &[(base_global.0, BodyOwner::Global(base_global.1))],
    )
    .expect("transitive overlay")
    .expect_overlay();
    for overlay in [&overlay_unrelated, &overlay_direct, &overlay_transitive] {
        assert_eq!(
            overlay.matches("run_wire_systems();").count(),
            1,
            "a JS overlay must have exactly one runtime wire caller"
        );
    }
    let driver = r#"
function run_wires() {
  var phase = __G.__ROCK_LIFECYCLE_PLAN.wire;
  for (var i = 0; i < phase.len; i++) {
    var entry = phase._data[i];
    if (__rock_lifecycle_take_wire(entry.key)) __rock_call(entry.func);
  }
}
function state(name) { return __rock_globals["global:<test>::" + name + "::value"]; }
run_wires();
console.log("initial:" + state("PLAIN_RUNS") + ":" + state("DIRECT_RUNS") + ":" + state("TRANSITIVE_RUNS") + ":" + state("DIRECT_VALUE") + ":" + state("TRANSITIVE_VALUE"));
var driver_before = state("DRIVER_RUNS");
(0, eval)(OVERLAY_UNRELATED);
run_wires();
console.log("unrelated:" + state("PLAIN_RUNS") + ":" + state("DIRECT_RUNS") + ":" + state("TRANSITIVE_RUNS") + ":" + state("PLAIN_VALUE") + ":" + (state("DRIVER_RUNS") - driver_before));
driver_before = state("DRIVER_RUNS");
(0, eval)(OVERLAY_DIRECT);
run_wires();
console.log("direct:" + state("PLAIN_RUNS") + ":" + state("DIRECT_RUNS") + ":" + state("TRANSITIVE_RUNS") + ":" + state("DIRECT_VALUE") + ":" + (state("DRIVER_RUNS") - driver_before));
driver_before = state("DRIVER_RUNS");
(0, eval)(OVERLAY_TRANSITIVE);
run_wires();
console.log("transitive:" + state("PLAIN_RUNS") + ":" + state("DIRECT_RUNS") + ":" + state("TRANSITIVE_RUNS") + ":" + state("TRANSITIVE_VALUE") + ":" + (state("DRIVER_RUNS") - driver_before));
"#;
    let source = format!(
        "{}\nvar __G = {{}};\n{}\nvar OVERLAY_UNRELATED = {:?};\nvar OVERLAY_DIRECT = {:?};\nvar OVERLAY_TRANSITIVE = {:?};\n{}",
        js_runtime(), program.program, overlay_unrelated, overlay_direct, overlay_transitive, driver
    );
    let output = node(&source);
    assert_eq!(
        output.trim(),
        "initial:1:1:1:10:2\nunrelated:2:1:1:8:1\ndirect:3:2:1:11:1\ntransitive:4:2:2:5:1"
    );
}

#[test]
fn definition_plan_publishes_and_aborts_with_the_code_slot_transaction() {
    let output = node(&format!(
        "{}\nvar __G = {{}};\n\
         __rock_code_slot_begin();\n\
         __rock_code_slot_stage_one(\"probe\", function() {{ return 1; }}, \"abi\");\n\
         __rock_definition_plan_stage({{panels: {{len: 1, _data: [{{key: \"old\"}}]}}}});\n\
         __rock_code_slot_publish();\n\
         var oldPlan = __G.__ROCK_DEFINITION_PLAN;\n\
         var ok = false;\n\
         try {{\n\
           __rock_code_slot_begin();\n\
           __rock_definition_plan_stage({{panels: {{len: 2, _data: []}}}});\n\
           __rock_code_slot_stage_one(\"probe\", function() {{ return 2; }}, \"changed-abi\");\n\
           __rock_code_slot_publish();\n\
         }} catch (e) {{ ok = true; }}\n\
         __rock_code_slot_abort();\n\
         console.log(ok && __rock_definition_plan_pending === null\n\
           && __G.__ROCK_DEFINITION_PLAN === oldPlan\n\
           && oldPlan.panels.len === 1\n\
           && __rock_code_slot_ref(\"probe\").current_body() === 1);",
        js_runtime()
    ));
    assert_eq!(output.trim(), "true");
}

#[test]
fn failed_definition_materialization_keeps_the_old_plan_and_clears_pending() {
    let source = format!("{}\n", js_runtime()) + r#"
var __G = {};
__rock_code_slot_begin();
__rock_code_slot_stage_one("probe", function() { return 1; }, "abi");
__rock_definition_plan_stage({widgets: {len: 1, _data: [{key: "old", factory: function() { return {key: "old"}; }}]}});
__rock_code_slot_publish();
var oldPlan = __G.__ROCK_DEFINITION_PLAN;
var ok = false;
try {
  __rock_code_slot_begin();
  __rock_definition_plan_stage({widgets: {len: 1, _data: [{key: "new", factory: function() { throw new Error("materialization failed"); }}]}});
  __rock_code_slot_publish();
} catch (e) { ok = true; }
__rock_code_slot_abort();
console.log(ok && __rock_definition_plan_pending === null
  && __G.__ROCK_DEFINITION_PLAN === oldPlan
  && oldPlan.widgets._data[0].definition.key === "old"
  && __rock_code_slot_ref("probe").current_body() === 1);
"#;
    let output = node(&source);
    assert_eq!(output.trim(), "true");
}

#[test]
fn programs_without_definition_roles_do_not_stage_an_empty_plan() {
    let ws = ws_with("fn main() {}\n");
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let program = skull_codegen_js::emit::emit_program(&ws, mid).expect("emit program").program;
    assert!(!program.contains("__rock_definition_plan_stage("));
    let output = node(&format!("{}\nvar __G = {{}};\n{}\nconsole.log('PASS');", js_runtime(), program));
    assert_eq!(output.trim(), "PASS");
}

#[test]
fn typed_definition_catalog_body_edits_overlay_but_membership_edits_require_full() {
    let source = |key: &str, value: i32| {
        format!(
            r#"
struct WidgetDef {{ value: int }}
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {{}}
@artifact_catalog(catalog="widgets")
@intrinsic extern fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
@widget(key="{key}") fn make_alpha() -> WidgetDef {{ return WidgetDef {{ value: {value} }} }}
fn main() {{}}
"#
        )
    };
    let before = ws_with(&source("alpha", 1));
    let body = ws_with(&source("alpha", 2));
    let membership = ws_with(&source("beta", 2));
    let mid = function(&before, "main").module;
    let (_, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid)
        .expect("initial typed catalog program");
    let changed = function(&body, "make_alpha");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &body,
        mid,
        &mut cache,
        &[(changed.module, BodyOwner::Function(changed.func))],
    )
    .expect("typed catalog body overlay")
    .expect_overlay();
    assert!(!overlay.contains("__rock_definition_plan_stage("));

    let (_, mut structural_cache) = skull_codegen_js::emit::emit_program_cached(&before, mid)
        .expect("structural typed catalog program");
    let changed = function(&membership, "make_alpha");
    assert!(matches!(
        skull_codegen_js::emit::emit_overlay(
            &membership,
            mid,
            &mut structural_cache,
            &[(changed.module, BodyOwner::Function(changed.func))],
        )
        .expect("typed catalog membership check"),
        skull_codegen_js::emit::JsReload::FullRequired
    ));
}


#[test]
fn const_value_overlay_redefines_users_without_reinitializing_state() {
    let before = ws_with(
        "const WISP_COUNT: int = 5\n\
         global STATE: int = 7\n\
         @nomangle fn hot() -> int { WISP_COUNT }\n\
         fn main() { print(hot()) }\n",
    );
    let after = ws_with(
        "const WISP_COUNT: int = 10\n\
         global STATE: int = 7\n\
         @nomangle fn hot() -> int { WISP_COUNT }\n\
         fn main() { print(hot()) }\n",
    );
    let mid = function(&before, "main").module;
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let (global_mid, global_id) = global(&after, "WISP_COUNT");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(global_mid, BodyOwner::Global(global_id))],
    )
    .unwrap()
    .expect_overlay();

    let output = node(&format!(
        "{}\nvar __G = {{}};\n{}\n__rock_globals[\"global:<test>::STATE::value\"] = 99;\n{}\nconsole.log(__rock_globals[\"global:<test>::STATE::value\"] + ':' + hot());",
        js_runtime(), program.program, overlay
    ));
    assert_eq!(output.trim(), "99:10");
}

#[test]
fn const_value_overlay_reinitializes_dependent_constants() {
    let before = ws_with(
        "const BASE: int = 5\n\
         const COUNT: int = BASE + 1\n\
         @nomangle fn hot() -> int { COUNT }\n\
         fn main() { print(hot()) }\n",
    );
    let after = ws_with(
        "const BASE: int = 10\n\
         const COUNT: int = BASE + 1\n\
         @nomangle fn hot() -> int { COUNT }\n\
         fn main() { print(hot()) }\n",
    );
    let mid = function(&before, "main").module;
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let (global_mid, global_id) = global(&after, "BASE");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(global_mid, BodyOwner::Global(global_id))],
    )
    .unwrap()
    .expect_overlay();

    let output = node(&format!(
        "{}\nvar __G = {{}};\n{}\n{}\nconsole.log(hot());",
        js_runtime(), program.program, overlay
    ));
    assert_eq!(output.trim(), "11");
}

#[test]
fn overlay_redefines_a_function_without_reinitializing_global_state() {
    let before = ws_with(
        "global STATE: int = 7\n\
         @nomangle fn hot() -> int { 1 }\n\
         fn main() { print(hot()) }\n",
    );
    let after = ws_with(
        "global STATE: int = 7\n\
         @nomangle fn hot() -> int { 2 }\n\
         fn main() { print(hot()) }\n",
    );
    let mid = function(&before, "main").module;
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let hot = function(&after, "hot");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(hot.module, BodyOwner::Function(hot.func))],
    )
    .unwrap()
    .expect_overlay();

    assert!(!overlay.contains("__rock_globals[\"global:<test>::STATE::value\"] = "));
    let output = node(&format!(
        "{}\n{}\n__rock_globals[\"global:<test>::STATE::value\"] = 99;\n{}\nconsole.log(__rock_globals[\"global:<test>::STATE::value\"] + ':' + hot());",
        js_runtime(), program.program, overlay
    ));
    assert_eq!(output.trim(), "99:2");
}

#[test]
fn retained_named_and_capture_free_values_refresh_through_slots() {
    let source = |named_delta: i32, free_value: i32| {
        format!(
            "global NAMED: (fn(int) -> int)? = null\n\
             global FREE: (fn() -> int)? = null\n\
             @nomangle fn named(x: int) -> int {{ return x + {named_delta} }}\n\
             @nomangle fn make_free() -> fn() -> int {{ return () => {free_value} }}\n\
             @nomangle fn start() {{\n\
               NAMED = named\n\
               FREE = make_free()\n\
             }}\n\
             @nomangle fn invoke() -> int {{\n\
               let a = NAMED\n\
               let b = FREE\n\
               if a != null {{ if b != null {{ return a(3) + b() }} }}\n\
               return -1\n\
             }}\n\
             fn main() {{ start(); invoke() }}\n"
        )
    };
    let before = ws_with(&source(1, 10));
    let after = ws_with(&source(2, 20));
    let mid = function(&before, "main").module;
    let (program, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let named = function(&after, "named");
    let free = function(&after, "make_free");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[
            (named.module, BodyOwner::Function(named.func)),
            (free.module, BodyOwner::Function(free.func)),
        ],
    )
    .unwrap()
    .expect_overlay();
    let output = node(&format!(
        "{}\nvar __G = {{}};\n{}\nstart();\nconsole.log(invoke());\n(0, eval)({:?});\nconsole.log(invoke());",
        js_runtime(), program.program, overlay
    ));
    assert_eq!(output.trim(), "14\n25");
    assert!(overlay.contains("__rock_code_slot_stage_one"));
}

#[test]
fn retained_capturing_value_refreshes_while_keeping_mutable_capture() {
    let source = |tail: i32| {
        format!(
            "global CALLBACK: (fn() -> int)? = null\n\
             @nomangle fn make() -> fn() -> int {{\n\
               let n = 0\n\
               return () => {{ n = n + 1 n + {tail} }}\n\
             }}\n\
             @nomangle fn start() {{ CALLBACK = make() }}\n\
             @nomangle fn invoke() -> int {{\n\
               let f = CALLBACK\n\
               if f != null {{ return f() }}\n\
               return -1\n\
             }}\n\
             fn main() {{ start(); invoke() }}\n"
        )
    };
    let before = ws_with(&source(0));
    let after = ws_with(&source(10));
    let mid = function(&before, "main").module;
    let (program, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let make = function(&after, "make");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(make.module, BodyOwner::Function(make.func))],
    )
    .unwrap()
    .expect_overlay();
    let output = node(&format!(
        "{}\nvar __G = {{}};\n{}\nstart();\nconsole.log(invoke());\n(0, eval)({:?});\nconsole.log(invoke());",
        js_runtime(), program.program, overlay
    ));
    assert_eq!(output.trim(), "1\n12");
    assert!(overlay.contains("__rock_code_slot_stage_one"));

    let (fresh, _) = skull_codegen_js::emit::emit_program_cached(&after, mid).unwrap();
    let fresh_output = node(&format!(
        "{}\nvar __G = {{}};\n{}\nstart();\nconsole.log(invoke());",
        js_runtime(), fresh.program
    ));
    assert_eq!(fresh_output.trim(), "11");
}

#[test]
fn wire_failure_after_publication_keeps_the_candidate_code_ref_live() {
    let source = |delta: i32, wire: &str| {
        format!(
            "global CALLBACK: (fn() -> int)? = null\n\
             @nomangle fn make() -> fn() -> int {{ return () => {delta} }}\n\
             @nomangle fn start() {{ CALLBACK = make() }}\n\
             @nomangle fn run_wire_systems() {{ {wire} }}\n\
             fn main() {{ start(); run_wire_systems() }}\n"
        )
    };
    let before = ws_with(&source(0, ""));
    let after = ws_with(&source(10, "error(\"late overlay failure\")"));
    let mid = function(&before, "main").module;
    let (program, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let make = function(&after, "make");
    let wire = function(&after, "run_wire_systems");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(make.module, BodyOwner::Function(make.func)), (wire.module, BodyOwner::Function(wire.func))],
    )
    .unwrap()
    .expect_overlay();
    assert!(overlay.contains("__rock_code_slot_stage_one"));
    assert!(overlay.contains("run_wire_systems();"), "overlay must contain the later top-level hook:\n{overlay}");

    let output = node(&format!(
        "{}\nvar __G = {{}};\n{}\nstart();\ntry {{ (0, eval)({:?}); }} catch (e) {{ console.log(false); }}\nconsole.log(__rock_code_slot_stage === null);\nconsole.log(__rock_call(__rock_globals[\"global:<test>::CALLBACK::value\"]));",
        js_runtime(),
        program.program,
        overlay
    ));
    assert_eq!(output.trim(), "false\ntrue\n10");
}

#[test]
fn successive_overlays_accumulate_changed_function_definitions() {
    let v0 = ws_with(
        "@nomangle fn first() -> int { 1 }\n\
         @nomangle fn second() -> int { 10 }\n\
         fn main() { print(first() + second()) }\n",
    );
    let v1 = ws_with(
        "@nomangle fn first() -> int { 2 }\n\
         @nomangle fn second() -> int { 10 }\n\
         fn main() { print(first() + second()) }\n",
    );
    let v2 = ws_with(
        "@nomangle fn first() -> int { 2 }\n\
         @nomangle fn second() -> int { 20 }\n\
         fn main() { print(first() + second()) }\n",
    );
    let mid = function(&v0, "main").module;
    let (_, mut cache) = skull_codegen_js::emit::emit_program_cached(&v0, mid).unwrap();
    let first = function(&v1, "first");
    skull_codegen_js::emit::emit_overlay(
        &v1,
        mid,
        &mut cache,
        &[(first.module, BodyOwner::Function(first.func))],
    )
    .unwrap()
    .expect_overlay();
    let second = function(&v2, "second");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &v2,
        mid,
        &mut cache,
        &[(second.module, BodyOwner::Function(second.func))],
    )
    .unwrap()
    .expect_overlay();

    let output = node(&format!(
        "{}\n{}\nconsole.log(first() + ':' + second());",
        js_runtime(), overlay
    ));
    assert_eq!(output.trim(), "2:20");
}

#[test]
fn changed_static_initializer_resets_only_its_live_cell() {
    let before = ws_with(
        "@nomangle fn hot() -> int {\n\
             static count = 1\n\
             count += 1\n\
             count\n\
         }\n\
         fn main() { print(hot()) }\n",
    );
    let after = ws_with(
        "@nomangle fn hot() -> int {\n\
             static count = 10\n\
             count += 1\n\
             count\n\
         }\n\
         fn main() { print(hot()) }\n",
    );
    let mid = function(&before, "main").module;
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let hot = function(&after, "hot");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(hot.module, BodyOwner::Function(hot.func))],
    )
    .unwrap()
    .expect_overlay();

    let output = node(&format!(
        "{}\n{}\nhot(); hot();\n{}\nconsole.log(hot());",
        js_runtime(), program.program, overlay
    ));
    assert_eq!(output.trim(), "11");
}

#[test]
fn a_body_edit_that_removes_a_static_drops_its_cells() {
    let before = ws_with(
        "@nomangle fn hot() -> int {\n\
             static count = 1\n\
             count += 1\n\
             count\n\
         }\n\
         fn main() { print(hot()) }\n",
    );
    let after = ws_with("@nomangle fn hot() -> int { 7 }\nfn main() { print(hot()) }\n");
    let mid = function(&before, "main").module;
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let hot = function(&after, "hot");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(hot.module, BodyOwner::Function(hot.func))],
    )
    .unwrap()
    .expect_overlay();

    let output = node(&format!(
        "{}\n{}\nhot();\n\
         function cells() {{ return Object.keys(__rock_globals).filter(k => k.includes('::count#')).length; }}\n\
         let live = cells();\n{}\nconsole.log(live + ':' + cells() + ':' + hot());",
        js_runtime(), program.program, overlay
    ));
    assert_eq!(output.trim(), "2:0:7");
}

#[test]
fn decorated_function_overlay_refreshes_the_registered_code_ref() {
    let source = |message: &str| {
        format!(
            "struct System {{ func: fn() -> (), name: string }}\n\
             global SYSTEMS: [System] = []\n\
             deco init system {{\n\
                 SYSTEMS.push(System {{ func: transmute(target.func), name: transmute(target.name) }})\n\
             }}\n\
             @system\n\
             @nomangle fn hot() {{ print(\"{message}\") }}\n\
             fn main() {{ hot() }}\n"
        )
    };
    let before = ws_with(&source("one"));
    let after = ws_with(&source("two"));
    let mid = function(&before, "main").module;
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let hot = function(&after, "hot");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(hot.module, BodyOwner::Function(hot.func))],
    )
    .unwrap()
    .expect_overlay();
    assert!(!overlay.contains("__rock_rebind_system"), "CodeRef overlays must not emit a registry repair");

    let output = node(&format!(
        "var output = [];\n\
         function print(value) {{ output.push(value); }}\n\
         {}\n\
         {}\n\
         var __G = {{}};\n\
         {}\n\
         __rock_call(__rock_globals[\"global:<test>::SYSTEMS::value\"]._data[0].func);\n\
         (0, eval)({:?});\n\
         __rock_call(__rock_globals[\"global:<test>::SYSTEMS::value\"]._data[0].func);\n\
         console.log(output.join(','));",
        skull_codegen_js::JS_TABLE_PRIM,
        js_runtime(),
        program.program,
        overlay
    ));
    assert_eq!(output.trim(), "one,two");
}

#[test]
fn unrelated_body_overlay_restages_unchanged_generic_callback_with_concrete_abi() {
    let source = "fn apply<T>(value: T, callback: fn(T) -> T) -> T { callback(value) }\n\
        fn unchanged(value: int) -> int { value + 3 }\n\
        @nomangle fn invoke() -> int { apply(7, unchanged) }\n\
        @nomangle fn edited() -> int { 1 }\n\
        fn main() { invoke(); edited() }\n";
    let before = ws_with(source);
    let after = ws_with(&source.replace("fn edited() -> int { 1 }", "fn edited() -> int { 2 }"));
    let mid = function(&before, "main").module;
    let (program, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let caller = function(&after, "edited");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after, mid, &mut cache,
        &[(caller.module, BodyOwner::Function(caller.func))],
    ).unwrap().expect_overlay();
    let staged: Vec<_> = overlay.lines().filter(|line|
        line.contains("__rock_code_slot_stage_one(") && line.contains("unchanged")
    ).collect();
    assert_eq!(staged.len(), 1, "unchanged callback must be staged: {staged:?}");
    assert!(!staged[0].contains("unresolved-named-value"), "stage must carry the concrete ABI");
    let output = node(&format!(
        "{}\nvar __G = {{}};\n{}\nconsole.log(invoke(), edited());\n(0, eval)({:?});\nconsole.log(invoke(), edited());",
        js_runtime(), program.program, overlay
    ));
    assert_eq!(output.trim(), "10 1\n10 2");
}
