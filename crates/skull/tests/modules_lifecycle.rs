//! Modules-v1 lifecycle discovery keeps private hooks discoverable and treats
//! aliases/re-exports as one decorator identity.

use crate::common::{emit_v1_package, run_luajit, ws_v1_package, V1Fragment};
use ustr::Ustr;

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

@lifecycle(phase="wire", owner="process")
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

#[test]
fn v1_wire_const_arithmetic_admits_a_namespaced_operator_family() {
    let fragments = [
        V1Fragment { file: "main.rock", src: r#"
use geometry::*;
const BASE: int = 2
const DEP: int = BASE + 1
@lifecycle(phase="wire", owner="process")
deco init wire {}
@wire(onchange=[DEP])
fn apply() {}
fn main() { print(DEP) }
"# },
        V1Fragment { file: "geometry.rock", src: r#"
mod geometry;
pub struct Point { x: int }
pub operator +(a: Point, b: Point) -> Point { Point { x: a.x + b.x } }
"# },
    ];
    let emitted = emit_v1_package("wire_ops", &fragments);
    assert_eq!(emitted.lifecycle_plan.entries.len(), 1);
    assert_eq!(emitted.lifecycle_plan.entries[0].qualification.dependencies().len(), 2);
    assert_eq!(run_luajit(&format!("{}\nmain()\n", emitted.lua), "wire_module_operator"), "3");
}

#[test]
fn v1_wire_still_rejects_a_namespaced_ordinary_function_dependency() {
    let fragments = [V1Fragment { file: "main.rock", src: r#"
fn helper() -> int { 3 }
const DEP: int = helper()
@lifecycle(phase="wire", owner="process")
deco init wire {}
@wire(onchange=[DEP])
fn apply() {}
fn main() {}
"# }];
    let ws = ws_v1_package("wire_functions", &fragments);
    let error = skull_mir::lifecycle::collect(&ws).expect_err("ordinary functions are not const identities");
    assert!(error.message.contains("wire_functions::helper"), "{}", error.message);
}

/// Only a decorator claims an annotation spelling. A v1 fn that shares a
/// builtin's name (host engine defines `fn shader`, `fn uniform`, `fn size`) must
/// leave the builtin in force rather than making it an unknown annotation.
#[test]
fn v1_fn_named_like_a_builtin_annotation_keeps_the_builtin() {
    let fragments = [V1Fragment {
        file: "main.rock",
        src: r#"
fn pure(x: int) -> int { return x }
fn test(x: int) -> int { return x }

@pure
fn twice(x: int) -> int { return x * 2 }

@test
fn doubles() { assert(twice(pure(test(3))) == 6, "doubled") }

fn main() {}
"#,
    }];
    let ws = ws_v1_package("v1builtin", &fragments);
    let mid = *ws.module_ids.get(&Ustr::from("main.rock")).expect("module id");
    let tests = skull::module_tests(&ws, mid).expect("test discovery");
    assert_eq!(tests.len(), 1, "builtin @test must still discover `doubles`");
}
