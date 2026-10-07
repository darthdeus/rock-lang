//! P3 of docs/closure-env-records-plan.md: a lambda whose scope captures NOTHING
//! hoists to a top-level `function <enclosing>__lambda<k>(<params>) ... end` and
//! is referenced through one reusable logical CodeRef at the creation site. A
//! capturing lambda carries that CodeRef plus its environment record.
use crate::common::{emit_module, function_body, mangled_name, run_luajit};

// `free_loop` creates a capture-free lambda (`() => { sink(42) }` closes over
// nothing) in a loop; `capturing_loop` creates one that captures the loop
// binding `x`. Same shape, different capture status.
const FIXTURE: &str = r#"
@nomangle inline(lua) fn sink(a: int) = "_G.__sink = a"

fn free_loop() -> [fn()] {
    let fns: [fn()] = []
    for i in 0..3 {
        fns.push(() => { sink(42) })
    }
    return fns
}

fn capturing_loop() -> [fn() -> int] {
    let fns: [fn() -> int] = []
    for i in 0..3 {
        let x = i
        fns.push(() => x)
    }
    return fns
}

fn main() { let _ = free_loop() let _ = capturing_loop() }
"#;

/// SHAPE: the capture-free lambda hoists to a top-level `<free_loop>__lambda0`
/// (the enclosing name mangled) and the creation site references it by name --
/// no inline `function(` closure in `free_loop`'s body.
#[test]
fn capture_free_lambda_hoists_to_named_toplevel() {
    let lua = emit_module(FIXTURE);
    let lam = format!("{}__lambda0", mangled_name(&lua, "free_loop"));
    assert!(
        lua.contains(&format!("function {lam}(")),
        "the capture-free lambda must hoist to a top-level `{lam}`:\n{lua}"
    );
    let body = function_body(&lua, "free_loop");
    assert!(
        body.contains("__rock_code_slot_ref("),
        "the creation site must reference the reusable CodeRef:\n{body}"
    );
    // An anonymous inline closure renders `function(` (no space); the enclosing
    // fn header is `function free_loop(` (space + name), so `function(` is the
    // precise marker of a leftover inline closure.
    assert!(
        !body.contains("function("),
        "a hoisted capture-free lambda must leave NO inline closure in the enclosing body:\n{body}"
    );
}

/// SHAPE (P2.6): a CAPTURING lambda also hoists -- to a `(__rock_env, ..)` body -- and its
/// creation site builds an env record instead of an inline closure. (Pre-P2.6 it
/// stayed inline; env_record_hoist.rs covers the record mechanics in depth.)
#[test]
fn capturing_lambda_hoists_as_record() {
    let lua = emit_module(FIXTURE);
    let body = function_body(&lua, "capturing_loop");
    let lam = format!("{}__lambda", mangled_name(&lua, "capturing_loop"));
    assert!(
        !body.contains("function("),
        "a capturing lambda no longer renders an inline closure at the creation site:\n{body}"
    );
    assert!(
        body.contains("__slot = __rock_code_slot_ref("),
        "the capturing lambda's creation site builds a CodeRef env record:\n{body}"
    );
    assert!(
        lua.contains(&format!("function {lam}")),
        "the capturing lambda's body hoists to a named top-level fn:\n{lua}"
    );
}

/// RUNTIME (RED until P3): two creations of the SAME capture-free lambda are the
/// SAME fn value (a shared top-level global), not fresh per-creation closures.
#[test]
fn hoisted_lambda_shared_across_creations() {
    let src = r#"
@nomangle inline(lua) fn sink(a: int) = "_G.__sink = a"
fn mk() -> fn() { return () => { sink(1) } }
fn main() { let _ = mk() }
"#;
    let lua = emit_module(src);
    let mk = mangled_name(&lua, "mk");
    let driver = format!("\nlocal a = {mk}()\nlocal b = {mk}()\nprint(rawequal(a, b))\n");
    let out = run_luajit(&format!("{lua}{driver}"), "shared");
    assert_eq!(out, "true", "two creations of a capture-free lambda must share one hoisted fn value; got: {out}");
}

/// RUNTIME (behavior preserved): a hoisted lambda is still callable through a
/// fn-typed value, and a lambda-local `let` stays PER-CALL (two calls don't
/// share state) -- "6\n6", never "6\n7".
#[test]
fn hoisted_lambda_locals_stay_per_call() {
    let src = r#"
fn counter_fn() -> fn() -> int {
    return () => { let n = 5 n = n + 1 return n }
}
fn main() {
    let f = counter_fn()
    print("{f()}")
    print("{f()}")
}
"#;
    let lua = emit_module(src);
    let out = run_luajit(&format!("{lua}\nmain()\n"), "percall");
    assert_eq!(out, "6\n6", "a hoisted lambda's local must be per-call (not shared); got: {out}");
}

/// A definition whose name equals a hoisted lambda's `<enclosing>__lambda<k>`
/// name must be REJECTED (a spanned `CompilerError`, not a panic / silent
/// clobber). `foo() -> fn()` mangles to `foo`, so its capture-free
/// lambda hoists to `foo__lambda0`; a `@nomangle` fn keeps that
/// bare name and collides. (A PLAIN user fn would itself mangle and could no
/// longer BE a bare synthetic name.) Full family: `synthetic_name_collision.rs`.
#[test]
fn hoist_name_collision_with_user_fn_rejected() {
    let src = r#"
@nomangle inline(lua) fn sink(a: int) = "_G.__sink = a"
@nomangle fn foo__lambda0() -> int { return 99 }
fn foo() -> fn() { return () => { sink(1) } }
fn main() {}
"#;
    let err = crate::common::try_emit_module(src).err().expect("the collision must be rejected");
    assert!(
        err.message.contains("foo__lambda0") && err.message.contains("compiler-reserved synthetic name"),
        "rejection names the colliding synthetic name; got: {}",
        err.message
    );
}

/// TRANSITIVE capture (P2.6): an outer lambda that is locally-capture-free but
/// whose INNER lambda reads an enclosing-FN cell captures that cell transitively.
/// Both now hoist -- the outer as an env record threading the cell inward, the
/// inner as an env record reading it from the outer's `__rock_env`. (Pre-P2.6 neither
/// hoisted; the outer threaded the cell as an inline-closure upvalue.)
#[test]
fn transitive_root_capture_hoists_both() {
    let src = r#"
fn transitive(x: int) -> fn() -> int {
    let outer = () => { return () => x }
    return outer()
}
fn main() { let g = transitive(7) print("{g()}") }
"#;
    let lua = emit_module(src);
    // Both lambdas capture the root param `x`, so both hoist to env-carrying bodies
    // (a colored `c<N>` env param, not the old magic `__rock_env` local).
    let m = mangled_name(&lua, "transitive");
    assert!(
        lua.contains(&format!("function {m}__lambda0(c")) && lua.contains(&format!("function {m}__lambda1(c")),
        "both lambdas hoist to env bodies (both capture the root param `x`):\n{lua}"
    );
    // and it still behaves (the inner reads x threaded through both envs).
    let out = run_luajit(&format!("{lua}\nmain()\n"), "transitive");
    assert_eq!(out, "7", "transitive capture must still read the enclosing param; got: {out}");
}

/// DECLARATION SPLIT: a hoisted body's own cells are declared as `local`s in the
/// hoisted fn (not the enclosing one, not NEITHER). A cell declared in neither
/// function becomes an implicit Lua GLOBAL on first write (`c<N> = ..`), which
/// this pins by asserting no `c<N>` global exists after running -- a vacuous
/// green otherwise (write-then-read within one call reads back its own global).
#[test]
fn hoisted_body_declares_own_cells_no_global_leak() {
    let src = r#"
fn split_probe() -> fn() -> int {
    return () => {
        let a = 1
        let b = 2
        let c = a + b
        return c
    }
}
fn main() { let f = split_probe() print("{f()}") }
"#;
    let lua = emit_module(src);
    let driver = "\nmain()\n\
        local leaked = {}\n\
        for k in pairs(_G) do if type(k) == \"string\" and k:match(\"^c%d+$\") then leaked[#leaked + 1] = k end end\n\
        table.sort(leaked)\n\
        print(#leaked == 0 and \"no-leak\" or table.concat(leaked, \",\"))\n";
    let out = run_luajit(&format!("{lua}{driver}"), "split");
    assert_eq!(
        out, "3\nno-leak",
        "the hoisted body must declare its own cells as locals (no c<N> leaked to _G); got:\n{out}"
    );
}
