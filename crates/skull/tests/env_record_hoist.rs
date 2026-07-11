//! P2 step 6 of docs/closure-env-records-plan.md: a CAPTURING lambda hoists to a
//! top-level `function <enclosing>__lambda<k>(e, <params>) ... end` whose captures
//! arrive as fields of the env record `e`, and the creation site builds a plain
//! data record `setmetatable({ __fn = <enclosing>__lambda<k>, c<A> = .. }, __rock_fn_mt)`
//! instead of an inline Lua closure. A by-value capture snapshots the value into
//! the field; a boxed (mutable) capture threads the shared box TABLE, so two
//! records that capture it observe one another's writes. A nested capturing lambda
//! reads its outer's captures through the outer's `e`.
//!
//! Contrast `capture_free_hoist.rs`: a lambda that captures NOTHING stays a bare
//! hoisted global name (no record, no `__call`).

mod common;
use common::{emit_module, function_body, run_luajit};

/// A lambda capturing a single-assignment fn param: the record + `(e, ..)` body
/// shape, and no inline closure at the creation site.
#[test]
fn capturing_lambda_emits_record_and_env_body() {
    let src = r#"
fn mk_cap(x: int) -> fn() -> int { return () => x }
fn main() { let f = mk_cap(5) print("{f()}") }
"#;
    let lua = emit_module(src);
    assert!(
        lua.contains("function mk_cap__lambda0(e"),
        "the capturing lambda hoists to an `(e, ..)` body:\n{lua}"
    );
    let body = function_body(&lua, "mk_cap");
    assert!(
        body.contains("setmetatable({ __fn = mk_cap__lambda0"),
        "the creation site builds an env record, not a closure:\n{body}"
    );
    assert!(body.contains("__rock_fn_mt"), "the record uses the shared metatable:\n{body}");
    assert!(
        !body.contains("function("),
        "a hoisted capturing lambda leaves NO inline closure at the creation site:\n{body}"
    );
    let out = run_luajit(&format!("{lua}\nmain()\n"), "envrec_simple");
    assert_eq!(out, "5", "the record is callable via __call and reads its capture; got: {out}");
}

/// A mutable capture is BOXED and the SAME box table is threaded into two
/// records: a write through one is visible through the other (by reference).
#[test]
fn boxed_capture_shares_one_box_across_records() {
    let src = r#"
fn pair_box() -> (fn(), fn() -> int) {
    let n = 0
    let bump = () => { n = n + 1 }
    let read = () => n
    return (bump, read)
}
fn main() {
    let (bump, read) = pair_box()
    print("{read()}")
    bump()
    bump()
    print("{read()}")
}
"#;
    let lua = emit_module(src);
    let body = function_body(&lua, "pair_box");
    assert!(body.contains("= {}"), "the mutable capture declares a box `local c<N> = {{}}`:\n{body}");
    assert!(
        body.contains("setmetatable({ __fn = pair_box__lambda"),
        "both captures hoist to env records:\n{body}"
    );
    // A boxed capture field passes the box TABLE (no `.v`); each hoisted body
    // reads/writes it through `e.c<N>.v`.
    assert!(lua.contains(".v"), "the hoisted bodies read/write the box through `.v`:\n{lua}");
    let out = run_luajit(&format!("{lua}\nmain()\n"), "envrec_box");
    assert_eq!(
        out, "0\n2",
        "two records sharing one box must observe each other's writes; got:\n{out}"
    );
}

/// A nested capturing lambda: the outer (which itself captures the fn param
/// transitively) constructs the inner record inside its hoisted body, reading the
/// capture from its OWN `e`.
#[test]
fn nested_capturing_lambda_reads_outer_env() {
    let src = r#"
fn outer_cap(x: int) -> fn() -> fn() -> int { return () => { return () => x } }
fn main() {
    let mid = outer_cap(9)
    let inner = mid()
    print("{inner()}")
}
"#;
    let lua = emit_module(src);
    // Both lambdas hoist (the outer captures `x` transitively, threading it in);
    // inst-arena order fixes which k is inner vs outer, so assert order-agnostically.
    assert!(lua.contains("function outer_cap__lambda0(e"), "one nested lambda hoists to an env body:\n{lua}");
    assert!(lua.contains("function outer_cap__lambda1(e"), "the other nested lambda hoists to an env body:\n{lua}");
    // The OUTER hoisted body constructs the INNER record, seeding a field from its
    // OWN `e` (the transitive capture threaded inward): `<field> = e.c<N>`.
    let outer_builds_inner = (0..2).any(|k| {
        let body = function_body(&lua, &format!("outer_cap__lambda{k}"));
        body.contains("setmetatable({ __fn = outer_cap__lambda") && body.contains("= e.")
    });
    assert!(outer_builds_inner, "the outer hoisted body must build the inner record from its own env:\n{lua}");
    let out = run_luajit(&format!("{lua}\nmain()\n"), "envrec_nested");
    assert_eq!(out, "9", "the nested capture must thread through both envs; got: {out}");
}

/// BOXED LAMBDA-PARAM (deferred from P2.5, owed to P2.6): a lambda's OWN param,
/// mutated inside it and captured by a NESTED lambda, must box so the nested
/// capture observes the mutation by reference. The box rebinds from the param at
/// the hoisted body entry (`local c<N> = { v = c<N> }`).
#[test]
fn boxed_lambda_param_shares_box_with_nested_capture() {
    let src = r#"
fn apply_ten(f: fn(int) -> fn() -> int) -> fn() -> int { return f(10) }
fn drive() -> int {
    let g = apply_ten((n: int) => {
        let read = () => n
        n = n + 5
        return read
    })
    return g()
}
fn main() { print("{drive()}") }
"#;
    let lua = emit_module(src);
    assert!(
        lua.contains("= { v = "),
        "the mutated lambda param must rebind to a box seeded from the param:\n{lua}"
    );
    let out = run_luajit(&format!("{lua}\nmain()\n"), "envrec_boxparam");
    assert_eq!(out, "15", "the nested capture must observe the param mutation via the shared box; got: {out}");
}

/// SNAPSHOT-SHAPE: a record built for a lambda capturing only non-fn values holds
/// exactly one function-typed field, `__fn`, and it is the SHARED hoisted global
/// (rawequal across two creations), never a fresh per-instance closure.
#[test]
fn record_fields_hold_no_inline_closures() {
    let src = r#"
fn mk_snap(x: int) -> fn() -> int { return () => x }
fn main() { let _ = mk_snap(1) }
"#;
    let lua = emit_module(src);
    let driver = "\nlocal a = mk_snap(1)\nlocal b = mk_snap(2)\n\
        print(rawequal(a.__fn, b.__fn))\n\
        local extra = false\n\
        for k, v in pairs(a) do if k ~= \"__fn\" and type(v) == \"function\" then extra = true end end\n\
        print(extra)\n";
    let out = run_luajit(&format!("{lua}{driver}"), "envrec_snapshape");
    assert_eq!(
        out, "true\nfalse",
        "two records share one hoisted __fn and hold no other function-typed field; got:\n{out}"
    );
}

/// SENTINEL (docs/test-sentinels.md): `deepcopy` of a struct holding a capturing
/// lambda produces an UNCALLABLE copy today -- the stdlib deepcopy copies tables
/// without reattaching metatables (its "Rock values carry no metatables" premise
/// predates env records), so the copied record loses `__rock_fn_mt` and calling
/// it raises "attempt to call a table value". Correct behavior: the copy calls
/// fine and returns 42 (and diverges independently from the original -- P4 owns
/// the full divergence test). The pcall result is inverted to today's failure.
///
/// WHEN THIS FAILS: the deepcopy metatable fix landed. Flip the expectation to
/// "true\t42", drop the "buggy" framing, and check the plan doc P2.6-R box --
/// same commit.
#[test]
fn deepcopy_record_callback_uncallable_sentinel() {
    let src = r#"
struct Holder { cb: fn() -> int }
@nomangle inline(lua) fn stash(h: Holder) = "_G.__H2 = h"
fn main() {
    let n = 41
    let h = Holder { cb: () => n + 1 }
    stash(deepcopy(h))
}
"#;
    let lua = emit_module(src);
    let driver = "\n\
        main()\n\
        local ok, res = pcall(function() return __H2.cb() end)\n\
        print(tostring(ok) .. \"\\t\" .. tostring(res):gsub(\":.*\", \"\"))\n";
    let out = run_luajit(&format!("{lua}{driver}"), "deepcopy_sentinel");
    assert!(
        out.starts_with("false\t"),
        "SENTINEL: deepcopy'd record callback is expected to be UNCALLABLE today \
         (metatable lost); a `true` here means the fix landed -- flip this to \
         assert `true\\t42`; got:\n{out}"
    );
}
