//! Hot-reload stability of the unified always-mangle scheme.
//!
//! skull mangles every non-exempt fn by its own `(name, declared signature)`
//! ONLY (`skull-mir/src/mangle.rs`), so a fn's emitted symbol is
//! decl-set-independent: adding or removing an unrelated overload of `foo`
//! anywhere does NOT rename the existing `foo(int)` symbol. It stays
//! `foo_int_ret_int` with or without a `foo(string)` sibling.
//!
//! That is what makes a hot reload safe. A reload re-`dofile`s the chunk in the
//! SAME lua state (an add-overload edit is an `Interface` edit -> `SkullReload::
//! Full`, NOT a `ShapeChanged` restart). Because the symbol name is unchanged,
//! the new `foo_int_ret_int` overwrites the running `_G.foo_int_ret_int` IN
//! PLACE, and any surviving pre-reload code that resolves `foo(int)` by name (a
//! suspended `co fn`, a preserved closure/callback) sees the NEW body -- no
//! orphaned stale global.
//!
//! These tests were the sentinels pinning the OLD overload-driven-mangling bug
//! (adding an overload renamed the unique `foo` and orphaned the running global,
//! silent-stale). The fix (always-mangle) landed; the assertions are flipped to
//! guard the correct behavior. History: docs/mangle-rename-hot-reload-investigation.md.

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

// v1: `foo(int)` body returns the version marker 51 (no arithmetic -- the
// stdlib-free `ws_of` has no `__op_*`). `probe` calls `foo` BY NAME; under
// always-mangle both emit as `foo_int_ret_int` / `probe_ret_int` (no bare
// globals), and `probe`'s body is `GETGLOBAL "foo_int_ret_int"`.
const V1: &str = "\
fn foo(x: int) -> int { return 51 }
fn probe() -> int { return foo(5) }
fn main() {}
";

// Reload target WITH the unrelated overload added. `foo(int)` body edited to
// return 52; a `foo(string)` overload is added (the only other change). The int
// overload STILL emits as `foo_int_ret_int` -- the added overload does not
// rename it.
const V2_OVERLOAD: &str = "\
fn foo(x: int) -> int { return 52 }
fn foo(s: string) -> string { return s }
fn probe() -> int { return foo(5) }
fn main() {}
";

// CONTROL reload target: identical `foo(int)` body edit (return 52), NO overload
// added -- same emitted symbol `foo_int_ret_int`.
const V2_CONTROL: &str = "\
fn foo(x: int) -> int { return 52 }
fn probe() -> int { return foo(5) }
fn main() {}
";

/// CONTROL: a plain body edit hot-reloads correctly -- surviving pre-reload code
/// (`GETGLOBAL foo_int_ret_int`) sees the NEW body, because the re-dofile
/// overwrites the symbol in place.
///
/// `old_probe` is the v1 `probe` fn VALUE captured before the reload; its body
/// resolves `foo(int)` by its mangled name at call time. Cold-load v1 (foo=51),
/// capture, reload v2-control (foo=52), then call `old_probe`: it must return 52.
#[test]
fn body_edit_without_overload_hot_reloads_running_code() {
    let before = emit_program(V1);
    let after = emit_program(V2_CONTROL);
    let script = format!(
        "_G.HOT_RELOAD = false\n{before}\n\
         local old_probe = probe_ret_int\n\
         _G.HOT_RELOAD = true\n{after}\n\
         print('old_probe=' .. old_probe())\n"
    );
    let out = run_luajit("control", &script);
    assert_eq!(
        out, "old_probe=52",
        "control: a body edit must reach surviving GETGLOBAL callers; got: {out}"
    );
}

/// FLIPPED SENTINEL (was: pinned the orphan-on-rename bug; now: guards the fix).
///
/// Adding an unrelated `foo(string)` overload does NOT rename the emitted
/// `foo(int)` symbol -- it stays `foo_int_ret_int` in both v1 and v2. On the
/// in-place hot reload the new body lands under the SAME name, so surviving
/// pre-reload code that resolves `foo(int)` by name sees the NEW body (52). No
/// bare `foo` global was ever emitted (every non-exempt fn mangles), so there is
/// nothing to orphan.
///
/// This assertion is the CORRECT behavior; if it regresses to the stale 51 the
/// decl-set-independence of the mangle broke -- treat that as a real bug, do not
/// re-invert.
#[test]
fn overload_add_orphans_running_global_sentinel() {
    let before = emit_program(V1);
    let after = emit_program(V2_OVERLOAD);
    let script = format!(
        "_G.HOT_RELOAD = false\n{before}\n\
         local old_probe = probe_ret_int\n\
         _G.HOT_RELOAD = true\n{after}\n\
         print('fresh=' .. old_probe())\n\
         print('canonical=' .. foo_int_ret_int(5))\n\
         print('bare_foo_present=' .. tostring(_G.foo ~= nil))\n"
    );
    let out = run_luajit("overload", &script);
    let fresh = out.lines().find(|l| l.starts_with("fresh=")).unwrap();
    let canon = out.lines().find(|l| l.starts_with("canonical=")).unwrap();
    let bare = out.lines().find(|l| l.starts_with("bare_foo_present=")).unwrap();

    // FIXED: surviving GETGLOBAL-foo_int_ret_int code sees the edited body.
    assert_eq!(
        fresh, "fresh=52",
        "always-mangle: the added overload must not rename foo(int); surviving code sees the \
         edited body. If this is 51 the mangle stopped being decl-set-independent. got: {out}"
    );
    // The reloaded chunk's authoritative int fn carries the edit, at the SAME name.
    assert_eq!(canon, "canonical=52", "foo_int_ret_int must carry the edit; got: {out}");
    // No bare `foo` global exists under always-mangle -- there is nothing to orphan.
    assert_eq!(bare, "bare_foo_present=false", "no bare foo global should be emitted; got: {out}");
}
