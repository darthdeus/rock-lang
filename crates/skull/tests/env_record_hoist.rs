use crate::common::{emit_module, function_body, mangled_name, run_luajit, run_main};

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
    assert!(lua.contains(&format!("function {lam}(c")));
    let body = function_body(&lua, "mk_cap");
    assert!(body.contains("setmetatable({ __slot = __rock_code_slot_ref("));
    assert!(body.contains("__rock_fn_mt"));
    assert!(!body.contains("function("));
    let out = run_main(&lua);
    assert_eq!(out, "5");
}

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
    assert!(body.contains("= {v = "));
    assert!(body.contains("setmetatable({ __slot = __rock_code_slot_ref("));
    // A boxed capture field passes the box TABLE (no `.v`); each hoisted body
    // reads/writes it through `__rock_env.c<N>.v`.
    assert!(lua.contains(".v"));
    let out = run_main(&lua);
    assert_eq!(out, "0\n2");
}

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
    assert!(lua.contains(&format!("function {m}__lambda0(c")));
    assert!(lua.contains(&format!("function {m}__lambda1(c")));
    // The OUTER hoisted body constructs the INNER record, seeding the capture from
    // its OWN env (the transitive thread). The exact env-field render is a colored
    // `c<N>.c<M>`; the behavior below proves the thread, so pin only the record.
    let outer_builds_inner = (0..2).any(|k| {
        let body = function_body(&lua, &format!("{m}__lambda{k}"));
        body.contains("setmetatable({ __slot = __rock_code_slot_ref(")
    });
    assert!(outer_builds_inner);
    let out = run_main(&lua);
    assert_eq!(out, "9");
}

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
    assert!(lua.contains("{v = "));
    let out = run_main(&lua);
    assert_eq!(out, "15");
}

#[test]
fn record_fields_hold_no_inline_closures() {
    let src = r#"
fn mk_snap(x: int) -> fn() -> int { return () => x }
fn main() { let _ = mk_snap(1) }
"#;
    let lua = emit_module(src);
    let mk = mangled_name(&lua, "mk_snap");
    let driver = format!(
        "\nlocal a = {mk}(1)\nlocal b = {mk}(2)\n\
        print(rawequal(a.__slot, b.__slot))\n\
        local extra = false\n\
        for k, v in pairs(a) do if k ~= \"__slot\" and type(v) == \"function\" then extra = true end end\n\
        print(extra)\n"
    );
    let out = run_luajit(&format!("{lua}{driver}"));
    assert_eq!(out, "true\nfalse");
}

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
    let out = run_luajit(&format!("{lua}{driver}"));
    assert_eq!(out, "true\t42");
}

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
    let out = run_luajit(&format!("{lua}{driver}"));
    assert_eq!(out, "12\t10");
}
