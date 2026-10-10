//! End-to-end tests through the `rock stone-test` watcher.
//!
//! These exercise the full incremental-compile pipeline (real fs events,
//! real `unified_compile`, real on-disk Lua output) — catching divergence
//! between the in-process test path used by `hot_reload_runtime_test.rs`
//! and what the watcher actually produces in production.
//!
//! Slower than the in-process tests (each test spawns the rock binary +
//! a debouncer + LuaJIT). When this file's count grows past ~10, gate it
//! to nextest `-P full` like `game_snapshot_tests`.

use rock_host::lua_runner::{run_lua_code_with_timeout, LuaRun};
use std::time::Duration;
use crate::stone_harness::StoneHarness;

const QUIESCENT: Duration = Duration::from_millis(400);
// The event must arrive before the quiet-period clock starts. Allow a loaded
// host's compiler and watcher processes to run, with nextest as the outer cap.
const SETTLE_TIMEOUT: Duration = test_utils::HANG_BACKSTOP;

// `run_lua_code` kills the script after 1.8 s of wall time.
fn run_lua(script: &str) -> Result<LuaRun, rock_core::CompilerError> {
    run_lua_code_with_timeout(script, Some(test_utils::HANG_BACKSTOP))
}

/// Validates the global-type-change-forces-restart fix from `73e642336`
/// through the real watcher path. The unit tests in
/// `hot_reload_runtime_test::test_global_type_change_*` exercise the
/// snapshot helpers directly; this exercises them via the live
/// incremental compile pipeline that production hot reload uses.
#[test]
fn global_type_change_vec_to_arena_emits_restart_decision() {
    let h = StoneHarness::spawn(&[(
        "game.rock",
        "struct Corpse { name: string }\n\
         global CORPSES: [Corpse] = []\n\
         fn count() -> int {\n\
             let n = 0\n\
             for c in CORPSES { n = n + 1 }\n\
             return n\n\
         }\n",
    )]);

    // Initial seed produced one fresh-compile reload. Wait for it to land
    // before mutating, otherwise the rewrite could coalesce with the seed
    // and we'd never see the type change as a separate reload event.
    h.await_reload_count(1, SETTLE_TIMEOUT);
    let baseline = h.event_count();

    h.write_file(
        "game.rock",
        "struct Corpse { name: string }\n\
         global CORPSES: Arena<Corpse> = arena_new()\n\
         fn count() -> int {\n\
             let n = 0\n\
             for (e, c) in CORPSES { n = n + 1 }\n\
             return n\n\
         }\n",
    );
    let log = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
    assert!(
        log.len() >= 2,
        "expected at least 2 reloads (seed + type change), got: {:?}",
        log,
    );
    let type_change = log.last().unwrap();
    assert_eq!(
        type_change.decision, "restart",
        "global type change must trigger restart, got: {:?}",
        type_change,
    );
}

/// Cross-checks the watcher's incremental compile against a fresh compile
/// by running the resulting Lua under LuaJIT in a hot-reload simulation.
/// If incremental and fresh ever diverge in emitted code, the assertion
/// inside the script catches it.
#[test]
fn incremental_recompile_lua_runs_correctly_under_hot_reload() {
    let h = StoneHarness::spawn(&[(
        "a.rock",
        "global SCORE: int = 0\n@nomangle fn bump() { SCORE = SCORE + 1 }\n",
    )]);
    // Settle before reading `before_lua` so a phantom seed-file event
    // (the OS can deliver one for the seed write right after the watcher
    // subscribes, especially under workspace-wide nextest load) lands
    // *before* we snapshot, not in the middle of the update window.
    h.await_settled(QUIESCENT, SETTLE_TIMEOUT);
    let before_lua = h.compiled_lua();

    let baseline = h.event_count();
    h.write_file(
        "a.rock",
        "global SCORE: int = 0\n@nomangle fn bump() { SCORE = SCORE + 10 }\n",
    );
    let log = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
    assert!(
        log.len() >= 2,
        "expected at least 2 reloads (seed + body change), got: {:?}",
        log,
    );
    // A body edit emits a skull `incremental.lua` overlay (the engine
    // light-reloads it on top of the unchanged `main.lua`); that overlay is
    // the "after" chunk the hot-reload simulation loads below.
    let after_lua = h.overlay_lua();
    assert!(
        !after_lua.trim().is_empty(),
        "body change should have produced an incremental.lua overlay",
    );

    // Same hot-reload simulation pattern as hot_reload_runtime_test.rs:
    // run before_lua, flip HOT_RELOAD, run after_lua, assert SCORE
    // accumulates the new function body.
    let script = format!(
        "_G.HOT_RELOAD = false\n\
         {before}\n\
         bump()\n\
         _G.HOT_RELOAD = true\n\
         {after}\n\
         _G.HOT_RELOAD = false\n\
         bump()\n\
         if _G.SCORE == 11 then\n\
             print(\"PASS\")\n\
         else\n\
             print(\"FAIL: SCORE=\" .. tostring(_G.SCORE))\n\
             os.exit(1)\n\
         end\n",
        before = before_lua,
        after = after_lua,
    );

    let out = run_lua(&script).unwrap_or_else(|e| {
        panic!("LuaJIT failed on watcher output:\n{}", e);
    });
    assert!(
        out.success,
        "LuaJIT exited non-zero on the watcher's output:\n{}",
        out.combined(),
    );
    assert!(
        out.stdout.trim().starts_with("PASS"),
        "watcher's incremental Lua output diverges from expected runtime behavior:\n{}",
        out.combined(),
    );
}

/// Sanity: a non-type-changing edit hot-reloads (decision: "hot"), not
/// restart. Sibling assertion to the vec→arena restart test.
#[test]
fn function_body_change_keeps_hot_reload_decision() {
    let h = StoneHarness::spawn(&[(
        "a.rock",
        "global SCORE: int = 0\n@nomangle fn bump() { SCORE = SCORE + 1 }\n",
    )]);
    h.await_reload_count(1, SETTLE_TIMEOUT);

    let baseline = h.event_count();
    h.write_file(
        "a.rock",
        "global SCORE: int = 0\n@nomangle fn bump() { SCORE = SCORE + 99 }\n",
    );
    let log = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
    assert!(log.len() >= 2);
    assert_eq!(
        log.last().unwrap().decision,
        "hot",
        "function body change must hot-reload, not restart: {:?}",
        log,
    );
}

/// The watcher harness exercises the same bound overlay as rock stone.

#[test]
fn a_body_edit_reaches_its_caller_even_with_callee_binding_requested() {
    let h = StoneHarness::spawn_with_env(
        &[(
            "a.rock",
            "global SCORE: int = 0\n\
             fn leaf(a: int) -> int { a + 1 }\n\
             @nomangle fn bump() { SCORE = leaf(SCORE) }\n",
        )],
        &[("ROCK_SKULL_CALLEE_BINDING", "upvalue")],
    );
    h.await_settled(QUIESCENT, SETTLE_TIMEOUT);
    let before_lua = h.compiled_lua();
    assert!(
        before_lua.contains("__rock_mk_bump"),
        "rock stone-test must retain the requested bound shape",
    );

    let baseline = h.event_count();
    h.write_file(
        "a.rock",
        "global SCORE: int = 0\n\
         fn leaf(a: int) -> int { a + 10 }\n\
         @nomangle fn bump() { SCORE = leaf(SCORE) }\n",
    );
    let log = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
    assert!(
        log.len() >= 2,
        "expected at least 2 reloads (seed + body change), got: {log:?}",
    );
    let after_lua = h.overlay_lua();
    assert!(
        !after_lua.trim().is_empty(),
        "body change should have produced an incremental.lua overlay",
    );

    let script = format!(
        "_G.HOT_RELOAD = false\n\
         {before}\n\
         bump()\n\
         _G.HOT_RELOAD = true\n\
         {after}\n\
         _G.HOT_RELOAD = false\n\
         bump()\n\
         if _G.SCORE == 11 then\n\
             print(\"PASS\")\n\
         else\n\
             print(\"FAIL: SCORE=\" .. tostring(_G.SCORE))\n\
             os.exit(1)\n\
         end\n",
        before = before_lua,
        after = after_lua,
    );
    let out = run_lua(&script).unwrap_or_else(|e| {
        panic!("LuaJIT failed on watcher output:\n{}", e);
    });
    assert!(
        out.stdout.trim().starts_with("PASS"),
        "a body edit must reach its caller through the overlay:\n{}",
        out.combined(),
    );
}

#[test]
fn caller_body_edit_preserves_cross_module_generic_callback_abi() {
    let initial = "global SCORE: int = 0\n\
        fn apply<T>(value: T, callback: fn(T) -> T) -> T { callback(value) }\n\
        @nomangle fn bump() { SCORE = SCORE + 1 }\n\
        @nomangle fn invoke() -> int { apply(7, unchanged) }\n";
    let h = StoneHarness::spawn(&[
        ("stick.toml", "[package]\nname = \"callbackwatch\"\n"),
        ("caller.rock", initial),
        ("callback.rock", "fn unchanged(value: int) -> int { value + 3 }\n"),
    ]);
    h.await_settled(QUIESCENT, SETTLE_TIMEOUT);
    let before = h.compiled_lua();
    let baseline = h.event_count();
    h.write_file("caller.rock", &initial.replace("apply(7, unchanged)", "apply(17, unchanged)"));
    let log = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
    assert_eq!(log.last().expect("reload event").decision, "hot");
    let after = h.overlay_lua();
    assert!(!after.trim().is_empty(), "body edit must emit an overlay");
    let script = format!(
        "_G.HOT_RELOAD = false\n{before}\n\
         assert(invoke() == 10)\n\
         bump()\n\
         _G.HOT_RELOAD = true\n{after}\n\
         _G.HOT_RELOAD = false\n\
         assert(invoke() == 20)\n\
         bump()\n\
         assert(_G.SCORE == 2)\n\
         print('PASS')\n"
    );
    let out = run_lua(&script).expect("run watcher output under LuaJIT");
    assert!(out.success && out.stdout.trim() == "PASS",
        "a caller edit must apply without changing the unchanged callback ABI:\n{}", out.combined());
}
