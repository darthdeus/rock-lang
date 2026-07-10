//! P2 step 4 of docs/closure-env-records-plan.md: the `__rock_fn_mt` runtime
//! helper -- a shared metatable making an env record `{ __fn = <fn>, <captures> }`
//! callable as `f(...)` via `__call`, initialized once at load with the
//! preservation idiom so pre- and post-reload records share metatable identity.
//!
//! It lives in the runtime prelude (`skull::runtime_prelude`) alongside its
//! siblings `__ROCK_C_STRUCT` / `__rock_on_hot_reload` / the RNG -- the home for
//! unconditional, load-time, reload-preserved runtime tables. (The plan said
//! "stdlib inline(lua)"; an inline(lua) fn doesn't auto-run at load and a
//! metatable is a table, not a callable, so the prelude bare-global form is what
//! the design's `setmetatable({..}, __rock_fn_mt)` reference needs.)

mod common;
use common::emit_module;

fn run_luajit(prog: &str, tag: &str) -> String {
    use std::process::Command;
    let path = std::env::temp_dir().join(format!("skull_fn_record_mt_{}_{tag}.lua", std::process::id()));
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

/// Any emitted program carries the prelude, so `__rock_fn_mt` is present.
#[test]
fn prelude_defines_shared_fn_metatable() {
    let lua = emit_module("fn main() {}\n");
    assert!(
        lua.contains("_G.__rock_fn_mt = { __call = function(e, ...) return e.__fn(e, ...) end }"),
        "the prelude must define the shared __rock_fn_mt metatable:\n(searched the prelude)"
    );
    assert!(
        lua.contains("if not _G.__rock_fn_mt then"),
        "__rock_fn_mt must use the reload-preservation guard"
    );
}

/// A hand-built record routes a plain `f(args)` call through `__call` to
/// `__fn(record, args)`, reading its captured field. This is the exact shape
/// P2.6 will emit.
#[test]
fn record_is_callable_via_call_metamethod() {
    let lua = emit_module("fn main() {}\n");
    let driver = "\n\
        local body = function(e, y) return e.x + y end\n\
        local rec = setmetatable({ __fn = body, x = 100 }, _G.__rock_fn_mt)\n\
        print(rec(5))\n";
    let out = run_luajit(&format!("{lua}{driver}"), "call");
    assert_eq!(out, "105", "f(5) must route through __call to __fn(rec, 5) reading rec.x; got: {out}");
}

/// Metatable identity is preserved across a reload: a record built before the
/// re-`dofile` and one built after share the SAME metatable (the `or`/`if not`
/// guard skips re-creation), so `getmetatable` compares equal -- required for
/// snapshot deep-copy to recognize records by `getmetatable(x) == __rock_fn_mt`.
#[test]
fn metatable_identity_preserved_across_reload() {
    let prog = emit_module("fn main() {}\n");
    let script = format!(
        "_G.HOT_RELOAD = false\n{prog}\n\
         local mt_before = _G.__rock_fn_mt\n\
         local rec_before = setmetatable({{ __fn = function(e) return 1 end }}, _G.__rock_fn_mt)\n\
         _G.HOT_RELOAD = true\n{prog}\n\
         local rec_after = setmetatable({{ __fn = function(e) return 2 end }}, _G.__rock_fn_mt)\n\
         if getmetatable(rec_before) == getmetatable(rec_after) and _G.__rock_fn_mt == mt_before then\n\
           print(\"PASS\")\n\
         else print(\"FAIL\") end\n"
    );
    let out = run_luajit(&script, "reload");
    assert_eq!(out, "PASS", "records built across a reload must share metatable identity; got: {out}");
}
