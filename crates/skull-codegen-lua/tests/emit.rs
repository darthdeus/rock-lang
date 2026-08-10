//! Emitted-Lua contracts for the dumb printer. Each lowers a snippet to MIR and
//! asserts its Lua. Naming is raw-arena-index based (`v<raw>`/`c<raw>`/`bb<raw>`),
//! so value numbers skip the indices consumed by effect instructions (`Store`).
//!
//! Execution under luajit is the deeper oracle, wired at the Phase-6 CLI cutover
//! (`rock {run,test} --backend skull`); these pin the textual shape until then.

use skull_codegen_lua::{emit_function, CodegenCtx};
use skull_mir::mir::FuncRef;

/// Lower `fn_name` from `modules` (last = the user `<test>` module), emit Lua.
fn emit_dump(mut modules: Vec<skull_parser::Ast>, fn_name: &str) -> String {
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("user module id");
    let m = ws.module(mid);
    let (fid, _) = m
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == fn_name)
        .expect("function by name");
    let scx = skull_mir::lower::ScalarCtx::new(&ws);
    let func = skull_mir::lower::lower_function(&ws, &scx, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    let ctx = CodegenCtx::new(&ws);
    emit_function(&ws, &ctx, scx.interner(), FuncRef { module: mid, func: fid }, &func)
}

/// Stdlib-free: literals, vars, `let`, builtin comparisons, control flow.
fn emit(src: &str, fn_name: &str) -> String {
    let ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse");
    emit_dump(vec![ast], fn_name)
}

/// Like [`emit_std`] but runs `skull_transform::cleanup` (copy-prop + DCE) before
/// emitting -- the artifact the REAL pipeline ships (`mir_emit.rs`). Needed to
/// assert allocation-shape claims (a dead `MakeTuple` left by the raw lowering is
/// removed here, so `no table` assertions reflect what actually runs).
fn emit_clean(src: &str, fn_name: &str) -> String {
    use std::sync::LazyLock;
    static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
        rock_host::stdlib_loader::stdlib_files()
            .iter()
            .map(|f| {
                let id = ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str());
                skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
            })
            .collect()
    });
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("user module id");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == fn_name)
        .expect("function by name");
    let scx = skull_mir::lower::ScalarCtx::new(&ws);
    let mut func = skull_mir::lower::lower_function(&ws, &scx, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    skull_transform::cleanup(&mut func);
    let ctx = CodegenCtx::new(&ws);
    emit_function(&ws, &ctx, scx.interner(), FuncRef { module: mid, func: fid }, &func)
}

/// Emit the graph-reached lowerable instances for the `<test>` module's user
/// roots, run a Lua `driver` expression (e.g. `"sign(-5)"`) under luajit, and
/// return its trimmed stdout. This keeps inlining disabled and delivers the
/// residual stdlib instances instead of depending on a splice for completeness.
/// Skips if luajit is absent so the suite still runs on a bare box.
fn run(src: &str, driver: &str) -> String {
    use std::sync::LazyLock;
    static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
        rock_host::stdlib_loader::stdlib_files()
            .iter()
            .map(|f| {
                let id = ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str());
                skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
            })
            .collect()
    });
    if std::process::Command::new("luajit").arg("-v").output().is_err() {
        eprintln!("luajit not found; skipping execution check");
        return driver.to_string(); // tests compare against this -> trivially pass
    }
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("user module id");
    let ctx = CodegenCtx::new(&ws);
    let roots = ws
        .module(mid)
        .functions
        .iter()
        .map(|(fid, _)| FuncRef { module: mid, func: fid })
        .collect();
    let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(&ws, roots, false, interner.clone());
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(interner);
    // Capturing lambdas lower to `setmetatable({..}, __rock_fn_mt)` env records
    // (docs/closure-env-records-plan.md); the record metatable lives in skull's
    // runtime prelude, which this bare harness does not emit -- pull in the
    // canonical definition (one copy, shared with `skull::runtime_prelude`).
    let mut lua = format!(
        "local bit = require('bit')\n{}\n{}\n",
        skull_codegen_lua::FN_MT_PRELUDE,
        skull_codegen_lua::VEC_CHECK_SHIMS
    );
    for inst in graph.expanded() {
        let f = &ws.module(inst.func.module).functions[inst.func.func];
        if f.inline_body.is_some() {
            continue;
        }
        if let Ok(mut func) = skull_mir::lower::lower_instance(&ws, &scx, inst) {
            skull_transform::cleanup(&mut func);
            skull_transform::recover_counted_loops(&ws, &mut func);
            lua.push_str(&skull_codegen_lua::emit_named(
                &ws,
                &ctx,
                scx.interner(),
                &inst.symbol(&ws, scx.interner()),
                &func,
            ));
        }
    }
    // Alias the user module's uniquely-named fns back to their bare source names
    // so the `driver` can call them by source name (every non-exempt fn mangles).
    {
        use std::collections::HashMap;
        let mut counts: HashMap<ustr::Ustr, usize> = HashMap::new();
        for (_, f) in ws.module(mid).functions.iter() {
            if !skull_mir::mangle::is_exempt(f) {
                *counts.entry(f.name).or_insert(0) += 1;
            }
        }
        for (_, f) in ws.module(mid).functions.iter() {
            if skull_mir::mangle::is_exempt(f) || counts[&f.name] > 1 {
                continue;
            }
            let m = skull_codegen_lua::mangle_fn(&ws, f);
            if m != f.name.as_str() {
                lua.push_str(&format!("_G[\"{}\"] = {}\n", f.name, m));
            }
        }
    }
    lua.push_str(&format!("print({})\n", driver));
    // Unique per call: parallel tests must not clobber a shared temp file.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("skull_emit_{}_{}.lua", std::process::id(), n));
    std::fs::write(&dir, &lua).expect("write lua");
    let out = std::process::Command::new("luajit")
        .arg(&dir)
        .output()
        .expect("run luajit");
    if !out.status.success() {
        panic!(
            "luajit failed:\n--- lua ---\n{lua}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Stdlib-loaded: needed for `__op_*` arithmetic overloads.
fn emit_std(src: &str, fn_name: &str) -> String {
    use std::sync::LazyLock;
    static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
        rock_host::stdlib_loader::stdlib_files()
            .iter()
            .map(|f| {
                let id = ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str());
                skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
            })
            .collect()
    });
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse"));
    emit_dump(modules, fn_name)
}

#[test]
fn int_literal_and_return() {
    // The `5` is a single-use pure value -> tree-folded into the return (no
    // register, no `local`).
    assert_eq!(
        emit("fn five() -> int { 5 }", "five"),
        "\
function five_ret_int()
  ::bb0::
  do return (5) end
end
"
    );
}

#[test]
fn param_cell_read() {
    assert_eq!(
        emit("fn id(x: int) -> int { x }", "id"),
        "\
function id_int_ret_int(c0)
  local r0
  ::bb0::
  r0 = c0
  do return r0 end
end
"
    );
}

#[test]
fn int_arithmetic() {
    assert_eq!(
        emit_std("fn add(a: int, b: int) -> int { a + b }", "add"),
        "\
function add_int_int_ret_int(c0, c1)
  local r0, r1
  ::bb0::
  r0 = c0
  r1 = c1
  do return (r0 + r1) end
end
"
    );
}

// --- execution oracle: emitted Lua is valid AND computes correctly ---

// Test fns avoid stdlib names (sign/sum/len exist there -> would mangle, and the
// driver calls by surface name).
#[test]
fn exec_sign_branch() {
    let src = "fn mysign(x: int) -> int { if x < 0 { 0 } else { 1 } }";
    assert_eq!(run(src, "mysign(-5)"), "0");
    assert_eq!(run(src, "mysign(7)"), "1");
}

#[test]
fn exec_arith_and_floor_div() {
    // int `./` is floor division; -7 ./ 2 = -4 (floor), not -3 (trunc).
    assert_eq!(run("fn f(a: int, b: int) -> int { a ./ b }", "f(-7, 2)"), "-4");
    assert_eq!(run("fn f(a: int, b: int) -> int { a * b + 1 }", "f(6, 7)"), "43");
}

#[test]
fn exec_while_loop_sum() {
    let src = "
fn mysum(n: int) -> int {
    let total = 0;
    let i = 1;
    while i <= n {
        total = total + i;
        i = i + 1;
    }
    total
}";
    assert_eq!(run(src, "mysum(10)"), "55");
}

#[test]
fn exec_for_range_sum() {
    let src = "
fn mysum(n: int) -> int {
    let total = 0;
    for i in 0..n {
        total = total + i;
    }
    total
}";
    assert_eq!(run(src, "mysum(5)"), "10"); // 0+1+2+3+4
}

#[test]
fn exec_recursion_factorial() {
    let src = "
fn fact(n: int) -> int {
    if n <= 1 { 1 } else { n * fact(n - 1) }
}";
    assert_eq!(run(src, "fact(5)"), "120");
}

#[test]
fn exec_tuple_and_index() {
    let src = "fn f() -> int { let t = (10, 20, 30); t.0 + t.2 }";
    assert_eq!(run(src, "f()"), "40");
}

#[test]
fn exec_vec_index() {
    let src = "fn f() -> int { let xs = [3, 4, 5]; xs[0] + xs[2] }";
    assert_eq!(run(src, "f()"), "8"); // 3 + 5
}

#[test]
fn exec_for_over_array() {
    // `[T]` iterates via concrete stdlib VecIter instances delivered beside the
    // user body; this fixture must work with inlining disabled.
    let src = "
fn total(xs: [int]) -> int {
    let s = 0;
    for x in xs { s = s + x; }
    s
}";
    assert_eq!(run(src, "total({_data = {4, 5, 6}, len = 3, __ty = \"Vec\"})"), "15");
}

#[test]
fn exec_struct_field() {
    let src = "
struct P { x: int, y: int }
fn f() -> int { let p = P { x: 7, y: 35 }; p.x + p.y }";
    assert_eq!(run(src, "f()"), "42");
}

#[test]
fn exec_string_concat() {
    let src = "fn greet(name: string) -> string { \"hi \" + name }";
    assert_eq!(run(src, "greet(\"bob\")"), "hi bob");
}

#[test]
fn inline_lua_with_inline_annotation_is_spliced() {
    // `@inline inline(lua)` is SPLICED at the call site (params bound to locals,
    // tail `return X` rewritten to `<result> = X`) instead of emitting a
    // `callee(args)` call -- so the body runs in the caller's scope (LuaJIT can
    // sink any cdata it produces; a call boundary blocks that).
    let body = emit(
        "@inline inline(lua) fn dbl(x: int) -> int = \"return x * 2\"\n\
         fn use_it(n: int) -> int { return dbl(n) }",
        "use_it",
    );
    assert!(
        !body.contains("dbl_int_ret_int("),
        "@inline inline(lua) should be spliced, not called:\n{body}"
    );
    assert!(
        body.contains("local x = "),
        "spliced body should bind the param to a local:\n{body}"
    );
}

#[test]
fn inline_lua_without_inline_annotation_is_called() {
    // Plain inline(lua) (no @inline) is still emitted as a function and CALLED --
    // splicing is opt-in (heavy unconditional inlining bloats code / traces).
    let body = emit(
        "inline(lua) fn dbl(x: int) -> int = \"return x * 2\"\n\
         fn use_it(n: int) -> int { return dbl(n) }",
        "use_it",
    );
    assert!(
        body.contains("dbl_int_ret_int("),
        "plain inline(lua) should be called, not spliced:\n{body}"
    );
}

#[test]
fn exec_inline_lua_spliced_multistatement() {
    // A multi-statement @inline inline(lua) body with a guard + tail return must
    // splice correctly and still compute the right value under luajit.
    let src = "@inline inline(lua) fn clampx(x: int) -> int = \"\"\"\n\
        if x < 0 then return 0 end\n\
        return x\n\
    \"\"\"\n\
    fn f(n: int) -> int { return clampx(n) }";
    // NOTE: `clampx` has an EARLY return -> not splice-safe -> falls back to a
    // call, but must still be correct.
    assert_eq!(run(src, "f(-5)"), "0");
    assert_eq!(run(src, "f(7)"), "7");
}

#[test]
fn exec_bitwise() {
    let src = "fn f(a: int, b: int) -> int { (a & b) | (a << 1) }";
    // (6 & 3) = 2 ; (6 << 1) = 12 ; 2 | 12 = 14
    assert_eq!(run(src, "f(6, 3)"), "14");
}

#[test]
fn exec_closure_call_and_capture() {
    // A closure passed as a fn-value and called (CallValue), plus a captured
    // local (an upvalue).
    let src = "
fn apply(f: fn(int) -> int, x: int) -> int { f(x) }
fn go() -> int {
    let bonus = 100
    apply(n => n + bonus, 5)
}";
    assert_eq!(run(src, "go()"), "105");
}

#[test]
fn exec_default_argument_filled_callee_side() {
    // An omitted arg arrives as nil; the callee fills the default.
    let src = "fn greet(name: string, greeting: string = \"Hello\") -> string { \"{greeting}, {name}!\" }";
    assert_eq!(run(src, "greet(\"World\")"), "Hello, World!");
    assert_eq!(run(src, "greet(\"World\", \"Hi\")"), "Hi, World!");
}

#[test]
fn exec_named_args_reorder() {
    // The named call is rock syntax, so it lives in a rock wrapper; the driver
    // calls the wrapper positionally. Named args reorder: b=3, a=10 -> 10 - 3.
    let src = "
fn sub(a: int, b: int) -> int { a - b }
fn go() -> int { sub(b: 3, a: 10) }";
    assert_eq!(run(src, "go()"), "7");
}

#[test]
fn exec_named_arg_with_default_gap() {
    // A named arg that skips earlier defaulted params: the gaps fill defaults.
    let src = "
fn f(a: int = 1, b: int = 2, c: int = 3) -> int { a * 100 + b * 10 + c }
fn go() -> int { f(c: 9) }";
    assert_eq!(run(src, "go()"), "129"); // a=1, b=2, c=9
}

#[test]
fn exec_newtype_ctor_is_identity() {
    let src = "
newtype Id(int)
fn f() -> Id { let a = Id(40); a }";
    // Id(40) is transparent -> the runtime value is just 40.
    assert_eq!(run(src, "f() + 2"), "42");
}

#[test]
fn exec_let_tuple_destructure() {
    let src = "fn f() -> int { let (a, b, c) = (10, 20, 12); a + b + c }";
    assert_eq!(run(src, "f()"), "42");
}

#[test]
fn exec_safe_nav() {
    // x?.field: nil base -> nil, else the field. Pass a Lua struct table / nil.
    let src = "
struct P { x: int, y: int }
fn gx(p: P?) -> int? { p?.x }";
    assert_eq!(run(src, "gx({__ty = \"P\", x = 9, y = 1})"), "9");
    assert_eq!(run(src, "gx(nil)"), "nil");
}

#[test]
fn exec_cast_float_to_int() {
    // float as int floors (toward -inf); other casts are identity.
    assert_eq!(run("fn f(x: float) -> int { x as int }", "f(3.9)"), "3");
    assert_eq!(run("fn f(x: float) -> int { x as int }", "f(-1.2)"), "-2");
    assert_eq!(run("fn f(x: int) -> float { x as float }", "f(5)"), "5");
}

#[test]
fn exec_interpolation_scalars() {
    // `{x}` holes for int/string/bool fold to `tostring(..)` and `..` concat.
    let src = "fn label(n: int, name: string, ok: bool) -> string { \"n={n} {name} ok={ok}\" }";
    assert_eq!(run(src, "label(5, \"hi\", true)"), "n=5 hi ok=true");
}

#[test]
fn emits_interpolation_as_flat_concat() {
    // Interpolation must emit ONE flat `..` run (`a .. b .. c .. d .. e`), not
    // left-nested binary concats (`((((a .. b) .. c) .. d) .. e)`). LuaJIT's
    // BC_CAT folds a flat run into a SINGLE string allocation; nesting allocates
    // an intermediate per step (~2x -- the dominant skull-vs-rock HUD/text alloc
    // gap). Pin the codegen shape: a 5-part interpolation must not deeply nest.
    let body = emit(
        "fn label(a: int, b: int) -> string { \"x={a}/{b} done\" }",
        "label",
    );
    assert!(
        !body.contains("((("),
        "interpolation must emit a flat concat, not nested parens:\n{body}"
    );
    assert!(
        body.matches(" .. ").count() >= 4,
        "expected a flat 5-part concat (4 ` .. `):\n{body}"
    );
}

// Enum match execution is covered end-to-end where the program-level enum
// scaffolding (`<Enum>_ct` ctors, `ffi.cdef` typedefs) is emitted: the `skull`
// umbrella crate (`enum_cdata`/`enum_scalarize`/`enum_payload_sroa`) and the
// Rock suite (`tests/enum_tests.rock`). The per-function `run` harness here
// can't emit that scaffolding (it would need a dep on `skull`), so it can't
// execute a scalarized-enum match; per-function match shape is pinned by
// skull-mir's `match_enum_*` lowering tests.

// --- Slice 1: multi-return ABI for tuple-returning fns ---

#[test]
fn tuple_return_callee_splats_multi_value() {
    // A fn whose return type is a tuple returns N values, NOT `return ({a, b})`
    // (one table alloc per call). The MakeTuple in return position is elided.
    let body = emit_clean("fn pair() -> (bool, int) { return (true, 42) }", "pair");
    assert!(
        !body.contains("{"),
        "tuple return must splat to multi-value, not build a table:\n{body}"
    );
    assert!(
        body.contains("do return (true), (42) end"),
        "expected `do return (true), (42) end`:\n{body}"
    );
}

#[test]
fn tuple_return_tail_expr_splats() {
    // Tail-position tuple (no explicit `return`) splats the same way.
    let body = emit_clean("fn pair() -> (bool, int) { (true, 42) }", "pair");
    assert!(!body.contains("{"), "tail tuple must not build a table:\n{body}");
    assert!(
        body.contains("do return (true), (42) end"),
        "expected `do return (true), (42) end`:\n{body}"
    );
}

#[test]
fn tuple_destructure_caller_uses_multi_assign() {
    // `let (a, b) = f()` where f returns a tuple emits a multi-assign
    // `<a>, <b> = f(...)`, not a table build + `[1]`/`[2]` indexing. Zero alloc.
    let src = "\
fn pair() -> (bool, int) { (true, 42) }\n\
fn use_it() -> int { let (ok, n) = pair(); if ok { n } else { 0 } }";
    let body = emit_clean(src, "use_it");
    assert!(
        !body.contains("{"),
        "destructure must be zero-alloc multi-assign, not a table build:\n{body}"
    );
    assert!(
        body.lines().any(|l| l.contains(", ") && l.contains("= pair_ret_tuple2_bool_int(")),
        "expected a multi-assign call line `a, b = pair(...)`:\n{body}"
    );
}

#[test]
fn tuple_local_whole_store_scalarizes() {
    // A tuple result stored whole and only field-read never packs a table: the
    // multi-return lands in per-field cells and `.1` loads its cell (tuple
    // SROA). The escaping case that MUST pack is pinned in
    // skull/tests/tuple_scalarize.rs (tuple_whole_use_keeps_table).
    let src = "\
fn pair() -> (bool, int) { (true, 42) }\n\
fn use_it() -> int { let t = pair(); t.1 }";
    let body = emit_clean(src, "use_it");
    assert!(
        !body.contains("[1]") && !body.contains("[2]"),
        "non-escaping whole-stored tuple should scalarize, not pack:\n{body}"
    );
    assert!(body.contains("= pair_ret_tuple2_bool_int()"), "multi-assign call expected:\n{body}");
}

#[test]
fn if_as_value_branch_and_cell_join() {
    // The `if`-as-value joins through a cell (no block params). Cell coloring
    // coalesces the param `x` (dead after its `r0 = c0` read in bb0) with the
    // join cell, so both are `c0`. Single-use pure values are tree-folded: the
    // `x < 0` compare folds into the branch and the `0`/`1` literals into the
    // join stores, so only `r0` (the cross-block `Load x`) keeps a register.
    assert_eq!(
        emit("fn sign(x: int) -> int { if x < 0 { 0 } else { 1 } }", "sign"),
        "\
function sign_int_ret_int(c0)
  local r0
  ::bb0::
  r0 = c0
  if (r0 < (0)) then goto bb1 else goto bb2 end
  ::bb1::
  c0 = (0)
  goto bb3
  ::bb2::
  c0 = (1)
  goto bb3
  ::bb3::
  r0 = c0
  do return r0 end
end
"
    );
}

/// Full pre-emit pipeline (mirrors `mir_emit.rs`): lower -> `inline_functions`
/// -> `cleanup` -> emit. The honest artifact for zero-alloc claims: an `@inline`
/// `iter`/`next` is only spliced (and its cursor struct scalarized) once the
/// inliner has run, which `emit_clean` does NOT do.
fn emit_inlined(src: &str, fn_name: &str) -> String {
    use std::sync::LazyLock;
    static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
        rock_host::stdlib_loader::stdlib_files()
            .iter()
            .map(|f| {
                let id = ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str());
                skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
            })
            .collect()
    });
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("user module id");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == fn_name)
        .expect("function by name");
    let scx = skull_mir::lower::ScalarCtx::new(&ws);
    let self_ref = FuncRef { module: mid, func: fid };
    let mut func = skull_mir::lower::lower_function(&ws, &scx, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    let mut cache = skull_transform::InlineCache::default();
    skull_transform::inline_functions(&ws, &scx, self_ref, &mut func, &mut cache)
        .expect("inline failed");
    skull_transform::cleanup(&mut func);
    skull_transform::recover_counted_loops(&ws, &mut func);
    let ctx = CodegenCtx::new(&ws);
    emit_function(&ws, &ctx, scx.interner(), self_ref, &func)
}

// --- Slice 4 acceptance gate: an @inline user iterator driven by `for`
// scalarizes its cursor struct to per-field locals; the loop allocates nothing.
const ZERO_ALLOC_ITER_SRC: &str = "
struct IntRange { lo: int, hi: int }
struct IntRangeIter { cur: int, hi: int }

@inline
fn iter(r: IntRange) -> IntRangeIter { return IntRangeIter { cur: r.lo, hi: r.hi } }

@inline
fn next(it: IntRangeIter) -> (bool, int) {
    if it.cur >= it.hi { return (false, 0) }
    let v = it.cur
    it.cur = it.cur + 1
    return (true, v)
}

fn sum_range(lo: int, hi: int) -> int {
    let total = 0
    for x in (IntRange { lo: lo, hi: hi }) { total += x }
    return total
}";

#[test]
fn for_user_iter_inline_is_zero_alloc() {
    let lua = emit_inlined(ZERO_ALLOC_ITER_SRC, "sum_range");
    assert!(
        !lua.contains("__rock_create_struct"),
        "cursor struct must scalarize away, no create_struct:\n{lua}"
    );
    assert!(
        !lua.contains('{'),
        "@inline iter/next over a `for` must leave no table constructor:\n{lua}"
    );
}

// A cursor whose field is a Vec (not just scalars) also scalarizes: the Vec ref
// is hoisted to one cell + the index to a loop-carried scalar; the per-element
// read is `data[i+1]`, no cursor table. This is the shape slice 5's stdlib
// `VecIter` swap activates for real `[T]` (proven here via a wrapper type so the
// slice-3 desugar routing fires without the author's bone arm-removal).
const ZERO_ALLOC_VEC_BACKED_SRC: &str = "
struct MyVec { items: [int] }
struct MyVecIter { items: [int], i: int }

@inline
fn iter(v: MyVec) -> MyVecIter { return MyVecIter { items: v.items, i: 0 } }

@inline
fn next(it: MyVecIter) -> (bool, int) {
    if it.i >= it.items.len { return (false, 0) }
    let v = it.items[it.i]
    it.i = it.i + 1
    return (true, v)
}

fn sum_myvec(v: MyVec) -> int {
    let total = 0
    for x in v { total += x }
    return total
}";

#[test]
fn for_vec_backed_iter_inline_is_zero_alloc() {
    let lua = emit_inlined(ZERO_ALLOC_VEC_BACKED_SRC, "sum_myvec");
    assert!(
        !lua.contains("__rock_create_struct"),
        "vec-backed cursor must scalarize away:\n{lua}"
    );
    assert!(
        !lua.contains('{'),
        "vec-backed @inline iterator loop must leave no table constructor:\n{lua}"
    );
}

// --- Slice 4.5 acceptance gate: the desugared+inlined counted loop is rewritten
// into the codegen CountedLoop shape and prints as a native numeric `for` (FORL
// bytecode). The goto-LOOP form of the same loop measured 1.8x slower on a
// type-stable 2M-element sum (docs/iterator-implementation-plan.md, slice 4.5).
#[test]
fn for_user_iter_inline_emits_forl() {
    let lua = emit_inlined(ZERO_ALLOC_ITER_SRC, "sum_range");
    assert!(
        lua.contains("for "),
        "desugared counted loop must print as a numeric `for`:\n{lua}"
    );
    assert!(
        !lua.contains(">="),
        "the slot exit-compare must be absorbed into the `for` header:\n{lua}"
    );
}

#[test]
fn for_vec_backed_iter_inline_emits_forl() {
    let lua = emit_inlined(ZERO_ALLOC_VEC_BACKED_SRC, "sum_myvec");
    assert!(
        lua.contains("for "),
        "vec-backed desugared loop must print as a numeric `for` (bound hoisted):\n{lua}"
    );
    assert!(
        !lua.contains(">="),
        "the slot exit-compare must be absorbed into the `for` header:\n{lua}"
    );
}

// --- Slice 5 acceptance gate: a REAL `[T]` for-loop routes through the stdlib
// VecIter protocol (no skull template) and still compiles to the same
// zero-alloc native `for` the deleted template emitted. Parity measured at
// ~1.0x, 0 bytes/elem (benchmarks/iterator-zero-alloc/vec_parity.rock).
#[test]
fn for_real_vec_loop_is_forl_zero_alloc() {
    let lua = emit_inlined(
        "fn sum_vec(xs: [int]) -> int {
            let total = 0
            for x in xs { total += x }
            return total
        }",
        "sum_vec",
    );
    assert!(
        lua.contains("for "),
        "real [T] loop must print as a numeric `for` via VecIter + FORL recovery:\n{lua}"
    );
    assert!(
        !lua.contains('{') && !lua.contains("__rock_create_struct"),
        "real [T] loop must not allocate (VecIter cursor scalarizes):\n{lua}"
    );
    assert!(
        !lua.contains("next"),
        "iter/next must be fully inlined, no residual protocol call:\n{lua}"
    );
}

// --- Slice 6 acceptance gate: combinator chains flatten. The bounded
// `map`/`filter` `next` bodies inline via placeholder substitution (the
// BoundTree), the cursors scalarize, and a map-only spine (counted structure
// preserved) recovers FORL. filter keeps the goto skip loop -- its template
// baseline anyway -- but still allocates nothing and leaves no protocol call.
#[test]
fn map_only_chain_is_forl_zero_alloc() {
    let lua = emit_inlined(
        "fn sum_mapped(xs: [int]) -> int {
            let total = 0
            for x in xs.iter().map(x => x * 2) { total += x }
            return total
        }",
        "sum_mapped",
    );
    assert!(
        lua.contains("for "),
        "map-only chain must recover a numeric `for` (FORL):\n{lua}"
    );
    assert!(
        !lua.contains('{') && !lua.contains("__rock_create_struct"),
        "chain must not allocate (MapIter/VecIter cursors scalarize):\n{lua}"
    );
    assert!(
        !lua.contains("next"),
        "chain must fully inline, no residual `next` call:\n{lua}"
    );
}

#[test]
fn map_filter_chain_is_zero_alloc_no_residual_next() {
    let lua = emit_inlined(
        "fn sum_kept(xs: [int]) -> int {
            let total = 0
            for x in xs.iter().map(x => x * 2).filter(x => x > 2) { total += x }
            return total
        }",
        "sum_kept",
    );
    assert!(
        !lua.contains('{') && !lua.contains("__rock_create_struct"),
        "map+filter chain must not allocate (all cursors scalarize):\n{lua}"
    );
    assert!(
        !lua.contains("next"),
        "map+filter chain must fully inline, no residual `next` call:\n{lua}"
    );
}

// --- Skip-loop FORL recovery: a filter/take_while layer's reject edge becomes
// a `continue` (skip) or a jump to the loop exit (stop), so the whole chain
// still rewrites into the codegen CountedLoop shape -- the doc-blessed flat
// `for + skip-branch` (docs/iterator-desugaring-investigation.md C3). The goto
// form of the same filter chain measured 1.82x a hand-fused loop
// (benchmarks/iterator-zero-alloc/chain_parity.rock).
#[test]
fn filter_chain_is_forl_zero_alloc() {
    let lua = emit_inlined(
        "fn sum_kept(xs: [int]) -> int {
            let total = 0
            for x in xs.iter().filter(x => x > 2) { total += x }
            return total
        }",
        "sum_kept",
    );
    assert!(
        lua.contains("for "),
        "filter chain must recover a numeric `for` (skip edge -> continue):\n{lua}"
    );
    assert!(
        !lua.contains('{') && !lua.contains("__rock_create_struct") && !lua.contains("next"),
        "filter chain must stay zero-alloc, fully inlined:\n{lua}"
    );
}

#[test]
fn map_filter_chain_is_forl() {
    let lua = emit_inlined(
        "fn sum_kept(xs: [int]) -> int {
            let total = 0
            for x in xs.iter().map(x => x * 2).filter(x => x > 2) { total += x }
            return total
        }",
        "sum_kept",
    );
    assert!(
        lua.contains("for "),
        "map+filter chain must recover a numeric `for`:\n{lua}"
    );
}

#[test]
fn take_while_chain_is_forl_zero_alloc() {
    let lua = emit_inlined(
        "fn sum_prefix(xs: [int]) -> int {
            let total = 0
            for x in xs.iter().take_while(x => x < 10) { total += x }
            return total
        }",
        "sum_prefix",
    );
    assert!(
        lua.contains("for "),
        "take_while chain must recover a numeric `for` (stop edge -> exit):\n{lua}"
    );
    assert!(
        !lua.contains('{') && !lua.contains("__rock_create_struct") && !lua.contains("next"),
        "take_while chain must stay zero-alloc, fully inlined:\n{lua}"
    );
}

#[test]
fn filter_take_while_mixed_chain_is_forl() {
    let lua = emit_inlined(
        "fn sum_mixed(xs: [int]) -> int {
            let total = 0
            for x in xs.iter().filter(x => x % 2 == 0).take_while(x => x < 100) { total += x }
            return total
        }",
        "sum_mixed",
    );
    assert!(
        lua.contains("for "),
        "filter+take_while chain must recover a numeric `for`:\n{lua}"
    );
}
