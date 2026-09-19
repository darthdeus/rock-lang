//! Modules-v1 lifecycle discovery keeps private hooks discoverable and treats
//! aliases/re-exports as one decorator identity.

use crate::common::{emit_v1_package, run_luajit, ws_v1_package, V1Fragment};
use ustr::Ustr;

const MAIN: &str = r#"
use facade::tick as first;
use facade::tick as second;

@lifecycle(phase="update", owner="session")
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
        &[
            V1Fragment { file: "main.rock", src: MAIN },
            V1Fragment { file: "facade.rock", src: FACADE },
        ],
    );
    assert_eq!(emitted.lifecycle_plan.entries.len(), 1);
    let out = run_luajit(
        &format!(
            "{}\nlocal p = _G.__ROCK_LIFECYCLE_PLAN.update; print(p.len)\n",
            emitted.lua
        ),
        "modules_v1_lifecycle",
    );
    assert_eq!(out, "1");
}

#[test]
fn v1_decorator_named_test_is_not_a_builtin_test() {
    let fragments = [
        V1Fragment {
            file: "main.rock",
            src: r#"
use facade::test;

@test
fn custom_test_name() {}

fn main() {}

deco init test {}
"#,
        },
        V1Fragment {
            file: "facade.rock",
            src: r#"
mod facade;

pub use ::test;
"#,
        },
    ];
    let ws = ws_v1_package("v1custom", &fragments);
    let mid = *ws.module_ids.get(&Ustr::from("main.rock")).expect("module id");
    assert!(skull::module_tests(&ws, mid).expect("test discovery").is_empty());

    let emitted = emit_v1_package("v1custom", &fragments);
    let decorator = skull::decorator_fn_name("v1custom::test");
    assert!(emitted.lua.contains(&format!("{decorator}(")));
}

#[test]
fn v1_wire_alias_resolves_its_const_by_definition_key() {
    let fragments = [
        V1Fragment {
            file: "main.rock",
            src: r#"
use facade::wire as hook;

const DEP: int = 1

@lifecycle(phase="wire", owner="session")
deco init wire {}

@hook(onchange=[DEP])
fn apply() {}

fn main() {}
"#,
        },
        V1Fragment {
            file: "facade.rock",
            src: r#"
mod facade;

pub use ::wire;
"#,
        },
    ];
    let ws = ws_v1_package("v1wire", &fragments);
    let plan = skull_mir::lifecycle::collect(&ws).expect("lifecycle plan");
    assert_eq!(plan.entries.len(), 1);
    assert_eq!(plan.entries[0].qualification.dependencies().len(), 1);
}
