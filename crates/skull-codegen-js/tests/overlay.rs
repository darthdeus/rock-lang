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
        skull_codegen_js::JS_CALL_HELPER
    ));
    assert_eq!(output.trim(), "true");
}

#[test]
fn programs_without_definition_roles_do_not_stage_an_empty_plan() {
    let ws = ws_with("fn main() {}\n");
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let program = skull_codegen_js::emit::emit_program(&ws, mid).expect("emit program").program;
    assert!(!program.contains("__rock_definition_plan_stage("));
    let output = node(&format!("var __G = {{}};\n{}\nconsole.log('PASS');", program));
    assert_eq!(output.trim(), "PASS");
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
        "var __G = {{}};\n{}\n__G.STATE = 99;\n{}\nconsole.log(__G.STATE + ':' + hot());",
        program.program, overlay
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
        "var __G = {{}};\n{}\n{}\nconsole.log(hot());",
        program.program, overlay
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

    assert!(!overlay.contains("__G.STATE ="));
    let output = node(&format!(
        "var __G = {{}};\n{}\n__G.STATE = 99;\n{}\nconsole.log(__G.STATE + ':' + hot());",
        program.program, overlay
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
        skull_codegen_js::JS_CALL_HELPER, program.program, overlay
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
               -1\n\
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
        skull_codegen_js::JS_CALL_HELPER, program.program, overlay
    ));
    assert_eq!(output.trim(), "1\n12");
    assert!(overlay.contains("__rock_code_slot_stage_one"));

    let (fresh, _) = skull_codegen_js::emit::emit_program_cached(&after, mid).unwrap();
    let fresh_output = node(&format!(
        "{}\nvar __G = {{}};\n{}\nstart();\nconsole.log(invoke());",
        skull_codegen_js::JS_CALL_HELPER, fresh.program
    ));
    assert_eq!(fresh_output.trim(), "11");
}

#[test]
fn failed_overlay_after_late_top_level_work_keeps_retained_code_ref_old() {
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
        "{}\nvar __G = {{}};\n{}\nstart();\ntry {{ (0, eval)({:?}); }} catch (e) {{ console.log(false); }}\nconsole.log(__rock_code_slot_stage === null);\nconsole.log(__rock_call(__G.CALLBACK));",
        skull_codegen_js::JS_CALL_HELPER,
        program.program,
        overlay
    ));
    assert_eq!(output.trim(), "false\ntrue\n0");
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
        "var __G = {{}};\n{}\nconsole.log(first() + ':' + second());",
        overlay
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
        "var __G = {{}};\n{}\nhot(); hot();\n{}\nconsole.log(hot());",
        program.program, overlay
    ));
    assert_eq!(output.trim(), "11");
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
         __rock_call(__G.SYSTEMS._data[0].func);\n\
         (0, eval)({:?});\n\
         __rock_call(__G.SYSTEMS._data[0].func);\n\
         console.log(output.join(','));",
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_CALL_HELPER,
        program.program,
        overlay
    ));
    assert_eq!(output.trim(), "one,two");
}
