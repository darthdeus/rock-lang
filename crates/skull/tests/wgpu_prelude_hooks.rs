//! The Lua prelude reaches the renderer's pending WGPU shader queue only
//! through `@nomangle` hooks, so a modules-v1 host engine, whose globals and plain
//! functions have hashed physical names, is published exactly like a legacy
//! one. A registrar without its publish hook fails the load instead of
//! skipping the publication.

use crate::common::{V1Fragment, emit_module, emit_v1_package, run_luajit};

const HOOKS: &str = r#"
@nomangle fn __rock_wgpu_shader_register(key: string, source: string, abi_wire: string, semantic_wire: string) {}

@nomangle fn __rock_wgpu_shader_publish_pending() -> bool {
    print("publish")
    return false
}

@nomangle fn __rock_wgpu_shader_discard_pending() {
    print("discard")
}

@lifecycle(phase="update", owner="process")
deco init tick {}

@tick
fn run() {}

fn main() {}
"#;

const REGISTRAR_ONLY: &str = r#"
@nomangle fn __rock_wgpu_shader_register(key: string, source: string, abi_wire: string, semantic_wire: string) {}

@lifecycle(phase="update", owner="process")
deco init tick {}

@tick
fn run() {}

fn main() {}
"#;

const NO_WGPU: &str = r#"
@lifecycle(phase="update", owner="process")
deco init tick {}

@tick
fn run() {}

fn main() {}
"#;

// Loads the chunk the way a reload does: the chunk's own code-slot
// publication runs the WGPU hooks, and a failed load is followed by the abort.
fn load_then_abort(lua: &str, tag: &str) -> String {
    assert!(
        lua.contains("__rock_code_slot_publish()"),
        "the fixture must publish code slots"
    );
    run_luajit(
        &format!(
            "local chunk = assert(loadstring([=====[{lua}]=====]))\n\
             local ok, err = pcall(chunk)\n\
             print(tostring(ok) .. \" \" .. tostring(err))\n\
             ok, err = pcall(__rock_code_slot_abort)\n\
             print(tostring(ok) .. \" \" .. tostring(err))\n"
        ),
        tag,
    )
}

fn v1_lua(src: &str) -> String {
    emit_v1_package(
        "v1wgpu",
        &[V1Fragment {
            file: "main.rock",
            src,
        }],
    )
    .lua
}

#[test]
fn v1_publication_invokes_the_declared_wgpu_hooks() {
    assert_eq!(
        load_then_abort(&v1_lua(HOOKS), "wgpu_hooks_v1"),
        "publish\nfalse WGPU shader registration did not complete\ndiscard\ntrue nil"
    );
}

#[test]
fn legacy_publication_invokes_the_same_wgpu_hooks() {
    assert_eq!(
        load_then_abort(&emit_module(HOOKS), "wgpu_hooks_legacy"),
        "publish\nfalse WGPU shader registration did not complete\ndiscard\ntrue nil"
    );
}

#[test]
fn a_wgpu_registrar_without_its_publish_hook_fails_the_load() {
    assert_eq!(
        load_then_abort(&v1_lua(REGISTRAR_ONLY), "wgpu_hooks_missing"),
        "false WGPU shader registrar has no publish hook\nfalse WGPU shader registrar has no discard hook"
    );
}

#[test]
fn a_program_without_wgpu_publishes_normally() {
    assert_eq!(
        load_then_abort(&v1_lua(NO_WGPU), "wgpu_hooks_absent"),
        "true nil\ntrue nil"
    );
}
