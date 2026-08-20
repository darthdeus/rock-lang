mod common;

use common::{emit_mir, run_luajit};

#[test]
fn emitted_lua_artifact_has_a_total_escaping_function_census() {
    let emit = emit_mir(
        r#"
fn named(x: int) -> int { return x }

fn named_value() -> fn(int) -> int { return named }

fn capture_value() -> fn() -> int {
    let captured = 3
    return () => captured
}

fn free_value() -> fn() -> int { return () => 7 }

fn main() {
    let f = named_value()
    let a = capture_value()
    let b = free_value()
    print("{f(1)} {a()} {b()}")
}

@nomangle fn direct_target(x: int) -> int { return x + 1 }
@nomangle fn direct_call() -> int { return direct_target(1) }
"#,
    );
    let census = &emit.code_artifact.census;
    assert!(census.examined > 0, "the fixture must expose escaping function values: {census:?}");
    assert!(census.named_values > 0, "the named-value population must be nonzero: {census:?}");
    assert!(census.lambda_values >= 2, "both lambda populations must be nonzero: {census:?}");
    assert!(census.capturing_lambdas > 0, "the capturing lambda population must be nonzero: {census:?}");
    assert!(census.capture_free_lambdas > 0, "the capture-free lambda population must be nonzero: {census:?}");
    let direct_body = emit
        .lua
        .split("function direct_call()")
        .nth(1)
        .and_then(|body| body.split("\nend").next())
        .expect("direct call body");
    assert!(direct_body.contains("direct_target("), "direct calls retain their direct target: {direct_body}");
    assert!(!direct_body.contains("__rock_code_slot_ref"), "direct calls must not pay the slot indirection: {direct_body}");
    assert!(census.is_total(), "emitted artifact census is not self-validating: {census:?}");
}

#[test]
fn generated_decorator_function_values_use_logical_slots() {
    let emit = emit_mir(
        r#"
global SYSTEMS: [fn() -> ()] = []
deco init system { SYSTEMS.push(transmute(target.func)) }
@system
fn alpha() { }
fn main() { }
"#,
    );
    let application = emit
        .lua
        .lines()
        .find(|line| line.contains("__deco_system({"))
        .expect("decorator application");
    assert!(
        application.contains("__rock_code_slot_ref(\"named:<test>::alpha<>\")"),
        "generated decorator target must be a CodeRef: {application}"
    );
    assert!(
        !application.contains("func = alpha_ret_unit"),
        "generated decorator target must not freeze the backend body: {application}"
    );
}

#[test]
fn lifecycle_metadata_emits_a_sorted_plan_without_executing_the_decorator() {
    let emit = emit_mir(r#"
@lifecycle(phase="update", owner="session", stage_param="stage")
deco init tick(stage: int = 100) {}
@tick(stage=7) fn first() {}
@tick(stage=7) fn second() {}
@tick fn defaulted() {}
fn main() {}
"#);
    assert!(!emit.lua.contains("function __deco_tick("), "lifecycle declaration bodies are not executable registration code");
    assert!(!emit.lua.contains("__deco_tick({"), "lifecycle applications must not execute decorator side effects");
    let first = emit.lua.find("name = \"first\", stage = 7").expect("first plan entry");
    let second = emit.lua.find("name = \"second\", stage = 7").expect("second plan entry");
    let defaulted = emit.lua.find("name = \"defaulted\", stage = 100").expect("defaulted plan entry");
    assert!(first < second && second < defaulted, "compiler plan must preserve equal-stage declaration order and read the declared default");
    let out = run_luajit(
        &format!("{}\nlocal p = _G.__ROCK_LIFECYCLE_PLAN.update; print(p.len .. ':' .. p._data[1].func.key)", emit.lua),
        "lifecycle_plan_root",
    );
    assert!(out.starts_with("3:named:<test>::first<>"), "plan targets must be admitted CodeRef roots: {out}");
}

#[test]
fn artifact_definition_metadata_emits_a_keyed_plan_without_running_the_decorator() {
    let emit = emit_mir(r#"
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@widget(key="alpha") fn make_alpha() -> int { return 1 }
@widget(key="beta") fn make_beta() -> int { return 2 }
fn main() {}
"#);
    assert!(!emit.lua.contains("function __deco_widget("), "definition roles are compiler-owned");
    assert!(!emit.lua.contains("__deco_widget({"), "definition applications do not execute the decorator");
    assert!(emit.lua.contains("__rock_definition_plan_stage({"));
    let alpha = emit.lua.find("key = \"alpha\"").expect("alpha plan entry");
    let beta = emit.lua.find("key = \"beta\"").expect("beta plan entry");
    assert!(alpha < beta, "definition order is declaration order");
    assert!(emit.lua.contains("factory = __rock_code_slot_pending_ref(\"named:<test>::make_alpha<>\")"));
    assert!(emit.lua.contains("factory = __rock_code_slot_pending_ref(\"named:<test>::make_beta<>\")"));
    let out = run_luajit(
        &format!(
            "{}\nlocal p = _G.__ROCK_DEFINITION_PLAN.widgets; print(p.len .. ':' .. p._data[1].key .. ':' .. p._data[1].factory())",
            emit.lua
        ),
        "definition_plan_root",
    );
    assert_eq!(out, "2:alpha:1");
}

#[test]
fn typed_catalog_accessor_is_compiler_owned_and_returns_the_published_view() {
    let emit = emit_mir(r#"
struct WidgetDef { value: int }
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@artifact_catalog(catalog="widgets")
@intrinsic extern fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
@widget(key="alpha") fn make_alpha() -> WidgetDef { return WidgetDef { value: 1 } }
fn main() {}
"#);
    assert!(emit.lua.contains("function widget_definition_plan()"));
    assert!(emit.lua.contains("return __rock_definition_catalog(\"widgets\")"));
    assert!(!emit.lua.contains("__ROCK_DEFINITION_PLAN or {}"));
    let out = run_luajit(
        &format!(
            "{}\nlocal p = widget_definition_plan(); print(p.len .. ':' .. p._data[1].key .. ':' .. p._data[1].definition.value)",
            emit.lua
        ),
        "typed_definition_catalog_root",
    );
    assert_eq!(out, "1:alpha:1");
}

#[test]
fn shadow_candidate_typed_accessor_reads_the_candidate_definition_plan() {
    let emit = emit_mir(r#"
struct WidgetDef { value: int }
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@artifact_catalog(catalog="widgets")
@intrinsic extern fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
@widget(key="live") fn make_live() -> WidgetDef { return WidgetDef { value: 1 } }
fn main() {}
"#);
    let accessor = "function widget_definition_plan()\n  return __rock_definition_catalog(\"widgets\")\nend";
    assert!(emit.lua.contains(accessor), "compiler did not emit the typed accessor");
    let candidate_chunk = format!("{}\n{}", skull_codegen_lua::FN_MT_PRELUDE, accessor);
    let script = format!(
        r#"
local live_plan = _G.__ROCK_DEFINITION_PLAN
local candidate = setmetatable({{}}, {{ __index = _G }})
candidate._G = candidate
local chunk = assert(loadstring([[
{}
]]))
setfenv(chunk, candidate)
chunk()
candidate.__ROCK_DEFINITION_PLAN = {{
  widgets = {{ len = 1, _data = {{ [1] = {{ key = "candidate", definition = {{ value = 2 }} }} }} }}
}}
local plan = candidate.widget_definition_plan()
assert(plan._data[1].key == "candidate", "candidate accessor observed the live catalog")
assert(_G.__ROCK_DEFINITION_PLAN == live_plan, "candidate accessor changed the live owner")
print("candidate:" .. plan.len .. ":" .. plan._data[1].key)
"#,
        candidate_chunk
    );
    assert_eq!(run_luajit(&script, "shadow_candidate_typed_catalog"), "candidate:1:candidate");
}

#[test]
fn full_publication_replaces_lifecycle_membership_by_logical_key() {
    let v0 = emit_mir(r#"
@lifecycle(phase="update", owner="session") deco init tick {}
@tick fn removed() {}
@tick fn retained() {}
fn main() {}
"#);
    let v1 = emit_mir(r#"
@lifecycle(phase="update", owner="session") deco init tick {}
@tick fn retained() {}
@tick fn added() {}
fn main() {}
"#);
    let script = format!(r#"
{}
local old = _G.__ROCK_LIFECYCLE_PLAN.update
_G.HOT_RELOAD = true
{}
local new = _G.__ROCK_LIFECYCLE_PLAN.update
print(old.len .. ":" .. new.len .. ":" .. old._data[1].name .. ":" .. new._data[1].name .. ":" .. new._data[2].name)
"#, v0.lua, v1.lua);
    let out = run_luajit(&script, "lifecycle_membership");
    assert_eq!(out, "2:2:removed:retained:added");
}
