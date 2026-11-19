use crate::common::{overlay_of, replace_once, run_node, ws_with};

fn js_runtime() -> String {
    format!(
        "{}\n{}",
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        skull_codegen_js::JS_CALL_HELPER
    )
}

/// Run `program` after the globals and call runtimes, then `tail`.
fn run_live(program: &str, tail: &str) -> String {
    run_node(&format!(
        "{}\nvar __G = {{}};\n{program}\n{tail}",
        js_runtime()
    ))
    .trim()
    .to_string()
}

/// The full program for `before` and the overlay that moves it to `after`,
/// given the definitions the edit changed.
fn overlay_after_edit(before: &str, after: &str, edited: &[&str]) -> (String, String) {
    let before = ws_with(before);
    let mid = crate::common::test_mid(&before);
    let (program, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let overlay = overlay_of(&ws_with(after), mid, &mut cache, edited);
    (program.program, overlay)
}

#[test]
fn dependency_gated_wire_overlay_selects_const_closure_and_calls_runtime_once() {
    let source = |base: i32, direct: i32, unrelated: i32| {
        format!(
            r#"
@lifecycle(phase="wire", owner="process") deco init wire {{}}
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
    let mid = crate::common::test_mid(&before);
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, mid).expect("initial JS program");
    let overlays = [
        ((1, 10, 8), "unrelated"),
        ((1, 11, 8), "DIRECT"),
        ((4, 11, 8), "BASE"),
    ]
    .map(|((base, direct, unrelated), edited)| {
        let after = ws_with(&source(base, direct, unrelated));
        overlay_of(&after, mid, &mut cache, &[edited])
    });
    for overlay in &overlays {
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
    let [unrelated, direct, transitive] = &overlays;
    let output = run_live(
        &program.program,
        &format!(
            "var OVERLAY_UNRELATED = {unrelated:?};\nvar OVERLAY_DIRECT = {direct:?};\n\
             var OVERLAY_TRANSITIVE = {transitive:?};\n{driver}"
        ),
    );
    assert_eq!(
        output,
        "initial:1:1:1:10:2\nunrelated:2:1:1:8:1\ndirect:3:2:1:11:1\ntransitive:4:2:2:5:1"
    );
}

#[test]
fn failed_code_slot_publication_keeps_the_old_definition_plan() {
    for (initial_plan, failing_transaction, old_plan_intact) in [
        (
            r#"{panels: {len: 1, _data: [{key: "old"}]}}"#,
            r#"__rock_definition_plan_stage({panels: {len: 2, _data: []}});
  __rock_code_slot_stage_one("probe", function() { return 2; }, "changed-abi");"#,
            "oldPlan.panels.len === 1",
        ),
        (
            r#"{widgets: {len: 1, _data: [{key: "old", factory: function() { return {key: "old"}; }}]}}"#,
            r#"__rock_definition_plan_stage({widgets: {len: 1, _data: [{key: "new", factory: function() { throw new Error("materialization failed"); }}]}});"#,
            r#"oldPlan.widgets._data[0].definition.key === "old""#,
        ),
    ] {
        let output = run_live(
            "",
            &format!(
                r#"__rock_code_slot_begin();
__rock_code_slot_stage_one("probe", function() {{ return 1; }}, "abi");
__rock_definition_plan_stage({initial_plan});
__rock_code_slot_publish();
var oldPlan = __G.__ROCK_DEFINITION_PLAN;
var ok = false;
try {{
  __rock_code_slot_begin();
  {failing_transaction}
  __rock_code_slot_publish();
}} catch (e) {{ ok = true; }}
__rock_code_slot_abort();
console.log(ok && __rock_definition_plan_pending === null
  && __G.__ROCK_DEFINITION_PLAN === oldPlan
  && {old_plan_intact}
  && __rock_code_slot_ref("probe").current_body() === 1);"#
            ),
        );
        assert_eq!(output, "true", "{failing_transaction}");
    }
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
@intrinsic extern(lua) fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
@widget(key="{key}") fn make_alpha() -> WidgetDef {{ return WidgetDef {{ value: {value} }} }}
fn main() {{}}
"#
        )
    };
    let (_, overlay) =
        overlay_after_edit(&source("alpha", 1), &source("alpha", 2), &["make_alpha"]);
    assert!(!overlay.contains("__rock_definition_plan_stage("));

    let before = ws_with(&source("alpha", 1));
    let membership = ws_with(&source("beta", 2));
    let mid = crate::common::test_mid(&before);
    let (_, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    let reload = skull_codegen_js::emit::emit_overlay(
        &membership,
        mid,
        &mut cache,
        &[crate::common::test_owner(&membership, "make_alpha")],
    )
    .expect("typed catalog membership check");
    assert!(matches!(
        reload,
        skull_codegen_js::emit::JsReload::FullRequired
    ));
}

#[test]
fn edits_redefine_users_without_reinitializing_state() {
    const STATE: &str = "__rock_globals[\"global:<test>::STATE::value\"]";
    for (source, old, new, edited, expected) in [
        (
            "const WISP_COUNT: int = 5\n\
             global STATE: int = 7\n\
             @nomangle fn hot() -> int { WISP_COUNT }\n\
             fn main() { print(hot()) }\n",
            "WISP_COUNT: int = 5",
            "WISP_COUNT: int = 10",
            "WISP_COUNT",
            "99:10",
        ),
        (
            "const BASE: int = 5\n\
             const COUNT: int = BASE + 1\n\
             global STATE: int = 7\n\
             @nomangle fn hot() -> int { COUNT }\n\
             fn main() { print(hot()) }\n",
            "BASE: int = 5",
            "BASE: int = 10",
            "BASE",
            "99:11",
        ),
        (
            "global STATE: int = 7\n\
             @nomangle fn hot() -> int { 1 }\n\
             fn main() { print(hot()) }\n",
            "fn hot() -> int { 1 }",
            "fn hot() -> int { 2 }",
            "hot",
            "99:2",
        ),
    ] {
        let after = replace_once(source, old, new);
        let (program, overlay) = overlay_after_edit(source, &after, &[edited]);
        assert!(!overlay.contains(&format!("{STATE} = ")), "{overlay}");
        let output = run_live(
            &program,
            &format!("{STATE} = 99;\n{overlay}\nconsole.log({STATE} + ':' + hot());"),
        );
        assert_eq!(output, expected, "{edited}");
    }
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
    let (program, overlay) =
        overlay_after_edit(&source(1, 10), &source(2, 20), &["named", "make_free"]);
    let output = run_live(
        &program,
        &format!(
            "start();\nconsole.log(invoke());\n(0, eval)({overlay:?});\nconsole.log(invoke());"
        ),
    );
    assert_eq!(output, "14\n25");
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
    let (program, overlay) = overlay_after_edit(&source(0), &source(10), &["make"]);
    let output = run_live(
        &program,
        &format!(
            "start();\nconsole.log(invoke());\n(0, eval)({overlay:?});\nconsole.log(invoke());"
        ),
    );
    assert_eq!(output, "1\n12");
    assert!(overlay.contains("__rock_code_slot_stage_one"));

    let after = ws_with(&source(10));
    let fresh = skull_codegen_js::emit::emit_program(&after, crate::common::test_mid(&after))
        .unwrap()
        .program;
    assert_eq!(run_live(&fresh, "start();\nconsole.log(invoke());"), "11");
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
    let (program, overlay) = overlay_after_edit(
        &source(0, ""),
        &source(10, "error(\"late overlay failure\")"),
        &["make", "run_wire_systems"],
    );
    assert!(overlay.contains("__rock_code_slot_stage_one"));
    assert!(
        overlay.contains("run_wire_systems();"),
        "overlay must contain the later top-level hook:\n{overlay}"
    );
    let output = run_live(
        &program,
        &format!(
            "start();\ntry {{ (0, eval)({overlay:?}); }} catch (e) {{ console.log(false); }}\n\
             console.log(__rock_code_slot_stage === null);\n\
             console.log(__rock_call(__rock_globals[\"global:<test>::CALLBACK::value\"]));"
        ),
    );
    assert_eq!(output, "false\ntrue\n10");
}

#[test]
fn successive_overlays_accumulate_changed_function_definitions() {
    const SOURCE: &str = "@nomangle fn first() -> int { 1 }\n\
        @nomangle fn second() -> int { 10 }\n\
        fn main() { print(first() + second()) }\n";
    let v1_source = replace_once(SOURCE, "fn first() -> int { 1 }", "fn first() -> int { 2 }");
    let v2_source = replace_once(
        &v1_source,
        "fn second() -> int { 10 }",
        "fn second() -> int { 20 }",
    );
    let v0 = ws_with(SOURCE);
    let mid = crate::common::test_mid(&v0);
    let (_, mut cache) = skull_codegen_js::emit::emit_program_cached(&v0, mid).unwrap();
    overlay_of(&ws_with(&v1_source), mid, &mut cache, &["first"]);
    let overlay = overlay_of(&ws_with(&v2_source), mid, &mut cache, &["second"]);
    assert_eq!(
        run_live(&overlay, "console.log(first() + ':' + second());"),
        "2:20"
    );
}

#[test]
fn static_initializer_edits_reset_or_drop_only_their_cells() {
    const SOURCE: &str = "@nomangle fn hot() -> int {\n\
        static count = 1\n\
        count += 1\n\
        count\n\
    }\n\
    fn main() { print(hot()) }\n";
    let changed = replace_once(SOURCE, "static count = 1", "static count = 10");
    let (program, overlay) = overlay_after_edit(SOURCE, &changed, &["hot"]);
    assert_eq!(
        run_live(
            &program,
            &format!("hot(); hot();\n{overlay}\nconsole.log(hot());")
        ),
        "11"
    );

    let removed = "@nomangle fn hot() -> int { 7 }\nfn main() { print(hot()) }\n";
    let (program, overlay) = overlay_after_edit(SOURCE, removed, &["hot"]);
    let output = run_live(
        &program,
        &format!(
            "hot();\n\
             function cells() {{ return Object.keys(__rock_globals).filter(k => k.includes('::count#')).length; }}\n\
             let live = cells();\n{overlay}\nconsole.log(live + ':' + cells() + ':' + hot());"
        ),
    );
    assert_eq!(output, "2:0:7");
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
    let (program, overlay) = overlay_after_edit(&source("one"), &source("two"), &["hot"]);
    assert!(
        !overlay.contains("__rock_rebind_system"),
        "CodeRef overlays must not emit a registry repair"
    );
    let call = "__rock_call(__rock_globals[\"global:<test>::SYSTEMS::value\"]._data[0].func);";
    let output = run_live(
        &format!(
            "var output = [];\nfunction print(value) {{ output.push(value); }}\n{}\n{program}",
            skull_codegen_js::JS_TABLE_PRIM
        ),
        &format!("{call}\n(0, eval)({overlay:?});\n{call}\nconsole.log(output.join(','));"),
    );
    assert_eq!(output, "one,two");
}

#[test]
fn unrelated_body_overlay_restages_unchanged_generic_callback_with_concrete_abi() {
    let source = "fn apply<T>(value: T, callback: fn(T) -> T) -> T { callback(value) }\n\
        fn unchanged(value: int) -> int { value + 3 }\n\
        @nomangle fn invoke() -> int { apply(7, unchanged) }\n\
        @nomangle fn edited() -> int { 1 }\n\
        fn main() { invoke(); edited() }\n";
    let after = replace_once(
        source,
        "fn edited() -> int { 1 }",
        "fn edited() -> int { 2 }",
    );
    let (program, overlay) = overlay_after_edit(source, &after, &["edited"]);
    let staged: Vec<_> = overlay
        .lines()
        .filter(|line| line.contains("__rock_code_slot_stage_one(") && line.contains("unchanged"))
        .collect();
    assert_eq!(
        staged.len(),
        1,
        "unchanged callback must be staged: {staged:?}"
    );
    assert!(
        !staged[0].contains("unresolved-named-value"),
        "stage must carry the concrete ABI"
    );
    let output = run_live(
        &program,
        &format!(
            "console.log(invoke(), edited());\n(0, eval)({overlay:?});\nconsole.log(invoke(), edited());"
        ),
    );
    assert_eq!(output, "10 1\n10 2");
}

#[test]
fn static_resets_for_several_edited_functions_emit_in_a_stable_order() {
    let source = |init: u32| {
        let mut text = String::new();
        let mut calls = Vec::new();
        for i in 0..8 {
            text.push_str(&format!(
                "@nomangle fn hot{i}() -> int {{\n\
                     static count = {}\n\
                     count += 1\n\
                     count\n\
                 }}\n",
                init + i
            ));
            calls.push(format!("hot{i}()"));
        }
        text.push_str(&format!("fn main() {{ print({}) }}\n", calls.join(" + ")));
        text
    };
    let edited: Vec<String> = (0..8).map(|i| format!("hot{i}")).collect();
    let edited: Vec<&str> = edited.iter().map(String::as_str).collect();
    let overlay = || overlay_after_edit(&source(1), &source(100), &edited).1;
    let first = overlay();
    assert_eq!(first.matches("] = false;").count(), 8, "{first}");
    for _ in 0..16 {
        assert_eq!(overlay(), first);
    }
}
