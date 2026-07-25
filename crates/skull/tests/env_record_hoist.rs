//! P2 step 6 of docs/closure-env-records-plan.md: a CAPTURING lambda hoists to a
//! top-level `function <enclosing>__lambda<k>(__rock_env, <params>) ... end` whose captures
//! arrive as fields of the env record (`__rock_env`), and the creation site builds a plain
//! data record `setmetatable({ __fn = <enclosing>__lambda<k>, c<A> = .. }, __rock_fn_mt)`
//! instead of an inline Lua closure. A by-value capture snapshots the value into
//! the field; a boxed (mutable) capture threads the shared box TABLE, so two
//! records that capture it observe one another's writes. A nested capturing lambda
//! reads its outer's captures through the outer's `__rock_env`.
//!
//! Contrast `capture_free_hoist.rs`: a lambda that captures NOTHING stays a bare
//! hoisted global name (no record, no `__call`).

mod common;
use common::{emit_module, function_body, mangled_name, run_luajit};

/// A lambda capturing a single-assignment fn param: the record + `(__rock_env, ..)` body
/// shape, and no inline closure at the creation site.
#[test]
fn capturing_lambda_emits_record_and_env_body() {
    let src = r#"
fn mk_cap(x: int) -> fn() -> int { return () => x }
fn main() { let f = mk_cap(5) print("{f()}") }
"#;
    let lua = emit_module(src);
    // The capturing lambda hoists to an env-carrying body. Post-cutover the env
    // param is a normal (colored `c<N>`) cell materialized by the pass, not the
    // magic `__rock_env` local the old codegen path named it.
    let lam = format!("{}__lambda0", mangled_name(&lua, "mk_cap"));
    assert!(
        lua.contains(&format!("function {lam}(c")),
        "the capturing lambda hoists to an env-carrying body:\n{lua}"
    );
    let body = function_body(&lua, "mk_cap");
    assert!(
        body.contains(&format!("setmetatable({{ __fn = {lam}")),
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
    let lam = format!("{}__lambda", mangled_name(&lua, "pair_box"));
    assert!(body.contains("= {}"), "the mutable capture declares a box `local c<N> = {{}}`:\n{body}");
    assert!(
        body.contains(&format!("setmetatable({{ __fn = {lam}")),
        "both captures hoist to env records:\n{body}"
    );
    // A boxed capture field passes the box TABLE (no `.v`); each hoisted body
    // reads/writes it through `__rock_env.c<N>.v`.
    assert!(lua.contains(".v"), "the hoisted bodies read/write the box through `.v`:\n{lua}");
    let out = run_luajit(&format!("{lua}\nmain()\n"), "envrec_box");
    assert_eq!(
        out, "0\n2",
        "two records sharing one box must observe each other's writes; got:\n{out}"
    );
}

/// A nested capturing lambda: the outer (which itself captures the fn param
/// transitively) constructs the inner record inside its hoisted body, reading the
/// capture from its OWN `__rock_env`.
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
    // Post-cutover the env param is a colored `c<N>` cell, not the magic `__rock_env`.
    let m = mangled_name(&lua, "outer_cap");
    assert!(lua.contains(&format!("function {m}__lambda0(c")), "one nested lambda hoists to an env body:\n{lua}");
    assert!(lua.contains(&format!("function {m}__lambda1(c")), "the other nested lambda hoists to an env body:\n{lua}");
    // The OUTER hoisted body constructs the INNER record, seeding the capture from
    // its OWN env (the transitive thread). The exact env-field render is a colored
    // `c<N>.c<M>`; the behavior below proves the thread, so pin only the record.
    let outer_builds_inner = (0..2).any(|k| {
        let body = function_body(&lua, &format!("{m}__lambda{k}"));
        body.contains(&format!("setmetatable({{ __fn = {m}__lambda"))
    });
    assert!(outer_builds_inner, "the outer hoisted body must build the inner record:\n{lua}");
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
    // The mutated lambda param rebinds to a box seeded from the param. The pass
    // materializes the box as `c<N> = {v = <param>}` (an assignment at the hoisted
    // body entry); the old codegen path wrote `local c<N> = { v = c<N> }`.
    assert!(
        lua.contains("{v = "),
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
    let mk = mangled_name(&lua, "mk_snap");
    let driver = format!("\nlocal a = {mk}(1)\nlocal b = {mk}(2)\n\
        print(rawequal(a.__fn, b.__fn))\n\
        local extra = false\n\
        for k, v in pairs(a) do if k ~= \"__fn\" and type(v) == \"function\" then extra = true end end\n\
        print(extra)\n");
    let out = run_luajit(&format!("{lua}{driver}"), "envrec_snapshape");
    assert_eq!(
        out, "true\nfalse",
        "two records share one hoisted __fn and hold no other function-typed field; got:\n{out}"
    );
}

/// `deepcopy` of a struct holding a capturing lambda produces a CALLABLE copy:
/// the stdlib deepcopy reattaches the source's metatable (R3), so the copied env
/// record keeps `__rock_fn_mt` and its `__call` still routes. Regression for the
/// P2.6-R deepcopy-drops-metatable bug (the copy used to raise "attempt to call a
/// table value").
#[test]
fn deepcopy_record_callback_stays_callable() {
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
    let out = run_luajit(&format!("{lua}{driver}"), "deepcopy_callable");
    assert_eq!(
        out, "true\t42",
        "a deepcopy'd env record keeps `__rock_fn_mt` and calls fine; got:\n{out}"
    );
}

/// `deepcopy` of a struct holding a lambda with a BOXED (mutable, shared) capture
/// gives an INDEPENDENT copy: mutating the original's box does not touch the copy's
/// (the deepcopy clones the box table, not just aliases it). Fronts P4's snapshot
/// divergence tests. Uses a getter+setter pair over one boxed `n` so the box is
/// shared within a Holder but distinct across the copy.
#[test]
fn deepcopy_record_boxed_capture_diverges_from_original() {
    let src = r#"
struct Pair { get: fn() -> int, bump: fn() -> int }
@nomangle inline(lua) fn stash_pair(p: Pair) = "_G.__P = p"
fn make_pair() -> Pair {
    let n = 10
    return Pair { get: () => n, bump: () => { n = n + 1 return n } }
}
fn main() {
    let orig = make_pair()
    stash_pair(deepcopy(orig))
    let _ = orig.bump()
    let _ = orig.bump()
    stash_orig(orig)
}
@nomangle inline(lua) fn stash_orig(p: Pair) = "_G.__O = p"
"#;
    let lua = emit_module(src);
    // orig bumped twice (10 -> 12); the copy was taken before either bump (still
    // 10) and must not observe the original's writes.
    let driver = "\n\
        main()\n\
        print(tostring(__O.get()) .. \"\\t\" .. tostring(__P.get()))\n";
    let out = run_luajit(&format!("{lua}{driver}"), "deepcopy_diverge");
    assert_eq!(
        out, "12\t10",
        "the copy's boxed capture is independent of the original's after mutation; got:\n{out}"
    );
}
