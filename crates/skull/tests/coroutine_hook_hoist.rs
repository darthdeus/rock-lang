//! P1 of docs/closure-env-records-plan.md: a co-fn lifecycle hook
//! (`on_killed`/`on_resume`/`on_hot_reload`) must compile to a per-co-fn
//! top-level Lua fn taking the frame `s`, stored as a plain fn-value field --
//! NOT a per-construction closure over the frame table.
//!
//! Premise (verified: `worker__step` reads its param `seed` as `c<s>.c1` and
//! the hook body reads `base` as `c<s>.c2` -- every cell a hook touches is a
//! frame field, so the hook's only upvalue is the frame table). That is what
//! makes hoisting to a fn taking `s` sound: the runtime tests below fire a hook
//! that reads a frame value and assert it still observes the right value, so a
//! dropped capture would go red.

mod common;
use common::{emit_module, mangled_name, run_luajit};

/// A co fn whose `on_killed` hook reads a frame local (`base`), derived from the
/// ctor param `seed`. `main` exercises worker/resume/kill so all four fns
/// (`worker`, `worker__step`, `worker__resume`, `__rock_coroutine_kill`) emit.
const ON_KILLED: &str = r#"
enum Ev { Tick(int) }
global LOG: int = 0
fn note(x: int) { LOG = x }

co fn worker(seed: int) -> Coroutine<Ev> {
    let base = seed + 100
    on_killed(() => note(base))
    yield Ev::Tick(base)
}

fn main() {
    let c = worker(0)
    let _ = c.resume()
    c.kill()
}
"#;

/// Run `prog` under luajit, returning trimmed stdout (panics with stderr on a
/// non-zero exit). Temp file keyed by pid + a caller tag so parallel tests
/// (nextest is process-per-test, but be safe under `cargo test` threads too)
/// never collide.

/// SHAPE: the `on_killed` hook hoists to a shared top-level `worker__on_killed`
/// and the ctor stores it as a fn-value field -- no inline `function() .. end`
/// closure per construction. RED until P1 lands.
#[test]
fn on_killed_hook_hoisted_to_shared_toplevel_fn() {
    let lua = emit_module(ON_KILLED);
    let hook = format!("{}__on_killed", mangled_name(&lua, "worker"));
    assert!(
        lua.contains(&format!("function {hook}(")),
        "on_killed must hoist to a top-level `{hook}`:\n{lua}"
    );
    assert!(
        lua.contains(&format!("__on_killed = {hook}")),
        "the ctor must store the hoisted fn by value (`__on_killed = {hook}`):\n{lua}"
    );
    assert!(
        !lua.contains("__on_killed = function"),
        "the ctor must NOT build a per-construction closure (`__on_killed = function`):\n{lua}"
    );
}

/// SNAPSHOT-SHAPE: EVERY function-typed field on a handle is shared between two
/// constructions (a top-level fn value, not a fresh per-construction closure)
/// -- the property a deep-copy/snapshot needs, asserted over the whole table so
/// a regression adding a per-construction closure to ANY field (a wrapped
/// defer, a future channel helper) goes red, not just the hook fields. And
/// firing still observes the right frame value (guards the premise: hoisting
/// drops no capture).
#[test]
fn hook_fn_shared_across_constructions_and_still_fires() {
    let lua = emit_module(ON_KILLED);
    let w = mangled_name(&lua, "worker");
    let driver = format!("\n\
        local h1 = {w}(5)\n\
        local h2 = {w}(6)\n\
        {w}__resume(h1, nil)\n\
        local fresh = {{}}\n\
        for k, v in pairs(h1) do\n\
        \x20 if type(v) == \"function\" and not rawequal(v, h2[k]) then fresh[#fresh + 1] = k end\n\
        end\n\
        table.sort(fresh)\n\
        print(#fresh == 0 and \"shared\" or table.concat(fresh, \",\"))\n\
        __rock_coroutine_kill(h1)\n\
        print(LOG)\n");
    let out = run_luajit(&format!("{lua}{driver}"), "shared");
    assert_eq!(
        out, "shared\n105",
        "every fn-typed handle field must be shared across constructions and \
         firing must still read the frame (105); got:\n{out}"
    );
}