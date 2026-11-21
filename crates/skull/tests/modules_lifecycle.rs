use crate::common::{emit_v1_package, module_id, run_luajit, run_main, ws_v1_package};

const MAIN: &str = r#"
use facade::tick as first;
use facade::tick as second;

@lifecycle(phase="update", owner="process")
deco init tick {}

@first
@second
fn run() {}

fn main() {}
"#;

const FACADE: &str = r#"
mod facade;

pub use ::tick;
"#;

#[test]
fn v1_lifecycle_aliases_register_one_private_hook() {
    let emitted = emit_v1_package(
        "v1lifecycle",
        &[("main.rock", MAIN), ("facade.rock", FACADE)],
    );
    assert_eq!(emitted.lifecycle_plan.entries.len(), 1);
    let out = run_luajit(&format!(
        "{}\nlocal p = _G.__ROCK_LIFECYCLE_PLAN.update; print(p.len)\n",
        emitted.lua
    ));
    assert_eq!(out, "1");
}

#[test]
fn v1_decorator_named_test_is_not_a_builtin_test() {
    let fragments = [
        (
            "main.rock",
            r#"
use facade::test;

@test
fn custom_test_name() {}

fn main() {}

deco init test {}
"#,
        ),
        (
            "facade.rock",
            r#"
mod facade;

pub use ::test;
"#,
        ),
    ];
    let ws = ws_v1_package("v1custom", &fragments);
    let mid = module_id(&ws, "main.rock");
    assert!(
        skull::module_tests(&ws, mid)
            .expect("test discovery")
            .is_empty()
    );

    let emitted = emit_v1_package("v1custom", &fragments);
    let decorator = skull_mir::decorator::decorator_symbol("v1custom::test");
    assert!(emitted.lua.contains(&format!("{decorator}(")));
}

#[test]
fn v1_wire_still_rejects_a_namespaced_ordinary_function_dependency() {
    let fragments = [(
        "main.rock",
        r#"
fn helper() -> int { 3 }
const DEP: int = helper()
@lifecycle(phase="wire", owner="process")
deco init wire {}
@wire(onchange=[DEP])
fn apply() {}
fn main() {}
"#,
    )];
    let ws = ws_v1_package("wire_functions", &fragments);
    let error = skull_mir::lifecycle::collect(&ws)
        .expect_err("ordinary functions are not const identities");
    assert!(error.message.contains("wire_functions::helper"));
}
