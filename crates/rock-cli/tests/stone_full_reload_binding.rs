//! Bound-callee Stone body edits publish executable incremental overlays.

use rock_cli::game_runtime::{GameCompiler, SkullGameCompiler};
use rock_cli::skull_backend::{SkullReload, SkullStone};
use rock_host::lua_runner::{run_lua_code_with_timeout, LuaRun};
use std::collections::HashSet;
use std::path::PathBuf;

fn run_lua(script: &str) -> Result<LuaRun, rock_core::CompilerError> {
    run_lua_code_with_timeout(script, Some(test_utils::HANG_BACKSTOP))
}

/// No `fn main`: emission is reachability-driven from `@nomangle bump`, and a
/// chunk with no entry point runs standalone under LuaJIT below.
const V1: &str = "global SCORE: int = 0\n\
                  fn leaf(a: int) -> int { a + 1 }\n\
                  @nomangle fn bump() { SCORE = leaf(SCORE) }\n";
const V2: &str = "global SCORE: int = 0\n\
                  fn leaf(a: int) -> int { a + 10 }\n\
                  @nomangle fn bump() { SCORE = leaf(SCORE) }\n";

struct Fixture {
    _dir: tempfile::TempDir,
    dir: String,
    main: PathBuf,
    incremental: PathBuf,
    source: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("main.rock");
    std::fs::write(&source, V1).expect("seed v1");
    Fixture {
        dir: dir.path().to_str().expect("utf-8 tempdir").to_string(),
        main: dir.path().join("main.lua"),
        incremental: dir.path().join("incremental.lua"),
        source,
        _dir: dir,
    }
}

/// The emitted shape itself: the shipped default binds callees, so the chunk
/// carries factory definitions and a binding pass. Without this the rest of the
/// file would pass vacuously against the global-lookup shape.
#[test]
fn the_shipped_stone_build_binds_its_callees() {
    let f = fixture();
    SkullStone::init(&f.dir, &f.main, &f.incremental).expect("init v1");
    let main = std::fs::read_to_string(&f.main).expect("main.lua");
    assert!(
        main.contains("__rock_mk_bump"),
        "the shipped default must emit the bound shape for `rock stone`:\n{main}"
    );
}

/// Body edits publish only the overlay, preserving the boot chunk.
#[test]
fn a_body_edit_preserves_main_and_writes_an_overlay() {
    let f = fixture();
    let mut stone = SkullStone::init(&f.dir, &f.main, &f.incremental).expect("init v1");
    let main_v1 = std::fs::read_to_string(&f.main).expect("main.lua after init");
    assert!(!f.incremental.exists(), "init must leave no overlay");

    std::fs::write(&f.source, V2).expect("write v2");
    let reload = stone
        .recompile(&[f.source.clone()], &f.main, &f.incremental)
        .expect("recompile v2");
    assert!(
        matches!(reload, SkullReload::Incremental { .. }),
        "a body edit under the bound shape must publish an overlay, got {reload:?}"
    );
    assert!(
        f.incremental.exists(),
        "the bound shape must write an incremental.lua overlay"
    );
    let main_v2 = std::fs::read_to_string(&f.main).expect("main.lua after edit");
    assert_eq!(
        main_v1, main_v2,
        "an overlay must preserve main.lua"
    );
}

/// A body edit must preserve the running process and its live state.
#[test]
fn a_body_edit_does_not_request_a_process_restart() {
    let f = fixture();
    let mut compiler =
        SkullGameCompiler::new(&f.dir, f.main.clone(), f.incremental.clone());
    compiler
        .compile(&HashSet::new(), false)
        .expect("initial compile");

    std::fs::write(&f.source, V2).expect("write v2");
    let changed: HashSet<PathBuf> = [f.source.clone()].into_iter().collect();
    let outcome = compiler.compile(&changed, false).expect("body-edit compile");
    assert!(
        outcome.restart_reasons.is_empty(),
        "a body edit must publish an overlay without restarting: {:?}",
        outcome.restart_reasons
    );
    assert!(
        f.incremental.exists(),
        "the bound shape must write an incremental.lua overlay"
    );
}

/// An unchanged caller must observe the edited callee after loading the overlay.
#[test]
fn a_body_edit_reaches_its_caller_through_the_overlay() {
    let f = fixture();
    let mut stone = SkullStone::init(&f.dir, &f.main, &f.incremental).expect("init v1");
    let before = std::fs::read_to_string(&f.main).expect("main.lua v1");

    std::fs::write(&f.source, V2).expect("write v2");
    stone
        .recompile(&[f.source.clone()], &f.main, &f.incremental)
        .expect("recompile v2");
    let after = std::fs::read_to_string(&f.incremental).expect("incremental.lua v2");

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
         end\n"
    );
    let out = run_lua(&script).unwrap_or_else(|e| panic!("LuaJIT failed:\n{e}"));
    assert!(
        out.stdout.trim().starts_with("PASS"),
        "a body edit must reach its caller through the overlay:\n{}",
        out.combined(),
    );
}


#[test]
fn a_bound_body_edit_publishes_an_overlay_that_preserves_live_state() {
    let f = fixture();
    let mut stone = SkullStone::init(&f.dir, &f.main, &f.incremental).expect("init");
    let before = std::fs::read_to_string(&f.main).expect("main.lua");
    std::fs::write(&f.source, V2).expect("edit");
    let reload = stone.recompile(&[f.source.clone()], &f.main, &f.incremental).expect("reload");
    assert!(matches!(reload, SkullReload::Incremental { .. }), "{reload:?}");
    assert_eq!(before, std::fs::read_to_string(&f.main).unwrap());
    let overlay = std::fs::read_to_string(&f.incremental).expect("incremental.lua");
    let script = format!(
        "{before}\nbump()\n{overlay}\nbump()\nassert(SCORE == 11)\nprint('PASS')\n"
    );
    let out = run_lua(&script).expect("loaded overlay");
    assert!(out.success, "{}", out.combined());
    assert!(out.stdout.contains("PASS"), "{}", out.combined());
}
