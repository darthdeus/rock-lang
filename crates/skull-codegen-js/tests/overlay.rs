use bone_semantic::hir::BodyOwner;
use skull_mir::lower::ScalarCtx;
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
    let output = std::process::Command::new("node")
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
    let before_scx = ScalarCtx::new(&before).with_target(bone_semantic::hir::FnBackend::Js);
    let after_scx = ScalarCtx::new(&after).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = function(&before, "main").module;
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, &before_scx, mid).unwrap();
    let (global_mid, global_id) = global(&after, "WISP_COUNT");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        &after_scx,
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
    let before_scx = ScalarCtx::new(&before).with_target(bone_semantic::hir::FnBackend::Js);
    let after_scx = ScalarCtx::new(&after).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = function(&before, "main").module;
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, &before_scx, mid).unwrap();
    let (global_mid, global_id) = global(&after, "BASE");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        &after_scx,
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
    let before_scx = ScalarCtx::new(&before).with_target(bone_semantic::hir::FnBackend::Js);
    let after_scx = ScalarCtx::new(&after).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = function(&before, "main").module;
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, &before_scx, mid).unwrap();
    let hot = function(&after, "hot");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        &after_scx,
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
    let scx0 = ScalarCtx::new(&v0).with_target(bone_semantic::hir::FnBackend::Js);
    let scx1 = ScalarCtx::new(&v1).with_target(bone_semantic::hir::FnBackend::Js);
    let scx2 = ScalarCtx::new(&v2).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = function(&v0, "main").module;
    let (_, mut cache) = skull_codegen_js::emit::emit_program_cached(&v0, &scx0, mid).unwrap();
    let first = function(&v1, "first");
    skull_codegen_js::emit::emit_overlay(
        &v1,
        &scx1,
        mid,
        &mut cache,
        &[(first.module, BodyOwner::Function(first.func))],
    )
    .unwrap()
    .expect_overlay();
    let second = function(&v2, "second");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &v2,
        &scx2,
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
    let before_scx = ScalarCtx::new(&before).with_target(bone_semantic::hir::FnBackend::Js);
    let after_scx = ScalarCtx::new(&after).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = function(&before, "main").module;
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, &before_scx, mid).unwrap();
    let hot = function(&after, "hot");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        &after_scx,
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
fn decorated_function_overlay_rebinds_the_registered_function_value() {
    let source = |message: &str| {
        format!(
            "struct System {{ func: fn() -> (), name: string }}\n\
             global SYSTEMS: [System] = []\n\
             deco init system {{\n\
                 SYSTEMS.push(System {{ func: transmute(target.func), name: transmute(target.name) }})\n\
             }}\n\
             @nomangle inline(js) fn __rock_rebind_system(name: string, f: any) = \"\"\"\n\
                 for (var i = 0; i < __G.SYSTEMS.len; i++) {{\n\
                     if (__G.SYSTEMS._data[i].name === name) __G.SYSTEMS._data[i].func = f;\n\
                 }}\n\
             \"\"\"\n\
             @system\n\
             @nomangle fn hot() {{ print(\"{message}\") }}\n\
             fn main() {{ hot() }}\n"
        )
    };
    let before = ws_with(&source("one"));
    let after = ws_with(&source("two"));
    let before_scx = ScalarCtx::new(&before).with_target(bone_semantic::hir::FnBackend::Js);
    let after_scx = ScalarCtx::new(&after).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = function(&before, "main").module;
    let (program, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, &before_scx, mid).unwrap();
    let hot = function(&after, "hot");
    let overlay = skull_codegen_js::emit::emit_overlay(
        &after,
        &after_scx,
        mid,
        &mut cache,
        &[(hot.module, BodyOwner::Function(hot.func))],
    )
    .unwrap()
    .expect_overlay();

    let output = node(&format!(
        "var output = [];\n\
         function print(value) {{ output.push(value); }}\n\
         {}\n\
         var __G = {{}};\n\
         {}\n\
         __G.SYSTEMS._data[0].func();\n\
         (0, eval)({:?});\n\
         __G.SYSTEMS._data[0].func();\n\
         console.log(output.join(','));",
        skull_codegen_js::JS_TABLE_PRIM,
        program.program,
        overlay
    ));
    assert_eq!(output.trim(), "one,two");
}
