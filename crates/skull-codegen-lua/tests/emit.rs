//! Emitted-Lua contracts for the dumb printer. Each lowers a snippet to MIR and
//! asserts its Lua. Naming is raw-arena-index based (`v<raw>`/`c<raw>`/`bb<raw>`),
//! so value numbers skip the indices consumed by effect instructions (`Store`).
//!
//! Execution under luajit is the deeper oracle, wired at the Phase-6 CLI cutover
//! (`rock {run,test} --backend skull`); these pin the textual shape until then.

mod common;

use skull_codegen_lua::{emit_function, CalleeBindingMode, CodegenCtx, LoopShape, TailCallShape};
use skull_mir::mir::FuncRef;

#[test]
fn allocation_activation_scopes_hits_and_callbacks_to_replay_observations() {
    if std::process::Command::new("luajit").arg("-v").output().is_err() {
        eprintln!("luajit not found; skipping execution check");
        return;
    }
    let output = std::env::temp_dir().join(format!(
        "rock-allocation-observations-{}.tsv",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&output);
    let driver = format!(
        r#"{}
local began, ended = {{}}, {{}}
function __rock_observation_profile_begin(observation)
  began[#began + 1] = observation
end
function __rock_observation_profile_end(observation)
  ended[#ended + 1] = observation
end
__rock_allocation_activation_begin()
for observation = 1, 5 do
  __rock_allocation_activation_frame_begin(observation)
  for _ = 1, observation do __rock_alloc_hit("aaaaaaaaaaaaaaaa", false) end
  __rock_allocation_activation_frame_end()
end
assert(table.concat(began, ",") == "2,4,5")
assert(table.concat(ended, ",") == "2,4,5")
assert(__rock_allocation_activation_dump(5))
"#,
        skull_codegen_lua::ALLOCATION_ACTIVATION_PRELUDE,
    );
    let run = std::process::Command::new("luajit")
        .arg("-e")
        .arg(driver)
        .env("SKULL_ALLOCATION_ACTIVATION", &output)
        .env("SKULL_ALLOCATION_OBSERVATIONS", "2,4-5")
        .output()
        .expect("run allocation activation prelude");
    assert!(
        run.status.success(),
        "observation-scoped activation failed: {}",
        String::from_utf8_lossy(&run.stderr),
    );
    let activation = std::fs::read_to_string(&output).expect("activation artifact");
    let _ = std::fs::remove_file(&output);
    assert_eq!(
        activation,
        "rock.allocation-activation.v3\nframes\t3\nobservations\t2,4,5\nsites\t1\naaaaaaaaaaaaaaaa\t11\t11\t3\t5\t0\n"
    );
}

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
    let (func, interner) = common::lower_test_ref(&ws, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    let ctx = CodegenCtx::new(&ws);
    emit_function(&ws, &ctx, &interner, FuncRef { module: mid, func: fid }, &func, None)
}

/// Stdlib-free: literals, vars, `let`, builtin comparisons, control flow.
fn emit(src: &str, fn_name: &str) -> String {
    let ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse");
    emit_dump(vec![ast], fn_name)
}

#[test]
fn numeric_selection_uses_native_minmax() {
    let lua = emit_clean("fn choose(a: float, b: float) -> float { return if a < b { a } else { b } }", "choose");
    assert!(lua.contains("math.min("), "{lua}");
    let lua = emit_clean("fn choose(a: float, b: float) -> float { return if a < b { b } else { a } }", "choose");
    assert!(lua.contains("math.max("), "{lua}");
    let lua = emit_clean("fn choose(a: int, b: int) -> int { return if a < b { a } else { b } }", "choose");
    assert!(lua.contains("math.min("), "{lua}");
    let lua = emit_clean("fn choose(a: int, b: int) -> int { if b > a { a = b }; return a }", "choose");
    assert!(lua.contains("math.max("), "{lua}");
}

#[test]
fn numeric_selection_rejects_effectful_and_inclusive_branches() {
    for src in [
        "fn choose(a: float, b: float) -> float { return if a <= b { a } else { b } }",
        "fn choose(a: float, b: float) -> float { return if a < b { a + 1.0 } else { b } }",
        "fn choose(a: float, b: float, log: [float]) -> float { return if a < b { log.push(a); a } else { b } }",
        "fn choose(a: float, b: float) -> float { let smaller = a < b; a += 10.0; return if smaller { a } else { b } }",
    ] {
        let lua = emit_clean(src, "choose");
        assert!(!lua.contains("math.min(") && !lua.contains("math.max("), "{lua}");
    }
}

#[test]
fn numeric_selection_preserves_special_values_and_guarded_assignment() {
    for (condition, yes, no, native) in [
        ("<", "a", "b", "min"),
        ("<", "b", "a", "max"),
        (">", "a", "b", "max"),
        (">", "b", "a", "min"),
    ] {
        let lua = emit_clean(&format!("@nomangle fn choose(a: float, b: float) -> float {{ return if a {condition} b {{ {yes} }} else {{ {no} }} }}"), "choose");
        assert!(lua.contains(&format!("math.{native}(")), "{lua}");
        let driver = format!(r#"{lua}
local values = {{0/0, math.huge, -math.huge, 0, -0.0, 1, -1, 1.5, -1.5}}
local cases = {{}}
for _,a in ipairs(values) do for _,b in ipairs(values) do
  cases[#cases+1] = {{a,b,a {condition} b and {yes} or {no}}}
end end
for pass=1,2000 do for _,r in ipairs(cases) do
  local a,b = r[1],r[2]
  local got, expected = choose(a,b), r[3]
  assert((got~=got and expected~=expected) or
    (got==expected and (got~=0 or 1/got==1/expected)))
end end
print('selection-ok')
"#);
        let result = std::process::Command::new("luajit").args(["-e", &driver]).output().expect("LuaJIT semantic gate");
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "selection-ok");
    }
    let lua = emit_clean("fn choose(a: float, b: float) -> float { if b > a { a = b }; return a }", "choose");
    assert!(lua.contains("math.max("), "{lua}");
}

#[test]
fn numeric_selection_preserves_writes_before_the_comparison() {
    let lua = emit_clean(r#"
struct Probe { value: float, writes: int }
@nomangle fn choose(a: float, b: float, p: Probe) -> float {
    p.value = b
    p.writes += 1
    return if a < b { a } else { b }
}
"#, "choose");
    assert!(lua.contains("math.min("), "{lua}");
    let driver = format!(r#"{lua}
for i=1,2000 do
  local p = {{value=i/10, writes=0}}
  assert(choose(i/10,i/10+1,p)==i/10)
  assert(p.value==i/10+1 and p.writes==1)
end
print('effects-ok')
"#);
    let result = std::process::Command::new("luajit").args(["-e", &driver]).output().expect("LuaJIT effect gate");
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "effects-ok");
}

/// Like [`emit_std`] but runs `skull_transform::cleanup` (copy-prop + DCE) before
/// emitting -- the artifact the REAL pipeline ships (`mir_emit.rs`). Needed to
/// assert allocation-shape claims (a dead `MakeTuple` left by the raw lowering is
/// removed here, so `no table` assertions reflect what actually runs).
fn emit_clean(src: &str, fn_name: &str) -> String {
    emit_clean_shaped(src, fn_name, TailCallShape::All)
}

/// [`emit_clean`] with an explicit emitted tail-call shape.
fn emit_clean_shaped(src: &str, fn_name: &str, shape: TailCallShape) -> String {
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
    let (mut func, interner, graph) = common::lower_test_ref_with_graph(&ws, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    skull_transform::cleanup(&mut func).unwrap();
    let mut ctx = CodegenCtx::new(&ws);
    ctx.set_tail_call_shape(shape);
    ctx.set_recursive_callees(common::cycle_callees(&graph));
    emit_function(&ws, &ctx, &interner, FuncRef { module: mid, func: fid }, &func, None)
}

/// Emit the graph-reached lowerable instances for the `<test>` module's user
/// roots, run a Lua `driver` expression (e.g. `"sign(-5)"`) under luajit, and
/// return its trimmed stdout. This keeps inlining disabled and delivers the
/// residual stdlib instances instead of depending on a splice for completeness.
/// Skips if luajit is absent so the suite still runs on a bare box.
fn run(src: &str, driver: &str) -> String {
    run_shaped(src, driver, TailCallShape::All)
}

/// [`run`] with an explicit emitted tail-call shape. `recursive_callees` is
/// populated from this harness's own instance graph exactly as
/// `skull::mir_emit` populates it from the whole-program one, so
/// [`TailCallShape::Cycles`] is exercised against real call edges.
fn run_shaped(src: &str, driver: &str, shape: TailCallShape) -> String {
    run_shaped_bound(src, driver, shape, CalleeBindingMode::Global)
}

/// [`run`] under an explicit emitted loop shape.
fn run_looped(src: &str, driver: &str, loops: LoopShape) -> String {
    run_shaped_bound_looped(src, driver, TailCallShape::All, CalleeBindingMode::Global, loops)
}

/// [`run_shaped`] with an explicit callee-binding mode, assembled the way
/// `skull::mir_emit::assemble` assembles a program: every definition first,
/// then the binding pass in component-rank order.
fn run_shaped_bound(
    src: &str,
    driver: &str,
    shape: TailCallShape,
    binding: CalleeBindingMode,
) -> String {
    run_shaped_bound_looped(src, driver, shape, binding, LoopShape::Goto)
}

fn run_shaped_bound_looped(
    src: &str,
    driver: &str,
    shape: TailCallShape,
    binding: CalleeBindingMode,
    loops: LoopShape,
) -> String {
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
    let mut ctx = CodegenCtx::new(&ws);
    ctx.set_tail_call_shape(shape);
    ctx.set_callee_binding(binding);
    ctx.set_loop_shape(loops);
    let roots = ws
        .module(mid)
        .functions
        .iter()
        .map(|(fid, _)| FuncRef { module: mid, func: fid })
        .collect();
    let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(&ws, roots, false, interner.clone());
    ctx.set_recursive_callees(common::cycle_callees(&graph));
    ctx.set_call_sccs(common::call_sccs(&graph));
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(interner);
    let concrete = skull_graph::concrete_body::build(&ws, &graph);
    let mut code_artifact = skull_mir::code_identity::CodeArtifact::default();
    // The binding pass, in ascending component rank -- the same order
    // `skull::mir_emit::callee_binding_pass_lua` emits, and for the same
    // reason: a captured callee must already be its final closure.
    let mut binds: Vec<skull_codegen_lua::CalleeBindingPlan> = Vec::new();
    // Capturing lambdas lower to `setmetatable({..}, __rock_fn_mt)` env records
    // (docs/closure-env-records-plan.md); the record metatable lives in skull's
    // runtime prelude, which this bare harness does not emit -- pull in the
    // canonical definition (one copy, shared with `skull::runtime_prelude`).
    let mut lua = format!(
        "local bit = require('bit')\nlocal ffi = require('ffi')\n{}\n{}\nfunction __nb_checked_get(b, i) if i < 0 or i >= b.len then error('NativeBuffer index out of bounds') end return b.ptr[i] end\nfunction __nb_checked_set(b, i, v) if i < 0 or i >= b.len then error('NativeBuffer index out of bounds') end b.ptr[i] = v end\n",
        skull_codegen_lua::FN_MT_PRELUDE,
        skull_codegen_lua::VEC_CHECK_SHIMS
    );
    for inst in graph.expanded() {
        let f = &ws.module(inst.func.module).functions[inst.func.func];
        if f.inline_body.is_some() {
            continue;
        }
        let Some(plan) = concrete.plans.get(inst) else { continue };
        if let Ok(mut func) = skull_mir::lower::lower_instance_with_plans(
            &ws,
            &scx,
            plan,
            &concrete.plans,
        ) {
            skull_transform::cleanup(&mut func).unwrap();
            skull_transform::recover_counted_loops(&ws, &mut func);
            let name = inst.symbol(&ws, scx.interner());
            let pre_nested = func.nested_fns.len();
            skull_transform::convert_closures(&mut func, &name);
            let mut unit_artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree(&func);
            unit_artifact.set_body_names(&func, &name);
            code_artifact.merge(unit_artifact);
            let plan = skull_codegen_lua::callee_binding_plan(
                &ws, &ctx, scx.interner(), &name, inst.func, &func,
            );
            lua.push_str(&skull_codegen_lua::emit_named(
                &ws, &ctx, scx.interner(), &name, &func, plan.as_ref(),
            ));
            if let Some(plan) = plan {
                binds.push(plan);
            }
            for nf in func.nested_fns[pre_nested..].iter_mut() {
                skull_transform::cleanup_with_nobox(&ws, nf).unwrap();
                skull_transform::recover_counted_loops(&ws, nf);
                lua.push_str(&skull_codegen_lua::emit_named(
                    &ws, &ctx, scx.interner(), nf.name.as_str(), nf, None,
                ));
            }
        }
    }
    binds.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.name.cmp(&b.name)));
    for plan in &binds {
        lua.push_str(&plan.bind_line());
    }
    assert!(
        code_artifact.census.is_total(),
        "incomplete Rock CodeRef census in emitter harness: {:?}",
        code_artifact.census
    );
    let mut entries: Vec<_> = code_artifact
        .entries
        .values()
        .filter(|entry| entry.escaping)
        .collect();
    entries.sort_by_key(|entry| entry.identity.logical_key(&ws, scx.interner()));
    let has_entries = !entries.is_empty();
    if !entries.is_empty() {
        lua.push_str("__rock_code_slot_begin()\n");
        for entry in entries {
            let body = entry.body_name.expect("escaping CodeRef has no emitted body");
            lua.push_str(&format!(
                "__rock_code_slot_stage({}, {}, {})\n",
                skull_codegen_lua::lua_string(
                    &entry.identity.logical_key(&ws, scx.interner())
                ),
                body,
                skull_codegen_lua::lua_string(&entry.compatibility.call_abi),
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
    if has_entries {
        lua.push_str("__rock_code_slot_publish()\n");
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
  ::bb0::
  do return c0 end
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
  ::bb0::
  do return (c0 + c1) end
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
fn exec_uint_exact_wrapping_and_ordering() {
    let src = "
fn add(a: uint, b: uint) -> uint { a + b }
fn mul(a: uint, b: uint) -> uint { a * b }
fn bitnot(a: uint) -> uint { ~a }
fn logical_shift(a: uint) -> uint { a >> 31 }
fn logical_shift_half(a: uint) -> uint { a >> 1 }
fn logical_shift_identity(a: uint) -> uint { a >> 0 }
fn above_signed_half(a: uint, b: uint) -> bool { a > b }";
    assert_eq!(run(src, "add(4294967295, 1)"), "0");
    assert_eq!(run(src, "mul(4294967295, 4294967295)"), "1");
    assert_eq!(run(src, "bitnot(0)"), "4294967295");
    assert_eq!(run(src, "logical_shift(2147483648)"), "1");
    assert_eq!(run(src, "logical_shift_half(2147483648)"), "1073741824");
    assert_eq!(run(src, "logical_shift_identity(2147483648)"), "2147483648");
    assert_eq!(run(src, "above_signed_half(2147483648, 0)"), "true");
}

#[test]
fn exec_uint_entry_values_are_normalized() {
    let src = "fn root(a: uint) -> uint { a }";
    assert_eq!(run(src, "root(-1)"), "4294967295");
}

#[test]
fn exec_uint_numeric_conversion_differs_from_bitcast() {
    let src = "
fn numeric(a: uint) -> int { a as int }
fn bits(a: uint) -> int { bitcast<int>(a) }";
    assert_eq!(run(src, "numeric(2147483648)"), "2147483648");
    assert_eq!(run(src, "bits(2147483648)"), "-2147483648");
}

#[test]
fn exec_uint_ranges_keep_unsigned_loop_and_materialized_types() {
    let src = "
fn last(lo: uint, hi: uint) -> uint {
    let result: uint = 0
    for i in lo..hi { result = i }
    result
}
fn escaped() -> uint {
    let values: [uint] = 2147483648..2147483651
    values[2]
}";
    assert_eq!(run(src, "last(2147483648, 2147483651)"), "2147483650");
    assert_eq!(run(src, "escaped()"), "2147483650");
}

#[test]
fn exec_uint_incdec_wraps() {
    let src = "
fn dec() -> uint { let x: uint = 0; x--; x }
fn inc() -> uint { let x: uint = 4294967295; x++; x }";
    assert_eq!(run(src, "dec()"), "4294967295");
    assert_eq!(run(src, "inc()"), "0");
}

#[test]
fn exec_uint_dynamic_shift_checks_the_runtime_count() {
    let src = "fn shift(value: uint, count: uint) -> uint { value << count }";
    assert_eq!(run(src, "shift(1, 31)"), "2147483648");
}

#[test]
fn exec_native_buffer_uint_uses_unsigned_four_byte_storage() {
    let src = "
fn root() -> uint {
    let b = native_buffer_uint(1)
    b[0] = 4294967295
    b[0]
}";
    assert_eq!(run(src, "root()"), "4294967295");
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
    // coalesces the param `x` (dead after the branch in bb0) with the join cell,
    // so both are `c0`. Single-use pure values are tree-folded into their uses.
    assert_eq!(
        emit("fn sign(x: int) -> int { if x < 0 { 0 } else { 1 } }", "sign"),
        "\
function sign_int_ret_int(c0)
  local r0
  ::bb0::
  if (c0 < (0)) then goto bb1 else goto bb2 end
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

/// Full pre-emit pipeline (mirrors `mir_emit.rs`): plan-backed lower -> inline
/// -> `cleanup` -> emit. The honest artifact for zero-alloc claims: an `@inline`
/// `iter`/`next` is only spliced (and its cursor struct scalarized) once the
/// inliner has run, which `emit_clean` does NOT do.
fn emit_inlined(src: &str, fn_name: &str) -> String {
    emit_inlined_shaped(src, fn_name, LoopShape::Goto)
}

/// [`emit_inlined`] under an explicit emitted loop shape.
fn emit_inlined_shaped(src: &str, fn_name: &str, loops: LoopShape) -> String {
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
    let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(
        &ws,
        ws.module(mid)
            .functions
            .iter()
            .map(|(fid, _)| FuncRef { module: mid, func: fid })
            .collect(),
        false,
        interner.clone(),
    );
    let bodies = skull_graph::concrete_body::build(&ws, &graph);
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(interner);
    let self_ref = FuncRef { module: mid, func: fid };
    let self_inst = skull_mir::instance::Instance::base(self_ref);
    let plan = bodies.plans.get(&self_inst).expect("concrete self plan");
    let mut func = skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &bodies.plans)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    let mut cache = skull_transform::InlineCache::default();
    skull_transform::inline_functions_with_plans(
        &ws,
        &scx,
        self_ref,
        &mut func,
        &mut cache,
        &bodies.plans,
    )
    .expect("inline failed");
    skull_transform::cleanup(&mut func).unwrap();
    skull_transform::recover_counted_loops(&ws, &mut func);
    let name = skull_codegen_lua::mangle_fn(&ws, &ws.module(mid).functions[fid]);
    skull_transform::convert_closures(&mut func, &name);
    let mut ctx = CodegenCtx::new(&ws);
    ctx.set_loop_shape(loops);
    emit_function(&ws, &ctx, scx.interner(), self_ref, &func, None)
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

/// A register or cell local is assigned `nil` right after its last use, so a
/// LuaJIT exit snapshot taken later in the parent trace carries it as a
/// constant instead of a parent SLOAD that needs a register at the side-trace
/// head (`benchmarks/luajit/trace-aborts/calleekill.lua`). A local still live
/// across the branch is untouched.
#[test]
fn dead_locals_are_nil_killed_after_their_last_use() {
    let lua = emit_clean(
        "fn probe(a: int, b: int) -> int { let s = a * b; let t = s + s; if t + 1 > 10 { return t }; return 0 }",
        "probe",
    );
    let lines: Vec<&str> = lua.lines().map(str::trim).collect();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|l| *l == needle)
            .unwrap_or_else(|| panic!("missing `{needle}` in:\n{lua}"))
    };
    let product = at("r0 = c0 * c1");
    let double = at("r0 = r0 + r0");
    let branch = lines
        .iter()
        .position(|l| l.starts_with("if "))
        .unwrap_or_else(|| panic!("missing branch in:\n{lua}"));
    // `c1` (param `b`) dies at the product: killed before the next statement.
    let kill_b = at("c1 = nil");
    assert!(product < kill_b && kill_b < double, "{lua}");
    // `r0` is live across the branch (`t`): no kill between its def and the `if`.
    assert!(
        !lines[double..branch].contains(&"r0 = nil"),
        "live register killed before the branch:\n{lua}"
    );
    // A kill immediately before a plain `return` buys nothing (RET drops the
    // frame): the load of `t` into the return register is followed by no kill.
    let ret_load = at("r0 = c0");
    assert_eq!(lines[ret_load + 1], "do return r0 end", "{lua}");
}

/// A register declared inside a `for` block (a loop-scoped color) is only ever
/// killed inside that block: a kill at the loop's exit label would write a
/// global of the same name.
#[test]
fn loop_scoped_registers_are_never_killed_outside_their_loop() {
    let lua = emit_clean(
        "fn probe(v: [int], target: int) -> int { let n = 0; for i in 0..v.len() { let x = v[i] * 2; let y = x + 1; n = n + y; if x > target { break } }; return n }",
        "probe",
    );
    let lines: Vec<&str> = lua.lines().map(str::trim).collect();
    let for_line = lines.iter().position(|l| l.starts_with("for ")).unwrap_or_else(|| panic!("no for:\n{lua}"));
    let inner_decl = lines[for_line + 1];
    assert!(inner_decl.starts_with("local r"), "loop-scoped registers expected right after the for:\n{lua}");
    let end_line = lines.iter().rposition(|l| *l == "end").unwrap();
    let end_line = lines[..end_line].iter().rposition(|l| *l == "end").unwrap_or_else(|| panic!("no loop end:\n{lua}"));
    let scoped: Vec<&str> = inner_decl["local ".len()..].split(", ").collect();
    for name in scoped {
        let kill = format!("{name} = nil");
        for (i, l) in lines.iter().enumerate() {
            if *l == kill {
                assert!(for_line < i && i < end_line, "loop-scoped `{kill}` outside its for block at line {i}:\n{lua}");
            }
        }
    }
    assert!(lines.iter().any(|l| l.ends_with(" = nil")), "no kills emitted at all:\n{lua}");
}

// -- Emitted tail-call shape (ROCK_SKULL_TAIL_CALLS) ------------------------
//
// `do return f(...) end` compiles to `CALLT`, which LuaJIT's recorder counts
// against `loopunroll` in `lj_record_tailcall`; the switch exists so the
// alternative shapes can be measured on the trace-pressure benchmark. Dropping
// the forward does not introduce an emitted form: it leaves the ordinary path
// to write the same `r<N> = f(args)` / `do return r<N> end` a call outside tail
// position already emits.

/// A leaf call in tail position: `all` forwards it, `none` does not, and
/// `cycles` does not either because the callee is on no call cycle.
#[test]
fn a_non_recursive_tail_call_follows_the_shape_switch() {
    let src = "fn leaf(a: int) -> int { a + 1 }\nfn forward(a: int) -> int { return leaf(a) }";

    let all = emit_clean_shaped(src, "forward", TailCallShape::All);
    assert!(
        all.contains("do return leaf_int_ret_int(c0) end"),
        "expected the forwarded tail call:\n{all}"
    );

    for shape in [TailCallShape::None, TailCallShape::Cycles] {
        let lua = emit_clean_shaped(src, "forward", shape);
        assert!(
            lua.contains("r0 = leaf_int_ret_int(c0)") && lua.contains("do return r0 end"),
            "expected a non-tail call under {shape:?}:\n{lua}"
        );
        assert!(
            !lua.contains("do return leaf_int_ret_int"),
            "expected no forwarded tail call under {shape:?}:\n{lua}"
        );
    }
}

/// A self-recursive tail call keeps the shape under `cycles` -- that is the
/// whole point of the shape, since losing it turns an unbounded tail recursion
/// into a Lua stack overflow.
#[test]
fn a_self_recursive_tail_call_keeps_its_shape_under_cycles() {
    let src = "fn down(n: int, acc: int) -> int { if n == 0 { return acc }\n  return down(n - 1, acc + 1) }";

    for shape in [TailCallShape::All, TailCallShape::Cycles] {
        let lua = emit_clean_shaped(src, "down", shape);
        assert!(
            lua.contains("do return down_int_int_ret_int("),
            "expected the recursive tail call kept under {shape:?}:\n{lua}"
        );
    }
    let none = emit_clean_shaped(src, "down", TailCallShape::None);
    assert!(
        !none.contains("do return down_int_int_ret_int("),
        "expected no forwarded tail call under None:\n{none}"
    );
}

/// Mutual recursion is a two-node component, so both legs stay tail calls under
/// `cycles`. A single self-edge check would miss this.
#[test]
fn a_mutually_recursive_tail_call_keeps_its_shape_under_cycles() {
    let src = "fn is_even(n: int) -> bool { if n == 0 { return true }\n  return is_odd(n - 1) }\n\
               fn is_odd(n: int) -> bool { if n == 0 { return false }\n  return is_even(n - 1) }";

    let even = emit_clean_shaped(src, "is_even", TailCallShape::Cycles);
    assert!(
        even.contains("do return is_odd_int_ret_bool("),
        "expected the mutual-recursion leg kept under Cycles:\n{even}"
    );
    let odd = emit_clean_shaped(src, "is_odd", TailCallShape::Cycles);
    assert!(
        odd.contains("do return is_even_int_ret_bool("),
        "expected the mutual-recursion leg kept under Cycles:\n{odd}"
    );
}

/// The runtime obligation: a tail recursion deeper than the Lua stack survives
/// under every shape that claims to preserve tail-call elimination. Measured on
/// the shipped LuaJIT, a non-tail recursion overflows between 10,000 and 60,000
/// frames, so 200,000 is a real witness rather than a margin.
#[test]
fn deep_tail_recursion_survives_the_tco_preserving_shapes() {
    let src = "fn count_down(n: int, acc: int) -> int { if n == 0 { return acc }\n  return count_down(n - 1, acc + 1) }";
    for shape in [TailCallShape::All, TailCallShape::Cycles] {
        assert_eq!(
            run_shaped(src, "count_down(200000, 0)", shape),
            "200000",
            "deep tail recursion must survive {shape:?}"
        );
    }
}

/// `None` is a measurement arm, not a shippable default: it drops the shape at
/// the recursive call too. Pinned so the reason the default is not `None` stays
/// visible in the suite rather than only in the plan document.
#[test]
fn deep_tail_recursion_overflows_under_the_measurement_only_shape() {
    let src = "fn count_down(n: int, acc: int) -> int { if n == 0 { return acc }\n  return count_down(n - 1, acc + 1) }";
    assert_eq!(
        run_shaped(
            src,
            "(pcall(count_down, 200000, 0))",
            TailCallShape::None
        ),
        "false",
        "None must lose tail-call elimination -- otherwise the switch is not reaching the site"
    );
}

/// Multiple-return forwarding is a value contract, not a tail-call one: it must
/// deliver the same values under every shape.
#[test]
fn a_multi_return_forward_delivers_the_same_values_under_every_shape() {
    let src = "fn pair(a: int) -> (int, int) { (a, a + 1) }\n\
               fn forward_pair(a: int) -> (int, int) { return pair(a) }\n\
               fn total(a: int) -> int { let (x, y) = forward_pair(a); x * 100 + y }";
    for shape in [TailCallShape::All, TailCallShape::None, TailCallShape::Cycles] {
        assert_eq!(
            run_shaped(src, "total(7)", shape),
            "708",
            "multi-return forwarding must not change with {shape:?}"
        );
    }
}

// --- Callee binding as immutable upvalues (docs/backends.md) ------------------
//
// `CalleeBindingMode::Upvalue` defines a function as a factory whose parameters
// are named exactly like the callees they replace, so the body text is
// unchanged and every call reads an immutable upvalue (2 constant-pool slots on
// GC64) instead of a global (5, plus a guard). The assembler emits the
// matching `<name> = __rock_mk_<name>(<callees>)` line after every definition.

/// [`emit_clean`] under an explicit callee-binding mode, with the call-graph
/// components populated from this harness's own instance graph exactly as
/// `skull::mir_emit` populates them from the whole-program one.
fn emit_clean_bound(src: &str, fn_name: &str, mode: CalleeBindingMode) -> String {
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
    let self_ref = FuncRef { module: mid, func: fid };
    let (mut func, interner, graph) = common::lower_test_ref_with_graph(&ws, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    skull_transform::cleanup(&mut func).unwrap();
    let mut ctx = CodegenCtx::new(&ws);
    ctx.set_callee_binding(mode);
    ctx.set_recursive_callees(common::cycle_callees(&graph));
    ctx.set_call_sccs(common::call_sccs(&graph));
    let name = skull_codegen_lua::mangle_fn(&ws, &ws.module(mid).functions[fid]);
    let plan =
        skull_codegen_lua::callee_binding_plan(&ws, &ctx, &interner, &name, self_ref, &func);
    let lua = skull_codegen_lua::emit_function(&ws, &ctx, &interner, self_ref, &func, plan.as_ref());
    match plan {
        Some(plan) => format!("{lua}{}", plan.bind_line()),
        None => lua,
    }
}

/// Under the global-lookup shape the emitted definition is a bare
/// `function <name>(..)` and no factory exists. This is the shape every other
/// test in this file pins, so binding must not reach it.
#[test]
fn callee_binding_global_mode_emits_no_factory() {
    let src = "fn leaf(a: int) -> int { a + 1 }\nfn caller(a: int) -> int { leaf(a) + leaf(a + 1) }";
    let lua = emit_clean_bound(src, "caller", CalleeBindingMode::Global);
    assert!(
        lua.starts_with("function caller_int_ret_int("),
        "expected the unbound definition:\n{lua}"
    );
    assert!(
        !lua.contains(skull_codegen_lua::CALLEE_FACTORY_PREFIX),
        "expected no factory under Global:\n{lua}"
    );
}

/// The bound shape: a factory named for the function, one parameter per
/// distinct callee spelled exactly like the global it replaces, the body
/// verbatim, and the binding line that calls the factory with those globals.
#[test]
fn callee_binding_upvalue_wraps_the_body_in_a_factory() {
    let src = "fn leaf(a: int) -> int { a + 1 }\nfn caller(a: int) -> int { leaf(a) + leaf(a + 1) }";
    let global = emit_clean_bound(src, "caller", CalleeBindingMode::Global);
    let bound = emit_clean_bound(src, "caller", CalleeBindingMode::Upvalue);
    assert!(
        bound.starts_with(
            "__rock_mk_caller_int_ret_int = function(leaf_int_ret_int) return function("
        ),
        "expected the factory header:\n{bound}"
    );
    assert!(
        bound.contains("caller_int_ret_int = __rock_mk_caller_int_ret_int(leaf_int_ret_int)\n"),
        "expected the binding line:\n{bound}"
    );
    // The body is the ONLY copy and is byte-identical to the unbound one: the
    // factory parameter is named like the global, so no call site is rewritten.
    let body = global
        .strip_prefix("function caller_int_ret_int")
        .expect("unbound definition")
        .trim_end_matches('\n');
    assert!(
        bound.contains(&format!("return function{body} end\n")),
        "expected the unchanged body inside the factory:\n{bound}\n--- body ---\n{body}"
    );
}

/// A callee in the caller's own strongly connected component stays a global
/// lookup: binding it would need its final closure before it exists.
#[test]
fn callee_binding_excludes_same_component_callees() {
    let src = "fn leaf(a: int) -> int { a + 1 }\n\
               fn is_even(n: int) -> bool { if n == 0 { return true }\n  is_odd(n - 1) }\n\
               fn is_odd(n: int) -> bool { if n == 0 { return leaf(n) == 1 }\n  is_even(n - 1) }";
    let bound = emit_clean_bound(src, "is_odd", CalleeBindingMode::Upvalue);
    assert!(
        bound.starts_with("__rock_mk_is_odd_int_ret_bool = function(leaf_int_ret_int) return "),
        "expected only the off-component callee bound:\n{bound}"
    );
    assert!(
        !bound.contains("function(is_even") && !bound.contains(", is_even"),
        "expected the mutually recursive callee left global:\n{bound}"
    );
}

/// Direct self-recursion is a one-node component with a self-edge, so the
/// recursive call stays a global lookup too.
#[test]
fn callee_binding_excludes_self_recursion() {
    let src = "fn leaf(a: int) -> int { a + 1 }\n\
               fn down(n: int) -> int { if n == 0 { return leaf(0) }\n  down(n - 1) }";
    let bound = emit_clean_bound(src, "down", CalleeBindingMode::Upvalue);
    assert!(
        bound.starts_with("__rock_mk_down_int_ret_int = function(leaf_int_ret_int) return "),
        "expected the self-recursive call left global:\n{bound}"
    );
}

/// A function with more bindable callees than LuaJIT allows upvalues binds the
/// first [`skull_codegen_lua::MAX_BOUND_CALLEES`] in name order and leaves the
/// rest global. Exceeding the limit is a parse error in the emitted Lua, so
/// this cap is a correctness rule, not a tuning knob.
#[test]
fn callee_binding_caps_at_the_lua_upvalue_limit() {
    let cap = skull_codegen_lua::MAX_BOUND_CALLEES;
    let count = cap + 7;
    let mut src = String::new();
    for i in 0..count {
        // Zero-padded so name order is call order and the cap is predictable.
        src.push_str(&format!("fn leaf{i:03}(a: int) -> int {{ a + {i} }}\n"));
    }
    src.push_str("fn caller(a: int) -> int {\n  let t = 0\n");
    for i in 0..count {
        src.push_str(&format!("  t = t + leaf{i:03}(a)\n"));
    }
    src.push_str("  t\n}\n");
    let bound = emit_clean_bound(&src, "caller", CalleeBindingMode::Upvalue);
    let header = bound.lines().next().expect("factory header");
    let params = header
        .split_once("= function(")
        .expect("factory header")
        .1
        .split_once(") return function")
        .expect("factory header")
        .0;
    assert_eq!(
        params.split(", ").count(),
        cap,
        "expected exactly {cap} bound callees:\n{header}"
    );
    assert!(
        params.starts_with("leaf000_int_ret_int, ") && params.ends_with(&format!("leaf{:03}_int_ret_int", cap - 1)),
        "expected the first {cap} callees in name order:\n{params}"
    );
    for i in cap..count {
        assert!(
            !params.contains(&format!("leaf{i:03}_int_ret_int")),
            "expected `leaf{i:03}` left global:\n{params}"
        );
    }
    // The cap is a correctness rule, not a tuning knob: LuaJIT refuses to parse
    // a function with more than `MAX_BOUND_CALLEES` upvalues, so an uncapped
    // binding would make this program fail to load at all.
    let expected: i64 = (0..count as i64).map(|i| 7 + i).sum();
    assert_eq!(
        run_shaped_bound(&src, "caller(7)", TailCallShape::All, CalleeBindingMode::Upvalue),
        expected.to_string(),
        "a body with more bindable callees than LuaJIT allows upvalues must still load and run"
    );
}

/// The bound program must COMPUTE the same results as the global one, through
/// real LuaJIT, over the two shapes the binding rule is about: a mutually
/// recursive pair (whose legs stay global lookups) and a call through a
/// function value (which has no statically known callee at all).
#[test]
fn callee_binding_upvalue_computes_what_global_computes() {
    let src = "fn leaf(a: int) -> int { a * 3 + 1 }\n\
               fn is_even(n: int) -> bool { if n == 0 { return true }\n  is_odd(n - 1) }\n\
               fn is_odd(n: int) -> bool { if n == 0 { return false }\n  is_even(n - 1) }\n\
               fn apply(f: fn(int) -> int, a: int) -> int { f(a) + f(a + 1) }\n\
               fn total(a: int) -> int {\n\
                 let t = apply(leaf, a)\n\
                 if is_even(a) { t = t + 100 }\n\
                 if is_odd(a + 1) { t = t + 1000 }\n\
                 t + leaf(a)\n\
               }";
    for driver in ["total(6)", "total(7)", "total(0)"] {
        let global = run_shaped_bound(src, driver, TailCallShape::All, CalleeBindingMode::Global);
        let bound = run_shaped_bound(src, driver, TailCallShape::All, CalleeBindingMode::Upvalue);
        assert_eq!(
            bound, global,
            "`{driver}` must not depend on the emitted callee shape"
        );
    }
}

/// `(line, target)` for every `goto <target>` in `lua`, terminator arms
/// included -- a `Branch` renders as `if c then goto X else goto Y end`, so a
/// line-prefix scan misses half of them.
fn goto_refs(lua: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, line) in lua.lines().enumerate() {
        let mut rest = line;
        while let Some(at) = rest.find("goto ") {
            rest = &rest[at + 5..];
            let end = rest.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(rest.len());
            out.push((i, rest[..end].to_string()));
            rest = &rest[end..];
        }
    }
    out
}

/// The line each `::label::` sits on.
fn label_lines(lua: &str) -> std::collections::HashMap<String, usize> {
    lua.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let t = l.trim();
            t.strip_prefix("::")?.strip_suffix("::").map(|n| (n.to_string(), i))
        })
        .collect()
}

/// Every `goto` that jumps BACKWARD -- the shape that makes LuaJIT emit a
/// rangeless `BC_LOOP`.
fn backward_gotos(lua: &str) -> Vec<String> {
    let labels = label_lines(lua);
    goto_refs(lua)
        .into_iter()
        .filter(|(at, t)| labels.get(t).is_some_and(|l| *l < *at))
        .map(|(_, t)| t)
        .collect()
}

// --- ROCK_SKULL_LOOP_SHAPE=structured: a natural loop is a Lua `while` ---
//
// LuaJIT's parser gives a backward `goto` a `BC_LOOP` with `bc_j == -1` and no
// bytecode range, so `rec_setup_root` leaves `bc_min` unset and a root trace
// started at such a loop records past the loop's exit. `while true do .. end`
// gets a real range. These pin the emitted shape that buys it.

/// One simple `while`: the header LABEL stays outside the block so an entering
/// `goto` is still legal, the header's STATEMENTS are the first ones inside it,
/// and the back edge is gone -- the body falls off the end.
#[test]
fn structured_while_is_a_lua_while_with_the_header_label_outside() {
    let lua = emit_inlined_shaped(
        "fn mysum(n: int) -> int { let t = 0; let i = 1; while i <= n { t = t + i; i = i + 1 }; t }",
        "mysum",
        LoopShape::Structured,
    );
    assert!(
        lua.contains("  ::bb1::\n  while true do\n"),
        "header label belongs outside the `while`:\n{lua}"
    );
    assert!(backward_gotos(&lua).is_empty(), "no back edge may remain:\n{lua}");
    assert!(lua.contains("  ::lc1::\n  end\n"), "continue label closes the body:\n{lua}");
}

/// A `continue` is a second back edge. Both become FORWARD `goto`s to the
/// continue label; a backward one would give LuaJIT a second, rangeless
/// `BC_LOOP` inside the loop the `while` is there to close.
#[test]
fn structured_continue_is_a_forward_goto_to_the_continue_label() {
    let lua = emit_inlined_shaped(
        "fn f(n: int) -> int { let t = 0; let i = 0; while i < n { i = i + 1; if i % 3 == 0 { continue }; t = t + i }; t }",
        "f",
        LoopShape::Structured,
    );
    let labels = label_lines(&lua);
    let jumps: Vec<(usize, String)> =
        goto_refs(&lua).into_iter().filter(|(_, t)| t.starts_with("lc")).collect();
    assert_eq!(jumps.len(), 1, "one explicit continue jump expected:\n{lua}");
    let (at, target) = &jumps[0];
    assert!(*at < labels[target], "a continue must jump FORWARD:\n{lua}");
    assert!(backward_gotos(&lua).is_empty(), "no back edge may remain:\n{lua}");
}

/// Several exit targets need no dispatch: Lua forbids jumping INTO a block, not
/// out of one, so each exit stays the plain `goto` it already was.
#[test]
fn structured_several_exits_stay_plain_gotos_out_of_the_block() {
    let lua = emit_inlined_shaped(
        "fn f(n: int) -> int { let t = 0; let i = 0; while true { i = i + 1; if i > n { break }; if t > 40 { return 0 - t }; t = t + i }; t }",
        "f",
        LoopShape::Structured,
    );
    let close = lua.lines().position(|l| l.trim() == "end").expect("the `while` closes");
    let labels = label_lines(&lua);
    let mut exits: Vec<String> = goto_refs(&lua)
        .into_iter()
        .filter(|(at, t)| *at < close && labels.get(t).is_some_and(|l| *l > close))
        .map(|(_, t)| t)
        .collect();
    exits.sort();
    exits.dedup();
    assert!(exits.len() >= 2, "two distinct exit targets expected, got {exits:?}:\n{lua}");
    assert!(backward_gotos(&lua).is_empty(), "no back edge may remain:\n{lua}");
}

/// An iterator loop nested inside another loop is the shape FORL recovery bails
/// on (its preheader is reachable from its body through the enclosing back
/// edge), so it stays a goto loop -- the largest census bucket. The structured
/// shape must reach it.
#[test]
fn structured_reaches_the_nested_iterator_loop_forl_recovery_misses() {
    let src = "\
struct B { slot: int }\n\
struct D { bindings: [B] }\n\
fn f(ds: [D]) -> int { for d in ds { for b in d.bindings { if b.slot < 0 { return 1 } } }; 0 }";
    let goto = emit_inlined_shaped(src, "f", LoopShape::Goto);
    assert!(goto.contains("goto bb"), "sanity: the goto shape has gotos:\n{goto}");
    assert_eq!(
        backward_gotos(&goto).len(),
        1,
        "the nested iterator loop is one backward goto:\n{goto}"
    );
    let lua = emit_inlined_shaped(src, "f", LoopShape::Structured);
    assert!(lua.contains("while true do"), "the nested loop must become a `while`:\n{lua}");
    assert!(lua.contains("::lc"), "with a continue label:\n{lua}");
    assert!(
        backward_gotos(&lua).is_empty(),
        "no backward goto may survive the structured shape:\n{lua}"
    );
}

/// `expand_for` is untouched: a recognized counted loop prints as a Lua `for`
/// under BOTH shapes, and the outer loop here is one.
#[test]
fn structured_preserves_the_counted_for_exactly() {
    let src = "\
struct B { slot: int }\n\
struct D { bindings: [B] }\n\
fn f(ds: [D]) -> int { for d in ds { for b in d.bindings { if b.slot < 0 { return 1 } } }; 0 }";
    let goto = emit_inlined_shaped(src, "f", LoopShape::Goto);
    let structured = emit_inlined_shaped(src, "f", LoopShape::Structured);
    let fors = |s: &str| s.lines().filter(|l| l.trim_start().starts_with("for c")).count();
    assert_eq!(fors(&goto), 1, "one counted `for` expected:\n{goto}");
    assert_eq!(fors(&structured), fors(&goto), "the counted `for` must survive:\n{structured}");
}

/// The default shape emits the same BYTES it emitted before the switch existed.
#[test]
fn goto_shape_emission_is_byte_identical_to_the_default() {
    for src in [
        "fn a(n: int) -> int { let t = 0; let i = 1; while i <= n { t = t + i; i = i + 1 }; t }",
        "fn b(n: int) -> int { let t = 0; let i = 0; while i < n { i = i + 1; if i % 3 == 0 { continue }; t = t + i }; t }",
        "fn c(n: int) -> int { let t = 0; let i = 0; while true { i = i + 1; if i > n { break }; t = t + i }; t }",
    ] {
        let name = &src[3..4];
        assert_eq!(
            emit_inlined_shaped(src, name, LoopShape::Goto),
            emit_inlined(src, name),
            "the explicit `goto` shape must be the default's bytes"
        );
    }
}

/// The deeper oracle: real LuaJIT runs both shapes and agrees, over nested
/// loops, `continue`, an early `break` and an early `return`.
#[test]
fn structured_and_goto_shapes_compute_the_same_under_luajit() {
    let src = "\
@nomangle fn probe(n: int) -> int {\n\
    let total = 0\n\
    let i = 0\n\
    while i < n {\n\
        i = i + 1\n\
        if i % 4 == 0 { continue }\n\
        let j = 0\n\
        while true {\n\
            j = j + 1\n\
            if j > i { break }\n\
            if j % 3 == 0 { continue }\n\
            total = total + j\n\
            if total > 900 { return total }\n\
        }\n\
        total = total + 1\n\
    }\n\
    total\n\
}";
    for n in ["3", "11", "40"] {
        let driver = format!("probe({n})");
        assert_eq!(
            run_looped(src, &driver, LoopShape::Structured),
            run_looped(src, &driver, LoopShape::Goto),
            "the two shapes disagree at n={n}"
        );
    }
}
