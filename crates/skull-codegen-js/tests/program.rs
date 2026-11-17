use crate::common::{run_node, test_mid, try_ws_with_named, ws_with, ws_with_named};
use ustr::Ustr;

fn run_program(file: &str, source: &str, extra_runtime: &[&str], after_program: &str) -> String {
    let ws = ws_with_named(file, source);
    let mid = *ws
        .module_ids
        .get(&Ustr::from(file))
        .expect("fixture module");
    let emitted = skull_codegen_js::emit::emit_program(&ws, mid)
        .unwrap_or_else(|e| panic!("emit_program rejected: {}", e.message));
    let mut js = skull_codegen_js::assemble_prelude(
        &emitted.program,
        emitted.has_code_refs(),
        skull_codegen_js::JS_NODE_HOST,
        extra_runtime,
    );
    js.push_str(&emitted.program);
    if let Some(main) = emitted.main_js_name {
        js.push_str(&format!("\n{main}();\n"));
    }
    js.push_str(after_program);
    run_node(&js)
}

#[test]
fn unsupported_wire_dependencies_reject_on_both_backends() {
    for (annotation, expected) in [
        ("[MUTABLE]", "mutable"),
        ("[callable]", "names a function"),
        ("[UNKNOWN]", "`UNKNOWN` did not resolve to any definition"),
        ("[]", "requires a non-empty dependency list"),
        ("[GOOD.value]", "structured identifier list"),
    ] {
        let source = format!(
            r#"
            @lifecycle(phase="wire", owner="process") deco init wire {{}}
            const GOOD: int = 1
            global MUTABLE: int = 2
            fn callable() {{}}
            @wire(onchange={annotation}) fn apply_game_config() {{}}
            fn main() {{}}
            "#
        );
        let ws = match try_ws_with_named("<test>", &source) {
            Ok(ws) => ws,
            Err(error) => {
                assert!(error.message.contains(expected), "{annotation}: {error}");
                continue;
            }
        };
        let mid = test_mid(&ws);
        let lua = match skull::mir_emit::emit_full(&ws, mid) {
            Ok(_) => panic!("Lua accepted unsupported wire dependency {annotation}"),
            Err(error) => error.message,
        };
        let js = match skull_codegen_js::emit::emit_program(&ws, mid) {
            Ok(_) => panic!("JS accepted unsupported wire dependency {annotation}"),
            Err(error) => error.message,
        };
        assert!(lua.contains(expected), "Lua {annotation}: {lua}");
        assert!(js.contains(expected), "JS {annotation}: {js}");
    }
}

#[test]
fn duplicate_runtime_type_symbols_are_rejected_by_js_program_emission() {
    let ws = ws_with(
        r#"
enum Envelope { Empty, Text(string) }
fn main() {
    let follower = envelope(1.0, 2.0)
    let event = Envelope::Text("hello")
    print("{follower} {event}")
}
"#,
    );
    let error = match skull_codegen_js::emit::emit_program(&ws, test_mid(&ws)) {
        Ok(_) => panic!("duplicate runtime type symbols must reject"),
        Err(error) => error,
    };
    assert!(
        error.message.contains("would both emit as `Envelope`"),
        "{}",
        error.message
    );
}

#[test]
fn js_program_rejects_aggregate_map_keys_and_accepts_scalar_keys() {
    let invalid = ws_with(
        r#"
value VKey { x: int }
fn main() {
    let map: Map<VKey, string> = map_new()
    map.insert(VKey { x: 1 }, "value")
}
"#,
    );
    let error = match skull_codegen_js::emit::emit_program(&invalid, test_mid(&invalid)) {
        Ok(_) => panic!("value-struct map keys use identity and must reject"),
        Err(error) => error,
    };
    assert!(
        error.message.contains("`value` struct"),
        "{}",
        error.message
    );

    let valid = ws_with(
        r#"
fn main() {
    let map: Map<int, string> = map_new()
    map.insert(1, "value")
}
"#,
    );
    skull_codegen_js::emit::emit_program(&valid, test_mid(&valid))
        .expect("scalar Map keys remain supported");
}

#[test]
fn a_non_inlinable_call_result_runs_once_when_its_value_is_reused() {
    let ws = ws_with(
        r#"
inline(js) fn next_value() -> int = "return hits++;"
fn root() -> int {
    let value = next_value()
    return value * 10 + value
}
"#,
    );
    let js = crate::common::emit_closure(&ws, "root").expect("emit root");
    let output = run_node(&format!(
        "let hits = 4;\n{js}\nif (root() !== 44 || hits !== 5) throw new Error('call result was reevaluated');"
    ));
    assert!(output.is_empty(), "{output}");
}

#[test]
fn scene_lifecycle_callback_resolves_only_a_live_root() {
    let source = r#"
struct Room { ticks: int }
@lifecycle(phase="update", owner="scene") deco init scene_update {}
@scene_update fn scene_tick(root: Room) { root.ticks += 1 }
fn main() {}
"#;
    let extra_runtime = [
        "var room = { ticks: 0 };\nfunction __scene_root_any(id) { return id === 7 ? room : null; }",
    ];
    let after = r#"
const entries = __G.__ROCK_LIFECYCLE_PLAN.update._data;
const scene = entries.find(entry => entry.owner === "scene");
scene.func(7);
scene.func(8);
if (room.ticks !== 1) throw new Error("scene hook did not resolve its root");
"#;
    assert_eq!(run_program("scene.rock", source, &extra_runtime, after), "");
}

#[test]
fn sim_scoped_process_callback_runs_only_for_its_fixture_wire() {
    let source = r#"
@lifecycle(phase="update", owner="process") deco init update {}
fn register_sim(name: string, body: () -> ()) {}
@update fn observed_tick() { print("process") }
@sim fn selected_sim() {}
fn main() {
    let keep: () -> () = selected_sim
    keep()
}
"#;
    let after = r#"
const process_hook = __G.__ROCK_LIFECYCLE_PLAN.update._data.find(entry => entry.owner === "process");
__G.__ROCK_ACTIVE_SIM = "other";
process_hook.func(0);
__G.__ROCK_ACTIVE_SIM = "selected_sim";
process_hook.func(0);
"#;
    assert_eq!(
        run_program("tests/sims/scoped.rock", source, &[], after),
        "process\n"
    );
}

#[test]
fn definition_catalog_accessor_reads_the_published_materialized_plan() {
    let source = r#"
struct WidgetDef { value: int }
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@artifact_catalog(catalog="widgets")
@intrinsic extern(lua) fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
@widget(key="alpha") fn make_widget() -> WidgetDef { WidgetDef { value: 9 } }
fn main() {
    let entries = widget_definition_plan()
    print("{entries.len} {entries[0].key} {entries[0].definition.value}")
}
"#;
    assert_eq!(run_program("catalog.rock", source, &[], ""), "1 alpha 9\n");
}

#[test]
fn irreducible_mir_uses_a_trampoline_that_executes_the_return_path() {
    use skull_codegen_js::emit::emit_fn;
    use skull_mir::mir::{Inst, Lit, MirFunc, Term};

    let ws = ws_with("fn main() {}");
    let mut mf = MirFunc::new(ustr::Ustr::from("root"));
    let enter_right = mf.new_cell(None);
    let loop_again = mf.new_cell(None);
    mf.params = vec![enter_right, loop_again];
    let left = mf.new_block();
    let right = mf.new_block();
    let exit = mf.new_block();
    let route = mf.emit(mf.entry, Inst::Load(enter_right));
    mf.set_term(
        mf.entry,
        Term::Branch {
            cond: route,
            then_bb: right,
            else_bb: left,
        },
    );
    let again = mf.emit(left, Inst::Load(loop_again));
    mf.set_term(
        left,
        Term::Branch {
            cond: again,
            then_bb: right,
            else_bb: exit,
        },
    );
    let reset = mf.emit(right, Inst::Const(Lit::Bool(false)));
    mf.emit(
        right,
        Inst::Store {
            cell: loop_again,
            value: reset,
        },
    );
    mf.set_term(right, Term::Goto(left));
    let result = mf.emit(exit, Inst::Const(Lit::Int(42)));
    mf.set_term(exit, Term::Return(vec![result]));

    let js = emit_fn(&ws, &mf, "root").expect("irreducible MIR emits");
    run_node(&format!(
        "{js}\nfor (const enterRight of [false, true]) for (const loopAgain of [false, true]) if (root(enterRight, loopAgain) !== 42) throw new Error('wrong return');"
    ));
}
