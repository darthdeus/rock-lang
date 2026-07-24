//! Hot-reload hazard of overload-driven mangling.
//!
//! The Lua backend mangles a fn name ONLY when the workspace has >1 definition
//! of it (`compute_overloaded_names`, `skull-codegen-lua/src/mangle.rs`). So the
//! emitted symbol of a fn is a function of the GLOBAL decl-set, not of the fn:
//! adding a SECOND, unrelated overload of `foo` anywhere renames the existing
//! unique `foo` to `foo__<sig>` in the emitted chunk.
//!
//! Hot reload re-`dofile`s the chunk in the SAME lua state (an add-overload edit
//! is an `Interface` edit -> `SkullReload::Full`, NOT a `ShapeChanged` restart
//! -- `rock-cli/src/skull_backend.rs` `route_and_emit`; `restart_types` stays
//! empty so the stone loop hot-reloads in place, `rock-cli/src/stone.rs`). The
//! new chunk defines `foo__int_ret_int`; NOTHING nils the OLD global `foo`
//! (`__rock_known_globals` snapshots GLOBALS/data only, never fn names --
//! `mir_emit.rs`). Any surviving pre-reload code that does `GETGLOBAL "foo"` (a
//! suspended `co fn`, a closure/callback in preserved state, a not-re-registered
//! system) silently keeps running the STALE pre-reload body forever. No error,
//! no log.
//!
//! Investigation: docs/mangle-rename-hot-reload-investigation.md.

mod common;
use common::ws_of;

/// `src` -> full skull Lua program (stdlib-free: int/string/empty-main only).
fn emit_program(src: &str) -> String {
    let ws = ws_of(src);
    let mid = skull::user_module_id(&ws).unwrap();
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit").lua
}

fn run_luajit(test: &str, prog: &str) -> String {
    use std::process::Command;
    // Keyed per TEST, not per process: `cargo test` runs both tests
    // concurrently in one process, so a pid-only name races.
    let path =
        std::env::temp_dir().join(format!("skull_mangle_reload_{}_{}.lua", test, std::process::id()));
    std::fs::write(&path, prog).expect("write program");
    let out = Command::new("luajit").arg(&path).output().expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "luajit exited non-zero.\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout.trim().to_string()
}

// v1: unique `foo(int)`; body returns the version marker 51 (no arithmetic --
// the stdlib-free `ws_of` has no `__op_*`). `probe` calls `foo` BY NAME, so its
// emitted body is `GETGLOBAL "foo"`, exactly `Inst::Global`.
const V1: &str = "\
fn foo(x: int) -> int { return 51 }
fn probe() -> int { return foo(5) }
fn main() {}
";

// Reload target WITH the unrelated overload added. `foo(int)` body edited to
// return 52; a `foo(string)` overload is added (the only other change). The int
// overload now emits as `foo__int_ret_int`; bare `foo` is NOT redefined.
const V2_OVERLOAD: &str = "\
fn foo(x: int) -> int { return 52 }
fn foo(s: string) -> string { return s }
fn probe() -> int { return foo(5) }
fn main() {}
";

// CONTROL reload target: identical `foo(int)` body edit (return 52), but NO
// overload added -- `foo` stays unique, re-emits under the bare name `foo`.
const V2_CONTROL: &str = "\
fn foo(x: int) -> int { return 52 }
fn probe() -> int { return foo(5) }
fn main() {}
";

/// CONTROL (normal green): a body edit that does NOT change the emitted symbol
/// hot-reloads correctly -- surviving pre-reload code (`GETGLOBAL foo`) sees the
/// NEW body, because the re-dofile overwrites `_G.foo` in place.
///
/// `old_probe` is the v1 `probe` fn VALUE captured before the reload; its body
/// resolves `foo` by name at call time. Cold-load v1 (foo=51), capture, reload
/// v2-control (foo=52), then call `old_probe`: it must return the NEW 52.
#[test]
fn body_edit_without_overload_hot_reloads_running_code() {
    let before = emit_program(V1);
    let after = emit_program(V2_CONTROL);
    let script = format!(
        "_G.HOT_RELOAD = false\n{before}\n\
         local old_probe = probe\n\
         _G.HOT_RELOAD = true\n{after}\n\
         print('old_probe=' .. old_probe())\n"
    );
    let out = run_luajit("control", &script);
    assert_eq!(
        out, "old_probe=52",
        "control: a bare-name body edit must reach surviving GETGLOBAL callers; got: {out}"
    );
}

/// SENTINEL (pins a known bug -- see docs/test-sentinels.md).
///
/// THE BUG: adding an unrelated `foo(string)` overload renames the emitted
/// `foo(int)` symbol to `foo__int_ret_int`. On the in-place hot reload the new
/// body lands under the new name and the running `_G.foo` is orphaned (never
/// nilled, never overwritten). Surviving pre-reload code that does
/// `GETGLOBAL "foo"` silently keeps executing the STALE v1 body forever -- no
/// error, no log. The SAME edit without the overload (the control test above)
/// correctly reaches that code.
///
/// WHY INVERTED: this asserts the *buggy* outcome, so it passes today BECAUSE
/// the bug exists. `old_probe()` returns the stale 51 while the current
/// authoritative int fn (`foo__int_ret_int`) returns 52 -- the running code and
/// the reloaded chunk disagree.
///
/// WHEN IT FAILS (fix landed -- e.g. always-mangle so the symbol is
/// decl-set-independent, or a reload shim that re-aliases the orphaned bare
/// name): `old_probe()` will return 52. Flip the first assertion to
/// `assert_eq!(stale, "52", ...)`, drop the disagreement note, update this
/// comment.
#[test]
fn overload_add_orphans_running_global_sentinel() {
    let before = emit_program(V1);
    let after = emit_program(V2_OVERLOAD);
    let script = format!(
        "_G.HOT_RELOAD = false\n{before}\n\
         local old_probe = probe\n\
         _G.HOT_RELOAD = true\n{after}\n\
         print('stale=' .. old_probe())\n\
         print('canonical=' .. foo__int_ret_int(5))\n\
         print('bare_foo_present=' .. tostring(_G.foo ~= nil))\n"
    );
    let out = run_luajit("overload", &script);
    let stale = out.lines().find(|l| l.starts_with("stale=")).unwrap();
    let canon = out.lines().find(|l| l.starts_with("canonical=")).unwrap();
    let bare = out.lines().find(|l| l.starts_with("bare_foo_present=")).unwrap();

    // BUG (pinned): surviving code runs the STALE v1 body.
    assert_eq!(
        stale, "stale=51",
        "SENTINEL: surviving GETGLOBAL-foo code should see the edited body (52) but \
         runs the stale v1 body (51). If this is now 52 the orphan-on-rename bug was \
         fixed -- flip to expect 52. got: {out}"
    );
    // The reloaded chunk's authoritative int fn DOES carry the edit -> the
    // running program and its own current chunk disagree about `foo(5)`.
    assert_eq!(canon, "canonical=52", "reloaded chunk's foo__int_ret_int must carry the edit; got: {out}");
    // The orphaned bare global is never removed -- that is why it stays callable
    // and stale (never a crash for fns; `__rock_known_globals` is data-only).
    assert_eq!(bare, "bare_foo_present=true", "orphaned bare foo must persist; got: {out}");
}
