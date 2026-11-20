//! Exercise textual and runtime contracts for individual emitted Lua bodies.

mod common;

use bone_core::prelude::ModuleId;
use bone_core::workspace::Workspace;
use skull_codegen_lua::{CodegenCtx, emit_function};
use skull_mir::mir::FuncRef;
use std::sync::LazyLock;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// The stdlib plus `src` as the `<test>` module.
fn stdlib_with(src: &str) -> Vec<skull_parser::Ast> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse"));
    modules
}

fn stdlib_workspace(src: &str) -> (Workspace, ModuleId) {
    let mut modules = stdlib_with(src);
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("user module id");
    (ws, mid)
}

fn declared_name_matches(name: ustr::Ustr, expected: &str) -> bool {
    skull_mir::mangle::declared_name(name.as_str()) == expected
}

fn test_fn(
    ws: &Workspace,
    mid: ModuleId,
    name: &str,
) -> comfy_la_arena::Idx<bone_core::hir::Function> {
    ws.module(mid)
        .functions
        .iter()
        .find(|(_, f)| declared_name_matches(f.name, name))
        .expect("function by name")
        .0
}

/// `luajit -e driver`, returning stdout. Panics with stderr and `context` when
/// it cannot run or exits non-zero.
fn luajit_ok(driver: &str, context: &str) -> String {
    let out = std::process::Command::new("luajit")
        .args(["-e", driver])
        .output()
        .expect("spawn luajit");
    assert!(
        out.status.success(),
        "{}\n{context}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn luajit_loads(lua: &str, context: &str) {
    let path = std::env::temp_dir().join(format!(
        "skull_lua_emit_{}_{:?}.lua",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&path, lua).expect("write emitted Lua");
    let out = std::process::Command::new("luajit")
        .arg(&path)
        .output()
        .expect("spawn luajit");
    let _ = std::fs::remove_file(path);
    assert!(
        out.status.success(),
        "{}\n{context}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn emitted_symbol(lua: &str) -> &str {
    lua.lines()
        .find_map(|line| line.strip_prefix("function "))
        .expect("function definition")
        .split('(')
        .next()
        .unwrap()
}

fn emit_module(
    mut modules: Vec<skull_parser::Ast>,
    fn_name: &str,
) -> (String, Option<skull_parser::CompilerError>) {
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let fid = test_fn(&ws, mid, fn_name);
    let (func, interner) = common::lower_test_ref(&ws, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    let ctx = CodegenCtx::new(&ws);
    let lua = emit_function(
        &ws,
        &ctx,
        &interner,
        FuncRef {
            module: mid,
            func: fid,
        },
        &func,
        None,
    );
    (lua, ctx.take_rejection())
}

fn emit_run(src: &str) -> (String, Option<skull_parser::CompilerError>) {
    emit_module(stdlib_with(src), "run")
}

/// Stdlib-free: literals, vars, `let`, builtin comparisons, control flow.
fn emit(src: &str, fn_name: &str) -> String {
    let ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse");
    emit_module(vec![ast], fn_name).0
}

#[test]
fn prelude_predicates_emit_without_wrapper_calls() {
    let lua = emit_inlined(
        "fn run(x: float, value: any?) -> (bool, bool, bool) { return (isnan(x), isinf(x), is_null(value)) }",
        "run",
    );
    for name in ["isnan_", "isinf_", "is_null("] {
        assert!(!lua.contains(name), "{lua}");
    }
    assert!(lua.contains(" ~= ") && lua.contains("math.abs("), "{lua}");
    assert!(lua.contains(" == (nil)"), "{lua}");
}

#[test]
fn sinking_preserves_dynamic_format_error_order() {
    for result in ["s", "\"[{s}]\""] {
        let lua = emit_clean(
            &format!(
                r#"
@nomangle fn compute(w: int, mark: fn()) -> string {{
    let x = 3.25
    let s = "{{x:{{w}}.2f}}"
    let result = {result}
    mark()
    return result
}}
"#
            ),
            "compute",
        );
        let driver = format!(
            r#"{lua}
for _, width in ipairs({{8, 1000, 1000000000}}) do
    local actual_marks, expected_marks = 0, 0
    local ok, value = pcall(compute, width, function() actual_marks = actual_marks + 1 end)
    local expected_ok, expected = pcall(function()
        local s = string.format('%' .. width .. '.2f', 3.25)
        local result = {control}
        expected_marks = expected_marks + 1
        return result
    end)
    assert(ok == expected_ok)
    assert(actual_marks == expected_marks, 'side effects before error: ' .. actual_marks .. ' vs ' .. expected_marks)
    if ok then assert(value == expected) else assert(actual_marks == 0) end
end
"#,
            control = if result == "s" {
                "s"
            } else {
                "'[' .. s .. ']'"
            }
        );
        luajit_ok(&driver, &lua);
    }
}

#[test]
fn sinking_preserves_checked_numeric_error_order() {
    for (ty, expr, bad) in [
        ("float", "x as uint", "-1"),
        ("uint", "(1 as uint) << x", "32"),
    ] {
        let lua = emit_clean(
            &format!(
                "@nomangle fn compute(x: {ty}, mark: fn()) -> uint {{ let v = {expr}; let result = v + (1 as uint); mark(); return result }}"
            ),
            "compute",
        );
        // Checked casts and shifts bind the shared runtime helpers, which this
        // bare harness does not emit -- pull in the canonical definitions.
        let driver = format!(
            r#"{prelude}
{lua}
for _, input in ipairs({{2, {bad}}}) do
    local marks = 0
    local ok = pcall(compute, input, function() marks = marks + 1 end)
    assert(ok == (input == 2))
    assert(marks == (ok and 1 or 0), 'side effects before checked numeric error: ' .. marks)
end
"#,
            prelude = skull_codegen_lua::UINT_CHECK_PRELUDE
        );
        luajit_ok(&driver, &lua);
    }
}

#[test]
fn numeric_selection_through_local_copies_uses_native_minmax() {
    for (comparison, yes, no, native) in [
        ("<", "d", "b", "min"),
        ("<", "b", "d", "max"),
        (">", "d", "b", "max"),
        (">", "b", "d", "min"),
    ] {
        let lua = emit_clean(
            &format!(
                "@nomangle fn choose(a: float, b: float) -> float {{ let d = a + 1.0; return if d {comparison} b {{ {yes} }} else {{ {no} }} }}"
            ),
            "choose",
        );
        assert!(lua.contains(&format!("math.{native}(")), "{lua}");
        check_selection_values(
            &lua,
            &format!(
                "local d = a + 1.0; if d {comparison} b then expected = {yes} else expected = {no} end"
            ),
        );
    }
    let lua = emit_clean(
        "@nomangle fn choose(a: float, b: float) -> float { let d = if a > b { a } else { b }; return if d < 0.0 { d } else { 0.0 } }",
        "choose",
    );
    assert!(
        lua.contains("math.min(") && lua.contains("math.max("),
        "{lua}"
    );
    check_selection_values(
        &lua,
        "local d; if a > b then d = a else d = b end; if d < 0 then expected = d else expected = 0 end",
    );
}

#[test]
fn numeric_selection_clamp_with_ordered_bounds_uses_native_minmax() {
    let lua = emit_clean(
        "@nomangle fn choose(a: float) -> float { return if a < -1.0 { -1.0 } else if a > 1.0 { 1.0 } else { a } }",
        "choose",
    );
    assert!(lua.contains("math.max(-1.0, math.min(1.0, "), "{lua}");
    check_selection_values(
        &lua,
        "if a < -1 then expected = -1 elseif a > 1 then expected = 1 else expected = a end",
    );
    let lua = emit_inlined(
        "@nomangle fn choose(a: float) -> float { clamp(a, -1.0, 1.0) }",
        "choose",
    );
    assert!(lua.contains("math.max(-1.0, math.min(1.0, "), "{lua}");
    check_selection_values(
        &lua,
        "if a < -1 then expected = -1 elseif a > 1 then expected = 1 else expected = a end",
    );
    for (lo, hi) in [("0.0", "0.0"), ("-0.0", "0.0"), ("0.0", "-0.0")] {
        let lua = emit_clean(
            &format!(
                "@nomangle fn choose(a: float) -> float {{ return if a < {lo} {{ {lo} }} else if a > {hi} {{ {hi} }} else {{ a }} }}"
            ),
            "choose",
        );
        assert!(
            lua.contains("math.max(") && lua.contains("math.min("),
            "{lua}"
        );
        check_selection_values(
            &lua,
            &format!(
                "if a < {lo} then expected = {lo} elseif a > {hi} then expected = {hi} else expected = a end"
            ),
        );
    }
    for src in [
        "@nomangle fn choose(a: float) -> float { return if a < 1.0 { 1.0 } else if a > -1.0 { -1.0 } else { a } }",
        "@nomangle fn choose(a: float, b: float) -> float { return if a < b { b } else if a > 1.0 { 1.0 } else { a } }",
        "@nomangle fn choose(a: float, log: [float]) -> float { return if a < -1.0 { log.push(a); -1.0 } else if a > 1.0 { 1.0 } else { a } }",
    ] {
        let lua = emit_clean(src, "choose");
        assert!(!lua.contains("math.max("), "{lua}");
    }
}

#[test]
fn numeric_selection_clamp_keeps_mutable_frame_bounds() {
    use skull_mir::mir::Inst;
    let ast = skull_parser::parse_module_clean(
        "fn choose(a: float) -> float { let lo = 0.0; lo = -1.0; return if a < lo { lo } else if a > 1.0 { 1.0 } else { a } }",
        ustr::Ustr::from("<test>"),
    ).expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic_pass");
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let fid = test_fn(&ws, mid, "choose");
    let (mut func, interner) = common::lower_test_ref(&ws, mid, fid).expect("lower");
    let lo = func
        .cells
        .iter()
        .find(|(_, c)| c.name.is_some_and(|n| n == "lo"))
        .unwrap()
        .0;
    // Frame reads stay as Load while a converted callback writes the state table.
    let state = func.new_cell(Some(ustr::Ustr::from("state")));
    func.params.insert(0, state);
    let callee = func.alloc_inst(Inst::Global(ustr::Ustr::from("mutate")));
    let receiver = func.alloc_inst(Inst::Load(state));
    let call = func.alloc_inst(Inst::CallValue {
        callee,
        args: vec![receiver],
    });
    let entry = func.entry();
    let at = func.blocks[entry]
        .insts
        .iter()
        .rposition(|id| matches!(func.insts[*id], Inst::Store { cell, .. } if cell == lo))
        .expect("bound initialization")
        + 1;
    func.blocks[entry]
        .insts
        .splice(at..at, [callee, receiver, call]);
    let frame = skull_codegen_lua::FrameCtx {
        s_cell: state,
        fields: [lo].into_iter().collect(),
        slots: [(lo, 0)].into_iter().collect(),
    };
    let lua = skull_codegen_lua::emit_named_framed(
        &ws,
        &CodegenCtx::new(&ws),
        &interner,
        "choose",
        &func,
        Some(&frame),
        None,
    );
    let driver = format!(
        "local flushes = 0\njit.attach(function(what) if what == 'flush' then flushes = flushes + 1 end end, 'trace')\nfunction mutate(s) s.c0 = 0 end\n{lua}\nassert(choose({{}}, -0.5) == 0)\nprint('frame-ok flushes=' .. flushes)"
    );
    luajit_ok(&driver, &lua);
}

fn check_selection_values(lua: &str, reference: &str) {
    let driver = format!(
        r#"{lua}
local flushes = 0
jit.attach(function(what) if what == 'flush' then flushes = flushes + 1 end end, 'trace')
local values = {{0/0, math.huge, -math.huge, 0.0, -0.0, 1, -1, 1.5, -1.5}}
for pass = 1,2000 do for _,a in ipairs(values) do for _,b in ipairs(values) do
  local expected
  {reference}
  local got = choose(a,b)
  assert((got ~= got and expected ~= expected) or
    (got == expected and (got ~= 0 or 1/got == 1/expected)))
end end end
print('selection-ok flushes=' .. flushes)
"#
    );
    let stdout = luajit_ok(&driver, "");
    assert!(stdout.starts_with("selection-ok flushes="), "{stdout}");
}

#[test]
fn numeric_selection_rejects_effectful_and_inclusive_branches() {
    for src in [
        "fn choose(a: float, b: float) -> float { return if a <= b { a } else { b } }",
        "fn choose(a: float, b: float) -> float { return if a < b { a + 1.0 } else { b } }",
        "fn choose(a: float, b: float, log: [float]) -> float { return if a < b { log.push(a); a } else { b } }",
        "fn choose(a: float, b: float) -> float { let smaller = a < b; a += 10.0; return if smaller { a } else { b } }",
        "fn choose(a: float) -> float { return if a < 0.0 { a } else { -0.0 } }",
        "fn choose(a: float, b: float) -> float { return if a < b { b = 7.0; a } else { b } }",
    ] {
        let lua = emit_clean(src, "choose");
        assert!(
            !lua.contains("math.min(") && !lua.contains("math.max("),
            "{lua}"
        );
    }
}

#[test]
fn numeric_selection_preserves_writes_before_the_comparison() {
    let lua = emit_clean(
        r#"
struct Probe { value: float, writes: int }
@nomangle fn choose(a: float, b: float, p: Probe) -> float {
    p.value = b
    p.writes += 1
    return if a < b { a } else { b }
}
"#,
        "choose",
    );
    assert!(lua.contains("math.min("), "{lua}");
    let driver = format!(
        r#"{lua}
for i=1,2000 do
  local p = {{value=i/10, writes=0}}
  assert(choose(i/10,i/10+1,p)==i/10)
  assert(p.value==i/10+1 and p.writes==1)
end
print('effects-ok')
"#
    );
    let result = luajit_ok(&driver, "");
    assert_eq!(result.trim(), "effects-ok");
}

/// The shipped non-inlining body chain over a lowered `<test>` function.
fn convert(
    ws: &Workspace,
    mid: ModuleId,
    fid: comfy_la_arena::Idx<bone_core::hir::Function>,
    func: &mut skull_mir::mir::MirFunc,
) {
    let name = skull_codegen_lua::mangle_fn(ws, &ws.module(mid).functions[fid]);
    skull_transform::body_chain::convert_body(
        ws,
        func,
        &name,
        &skull_transform::body_chain::ChainOptions::PLAIN,
        &mut (),
    )
    .unwrap_or_else(|e| panic!("{:?} failed: {}", e.pass, e.error.message));
}

fn emit_clean(src: &str, fn_name: &str) -> String {
    let (ws, mid) = stdlib_workspace(src);
    let fid = test_fn(&ws, mid, fn_name);
    let (mut func, interner, _) = common::lower_test_ref_with_graph(&ws, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    convert(&ws, mid, fid, &mut func);
    let ctx = CodegenCtx::new(&ws);
    emit_function(
        &ws,
        &ctx,
        &interner,
        FuncRef {
            module: mid,
            func: fid,
        },
        &func,
        None,
    )
}

#[test]
fn spliced_lua_control_counts_toward_the_body_limits() {
    let mut body = String::new();
    for i in 0..32_739 {
        body.push_str(&format!("goto bb{i}\n::bb{i}::\n"));
    }
    let src =
        format!("@inline inline(lua) fn many() = \"\"\"\n{body}\"\"\"\nfn run() {{ many() }}");
    let (_, rejection) = emit_run(&src);
    let error = rejection.expect("Lua name-limit rejection");
    assert!(error.message.contains("LJ_MAX_VSTACK"), "{error:?}");
}

#[test]
fn inert_lua_text_does_not_trigger_body_limit_rejections() {
    let fake_control = "goto bb0\n::bb0::\n".repeat(16_370);
    let fake_assignments = "r0 = 1\n".repeat(32_768);
    let src = format!(
        "@inline inline(lua) fn many() = \"\"\"\n\
         local text = [=[\n{fake_control}]=]\n\
         --[=[\n{fake_control}]=]\n\
         local skipped\ngoto bb0\nskipped = [=[\n{fake_assignments}]=]\n::bb0::\n\
         \"\"\"\nfn run() {{ many() }}"
    );
    let (lua, rejection) = emit_run(&src);
    assert!(
        rejection.is_none(),
        "inert text caused a rejection: {rejection:?}"
    );
    luajit_loads(&lua, "inert long-bracket text must leave loadable Lua");
}

#[test]
fn signed_and_unsigned_constant_bitwise_results_match_lua() {
    let lua = emit_clean(
        "fn masks() -> (int, uint, uint, uint) { \
           let zero: uint = 0; let high: uint = 2147483648; \
           let all: uint = 4294967295; let high_again: uint = 2147483648; \
           return (~(~2147483647), ~(~zero), high | 1, all ^ high_again) \
         }",
        "masks",
    );
    assert!(
        !lua.contains("bit.b"),
        "constant masks should not call bit ops:\n{lua}"
    );
    let symbol = emitted_symbol(&lua);
    luajit_ok(
        &format!(
            "{lua}\nlocal s, a, b, c = {symbol}(); \
             assert(s == 2147483647 and a == 0 and b == 2147483649 and c == 2147483647)"
        ),
        &lua,
    );
}

#[test]
fn direct_calls_pass_literal_scalar_defaults_explicitly() {
    let src = "
fn f(a: int, b: int = 7, c: float = 0.5, d: string = \"x\", e: bool = true) -> int { a + b }
fn go() -> int { f(1) }
fn go_named() -> int { f(1, d: \"y\") }";
    let lua = emit_clean(src, "go");
    assert!(lua.contains("((1), (7), (0.5), (\"x\"), (true))"), "{lua}");
    let lua = emit_clean(src, "go_named");
    assert!(lua.contains("((1), (7), (0.5), (\"y\"), (true))"), "{lua}");
}

#[test]
fn a_non_literal_default_stays_callee_filled() {
    let src = "
global K: int = 3
fn h(a: int, b: int = K) -> int { a + b }
fn go() -> int { h(1) }";
    let lua = emit_clean(src, "go");
    assert!(lua.contains("((1))"), "{lua}");
    let callee = emit_clean(src, "h");
    assert!(
        callee.contains("(nil)"),
        "the callee prologue must remain:\n{callee}"
    );
}

#[test]
fn uint_checks_bind_shared_helpers_as_upvalues() {
    let lua = emit_clean_bound(
        "fn checked(value: float, count: uint) -> uint { (value as uint) << count }",
        "checked",
    );
    assert!(
        lua.contains("function(__rock_checked_u32, __rock_u32_shift_count) return function"),
        "{lua}"
    );
    assert_eq!(
        skull_codegen_lua::emitted_body_name(lua.lines().next().unwrap()),
        Some("checked_float_uint")
    );
}

#[test]
fn tuple_return_and_destructure_use_multiple_values_without_tables() {
    for expr in ["return (true, 42)", "(true, 42)"] {
        let src = format!("fn pair() -> (bool, int) {{ {expr} }}");
        let body = emit_clean(&src, "pair");
        assert!(!body.contains('{'), "{body}");
        assert!(body.contains("do return (true), (42) end"), "{body}");
    }
    let body = emit_clean(
        "fn pair() -> (bool, int) { (true, 42) } \
         fn use_it() -> int { let (ok, n) = pair(); if ok { n } else { 0 } }",
        "use_it",
    );
    assert!(!body.contains('{'), "{body}");
    assert!(
        body.lines()
            .any(|l| l.contains(", ") && l.contains("= pair_0(")),
        "expected a multi-assign call line `a, b = pair_0(...)`:\n{body}"
    );
}

/// The shipped body chain with inlining, which `emit_clean` skips.
fn emit_inlined(src: &str, fn_name: &str) -> String {
    let (ws, mid) = stdlib_workspace(src);
    let fid = test_fn(&ws, mid, fn_name);
    let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(
        &ws,
        ws.module(mid)
            .functions
            .iter()
            .map(|(fid, _)| FuncRef {
                module: mid,
                func: fid,
            })
            .collect(),
        interner.clone(),
    );
    let bodies = skull_graph::concrete_body::build(&ws, &graph);
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(interner);
    let self_ref = FuncRef {
        module: mid,
        func: fid,
    };
    let self_inst = skull_mir::instance::Instance::base(self_ref);
    let plan = bodies.plans.get(&self_inst).expect("concrete self plan");
    let mut func = skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &bodies.plans)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    let mut cache = skull_transform::InlineCache::default();
    let name = skull_codegen_lua::mangle_fn(&ws, &ws.module(mid).functions[fid]);
    let inliner = skull_transform::body_chain::Inliner {
        scx: &scx,
        self_ref,
        cache: &mut cache,
        plans: &bodies.plans,
    };
    skull_transform::body_chain::prepare_body(
        &ws,
        inliner,
        &mut func,
        &name,
        &skull_transform::body_chain::ChainOptions::PLAIN,
        &mut (),
    )
    .unwrap_or_else(|e| panic!("{:?} failed: {}", e.pass, e.error.message));
    let ctx = CodegenCtx::new(&ws);
    emit_function(&ws, &ctx, scx.interner(), self_ref, &func, None)
}

fn assert_counted_zero_alloc(lua: &str) {
    assert!(lua.contains("for "), "{lua}");
    for forbidden in ["{", "__rock_create_struct", "next"] {
        assert!(!lua.contains(forbidden), "unexpected {forbidden}:\n{lua}");
    }
}

#[test]
fn non_counted_loop_records_at_a_while_header() {
    let lua = emit_clean(
        r#"
@nomangle fn scan_until_break(n: int) -> int {
    let i = n
    let sum = 0
    while i > 0 {
        i -= 1
        if i == 5 { continue }
        if i == 2 { break }
        sum += i
    }
    return sum
}
"#,
        "scan_until_break",
    );
    assert!(lua.contains("while ("), "{lua}");
    assert!(!lua.contains("goto w"), "{lua}");
    luajit_ok(
        &format!(
            "{lua}\nassert(scan_until_break(0) == 0 and scan_until_break(2) == 1 and scan_until_break(8) == 20)"
        ),
        &lua,
    );
}

#[test]
fn for_real_vec_loop_is_forl_zero_alloc_with_raw_reads() {
    let lua = emit_inlined(
        "fn sum_vec(xs: [int]) -> int {
            let total = 0
            for x in xs { total += x }
            return total
        }",
        "sum_vec",
    );
    assert_counted_zero_alloc(&lua);
    assert!(lua.contains("._data["), "{lua}");
    assert!(!lua.contains("__vec_checked_get("), "{lua}");
}

#[test]
fn iterator_chain_lambdas_inline_into_the_counted_loop() {
    let lua = emit_inlined(
        "fn scaled_odd_sum(xs: [int], k: int) -> int {
            return xs.iter().map(x => x * k).filter(x => x % 2 != 0).sum()
        }",
        "scaled_odd_sum",
    );
    assert_counted_zero_alloc(&lua);
    for forbidden in ["__rock_code_slot_ref", "setmetatable", "lambda"] {
        assert!(!lua.contains(forbidden), "unexpected {forbidden}:\n{lua}");
    }
}

#[test]
fn any_with_an_early_return_is_a_counted_loop() {
    let lua = emit_inlined(
        "fn has(xs: [int], k: int) -> bool { return xs.any(x => x == k) }",
        "has",
    );
    assert_counted_zero_alloc(&lua);
    assert!(!lua.contains("goto w"), "{lua}");
}

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
    // `t`, never written again, is a value register, not a cell.
    let double = at("r0 = r0 + r0");
    let branch = lines
        .iter()
        .position(|l| l.starts_with("if "))
        .unwrap_or_else(|| panic!("missing branch in:\n{lua}"));
    // `c1` (param `b`) dies at the product: killed before the next statement.
    let kill_b = at("c1 = nil");
    assert!(product < kill_b && kill_b < double, "{lua}");
    // `r0` is live across the branch (`t`): no kill between its def and the `if`.
    assert!(!lines[double..branch].contains(&"r0 = nil"), "{lua}");
    // A kill immediately before a plain `return` buys nothing (RET drops the
    // frame): `t` is returned straight from its register with no kill before it.
    let ret = at("do return r0 end");
    assert_eq!(ret, branch + 1, "{lua}");
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
    let for_line = lines
        .iter()
        .position(|l| l.starts_with("for "))
        .unwrap_or_else(|| panic!("no for:\n{lua}"));
    let inner_decl = lines[for_line + 1];
    assert!(inner_decl.starts_with("local r"), "{lua}");
    let end_line = lines.iter().rposition(|l| *l == "end").unwrap();
    let end_line = lines[..end_line]
        .iter()
        .rposition(|l| *l == "end")
        .unwrap_or_else(|| panic!("no loop end:\n{lua}"));
    let scoped: Vec<&str> = inner_decl["local ".len()..].split(", ").collect();
    for name in scoped {
        let kill = format!("{name} = nil");
        for (i, l) in lines.iter().enumerate() {
            if *l == kill {
                assert!(for_line < i && i < end_line, "{lua}");
            }
        }
    }
    assert!(lines.iter().any(|l| l.ends_with(" = nil")), "{lua}");
}

/// [`emit_clean`] with its callee binding plan.
fn emit_clean_bound(src: &str, fn_name: &str) -> String {
    let (ws, mid) = stdlib_workspace(src);
    let fid = test_fn(&ws, mid, fn_name);
    let self_ref = FuncRef {
        module: mid,
        func: fid,
    };
    let (mut func, interner, graph) = common::lower_test_ref_with_graph(&ws, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    convert(&ws, mid, fid, &mut func);
    let mut ctx = CodegenCtx::new(&ws);
    ctx.set_call_sccs(common::call_sccs(&graph));
    let name = skull_codegen_lua::mangle_fn(&ws, &ws.module(mid).functions[fid]);
    let plan = skull_codegen_lua::callee_binding_plan(&ws, &ctx, &interner, &name, self_ref, &func);
    let mut lua =
        skull_codegen_lua::emit_function(&ws, &ctx, &interner, self_ref, &func, plan.as_ref());
    if let Some(plan) = plan {
        plan.write_bind_line(&mut lua);
    }
    lua
}

#[test]
fn read_only_nullable_value_lookup_does_not_rebuild_the_wrapper() {
    let prelude = r#"
value SlotMetadata { names: [string], by_name: Map<string, int> }
global METADATA: Map<string, SlotMetadata> = map_new()
"#;
    for (name, body) in [
        (
            "lookup",
            r#"fn lookup(shader: string, name: string) -> int {
    let slots = METADATA.get(shader)
    if slots == null || slots.names.len() == 0 { return -1 }
    return slots.by_name.get(name) ?? -1
}"#,
        ),
        (
            "presence",
            r#"fn presence(shader: string) -> int {
    let slots = METADATA.get(shader)
    if slots == null { return -1 }
    return 0
}"#,
        ),
        (
            "unwrap_field",
            r#"fn unwrap_field(shader: string) -> int {
    let slots = METADATA.get(shader)
    return slots.unwrap().names.len()
}"#,
        ),
    ] {
        let src = format!("{prelude}{body}");
        let lua = emit_clean_bound(&src, name);
        assert!(!lua.contains("SlotMetadata_vt("), "{lua}");
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
            let end = rest
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
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
            t.strip_prefix("::")?
                .strip_suffix("::")
                .map(|n| (n.to_string(), i))
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

/// The inner loop's preheader is reachable from its body through the outer
/// back edge, but it is not a back edge of the inner loop.
#[test]
fn nested_iterator_loop_is_recovered_as_a_counted_for() {
    let src = "\
struct B { slot: int }\n\
struct D { bindings: [B] }\n\
fn f(ds: [D]) -> int { for d in ds { for b in d.bindings { if b.slot < 0 { return 1 } } }; 0 }";
    let lua = emit_inlined(src, "f");
    let fors = lua
        .lines()
        .filter(|l| l.trim_start().starts_with("for "))
        .count();
    assert_eq!(fors, 2, "{lua}");
    assert!(backward_gotos(&lua).is_empty(), "{lua}");
}

/// A `[T]` element read is the backing read itself. Only a slot that reads
/// nil goes on to the checked helper, which raises when the index is out of
/// bounds and otherwise returns that nil; a `false` slot is the element, so a
/// `[bool]` of mostly `false` makes no call per element.
#[test]
fn vec_index_read_is_direct_with_a_checked_fallback() {
    let lua = emit("fn f(xs: [int], i: int) -> int { return xs[i] }", "f");
    assert!(
        lua.contains("._data[c1 + 1] or (c0._data[c1 + 1] == nil and __vec_checked_get(c0, c1))"),
        "{lua}"
    );
    let lua = emit("fn first(xs: [int]) -> int { return xs[0] }", "first");
    assert!(
        lua.contains("._data[1] or (c0._data[1] == nil and __vec_checked_get("),
        "{lua}"
    );
    let lua = emit("fn f(xs: [bool], i: int) -> bool { return xs[i] }", "f");
    assert!(lua.contains(" == nil and __vec_checked_get("), "{lua}");
}

/// The occupancy read is raw under the loop bound; payload reads keep the
/// fallback because a host getter can shrink the column.
#[test]
fn arena_occupancy_reads_are_raw() {
    let lua = emit_inlined(
        "struct Mote { life: int }
        fn count(us: Arena<Mote>) -> int {
            let t = 0
            for (_, _) in us { t += 1 }
            return t
        }",
        "count",
    );
    assert!(!lua.contains("__vec_checked_get("), "{lua}");
    let lua = emit_inlined(
        "struct Mote { life: int }
        fn sum(us: Arena<Mote>) -> int {
            let t = 0
            for (_, m) in us { t += m.life }
            return t
        }",
        "sum",
    );
    let occupancy = lua.lines().find(|l| l.contains("if ")).unwrap_or_default();
    assert!(
        occupancy.contains(" == nil and __vec_checked_get("),
        "{lua}"
    );
}

/// A call in the body that may shrink the vector keeps the loop's element
/// read on its checked fallback.
#[test]
fn a_shrinking_call_keeps_the_loop_element_fallback() {
    let lua = emit_inlined(
        "fn bump(xs: [int], x: int) -> int { if x < 0 { xs.pop() }; return x + 1 }
        fn sum_vec(xs: [int]) -> int {
            let total = 0
            for x in xs { total += bump(xs, x) }
            return total
        }",
        "sum_vec",
    );
    assert!(lua.contains("for "), "{lua}");
    assert!(
        lua.contains(" + 1] or (") && lua.contains(" == nil and __vec_checked_get("),
        "{lua}"
    );
}

/// An index whose operands are not plain locals would be evaluated twice by
/// the direct form, so it keeps the one call.
#[test]
fn vec_index_read_of_a_computed_operand_keeps_the_call() {
    let src = "
fn pick(xs: [int]) -> int { return 1 }
fn f(xs: [[int]], i: int) -> int { return xs[pick(xs[0])][i] }";
    let lua = emit(src, "f");
    assert_eq!(lua.matches("pick").count(), 1, "{lua}");
}

#[test]
fn whole_gc_snapshots_read_only_demanded_leaves() {
    let src = include_str!("../../../tests/values/copies.rock");
    for name in ["consume", "read_only", "read_produced"] {
        let lua = emit_clean(src, name);
        for unused in [
            ".ignored",
            "unused7",
            ".spare",
            "Detail_ct(",
            "Snapshot_vt(",
        ] {
            assert!(!lua.contains(unused), "{unused} in {name}:\n{lua}");
        }
    }
}

#[test]
fn ordinary_nullable_cdata_enum_return_keeps_boxed_transport() {
    let src = r#"
enum PackedResult { Number(int), Empty }
fn packed_result(present: bool) -> PackedResult? {
    if !present { return null }
    return PackedResult::Number(7)
}
"#;
    let lua = emit_clean(src, "packed_result");
    assert!(
        lua.contains("return (nil)"),
        "nullable enum changed transport: {lua}"
    );
    assert!(lua.contains("_ct("), "nullable enum lost its box: {lua}");
}

#[test]
fn fixed_scalar_nullable_value_return_keeps_boxed_transport() {
    let src = r#"
value Pair { x: int, y: int }
@scalar_abi @nomangle fn fixed_pair(present: bool) -> Pair? {
    if !present { return null }
    return Pair { x: 20, y: 21 }
}
"#;
    let lua = emit_clean(src, "fixed_pair");
    assert!(
        lua.contains("_ct("),
        "fixed nullable return changed transport: {lua}"
    );
}

#[test]
fn unreachable_block_traps_instead_of_returning() {
    let lua = emit_clean(
        "enum Dir { Up, Down } fn pick(d: Dir) -> int { match d { Dir::Up => { return 1 } Dir::Down => { return 2 } } }",
        "pick",
    );
    let symbol = emitted_symbol(&lua);
    // A value outside the enum reaches the arm chain's dead end; it must not
    // read as an early return of nil.
    let driver = format!(
        r#"{lua}
local ok, err = pcall({symbol}, "Sideways")
assert(not ok, "an unreachable block returned")
assert(tostring(err):find("unreachable", 1, true), tostring(err))
"#
    );
    luajit_ok(&driver, &lua);
}

/// A body that still carries what an earlier pass should have removed is a
/// compiler bug. The MIR verifier catches it in debug builds; in release the
/// emitter must report it as a diagnostic, not abort the process.
#[test]
fn pass_residue_reaching_emission_is_reported_not_a_panic() {
    use skull_mir::mir::{CoroHookKind, Inst, Lit, MirFunc, Term};

    type Inject = fn(&mut MirFunc);
    let cases: [(&str, Inject); 5] = [
        ("closure conversion", |mf| {
            let entry = mf.entry();
            let body = mf.new_block();
            let one = mf.emit(body, Inst::Const(Lit::Int(1)));
            mf.set_term(body, Term::Return(vec![one]));
            let lambda = mf.emit(
                entry,
                Inst::Lambda {
                    params: vec![],
                    entry: body,
                },
            );
            mf.set_term(entry, Term::Return(vec![lambda]));
        }),
        ("coroutine transform", |mf| {
            let entry = mf.entry();
            let resume = mf.new_block();
            mf.set_term(resume, Term::Return(vec![]));
            mf.set_term(
                entry,
                Term::Yield {
                    values: vec![],
                    resume,
                },
            );
        }),
        ("coroutine transform", |mf| {
            let entry = mf.entry();
            // Any placed value serves: the emitter rejects before reading it.
            let lambda = mf.emit(entry, Inst::Const(Lit::Int(0)));
            mf.emit(entry, Inst::CoroSetInner(lambda));
            mf.set_term(entry, Term::Return(vec![]));
        }),
        ("coroutine transform", |mf| {
            let entry = mf.entry();
            // Any placed value serves: the emitter rejects before reading it.
            let lambda = mf.emit(entry, Inst::Const(Lit::Int(0)));
            mf.emit(
                entry,
                Inst::CoroHook {
                    kind: CoroHookKind::OnKilled,
                    lambda,
                },
            );
            mf.set_term(entry, Term::Return(vec![]));
        }),
        ("residual-bound-call gate", |mf| {
            let entry = mf.entry();
            mf.emit(
                entry,
                Inst::CallBoundMulti {
                    dests: vec![],
                    generic_idx: 0,
                    args: vec![],
                },
            );
            mf.set_term(entry, Term::Return(vec![]));
        }),
    ];
    let mut modules = vec![
        skull_parser::parse_module_clean("fn f() {}", ustr::Ustr::from("<test>")).expect("parse"),
    ];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let interner = skull_mir::monoty::MonoInterner::new();
    for (pass, inject) in cases {
        let mut mf = MirFunc::new(ustr::Ustr::from("residue"));
        inject(&mut mf);
        let ctx = CodegenCtx::new(&ws);
        skull_codegen_lua::emit_named(&ws, &ctx, &interner, "residue", &mf, None);
        let err = ctx
            .take_rejection()
            .unwrap_or_else(|| panic!("residue from the {pass} must be rejected"));
        assert!(
            err.message.contains("internal compiler error") && err.message.contains(pass),
            "{}",
            err.message
        );
    }
}

#[test]
fn counted_loop_store_within_the_length_is_raw() {
    let lua = emit_inlined(
        "fn fill(xs: [int]) {
            for i in 0..xs.len() { xs[i] = i * 2 }
        }",
        "fill",
    );
    assert!(lua.contains("for "), "{lua}");
    assert!(lua.contains("._data[") && lua.contains(" + 1] = "), "{lua}");
    assert!(!lua.contains("__vec_checked_set("), "{lua}");
}

/// A range guard dominating the store proves it; a second vector of unknown
/// length keeps its check.
#[test]
fn a_dominating_range_guard_proves_the_store() {
    let lua = emit_inlined(
        "fn set_slot(xs: [int], ys: [int], i: int, v: int) -> bool {
            if i < 0 { return false }
            if i >= xs.len() { return false }
            xs[i] = v
            ys[i] = v
            return true
        }",
        "set_slot",
    );
    assert_eq!(lua.matches("__vec_checked_set(").count(), 1, "{lua}");
    assert!(lua.contains("._data[") && lua.contains(" + 1] = "), "{lua}");
}

/// A column-stored element access inside a counted loop needs no element
/// check: the leaf reads index their columns by the bare slot.
#[test]
fn column_access_in_a_counted_loop_drops_the_element_check() {
    let lua = emit_inlined(
        "nobox value Mote { x: float, y: float, life: int }
        fn sum(ms: [Mote]) -> float {
            let t = 0.0
            for i in 0..ms.len() { t += ms[i].x }
            return t
        }",
        "sum",
    );
    assert!(lua.contains("for "), "{lua}");
    assert!(!lua.contains("__vec_checked_index("), "{lua}");
    assert!(lua.contains("(c0.x)[(c2 + 1)]"), "{lua}");
}

#[test]
fn column_index_checks_require_a_strict_bound() {
    for (params, body, checked) in [
        ("", "if ms.len() > 0 { return ms[0].x }; return 0.0", false),
        (
            "",
            "if ms.len() > 0 { return ms[0].x + ms[ms.len() - 1].x }; return 0.0",
            true,
        ),
        ("", "return ms[ms.len() - 1].x", true),
        (", k: int", "return ms[k % ms.len()].x", true),
        (
            ", k: int",
            "if ms.len() > 0 { return ms[k % ms.len()].x }; return 0.0",
            false,
        ),
    ] {
        let lua = emit_inlined(
            &format!(
                "nobox value Mote {{ x: float, y: float, life: int }} fn get(ms: [Mote]{params}) -> float {{ {body} }}"
            ),
            "get",
        );
        assert_eq!(lua.contains("__vec_checked_index("), checked, "{lua}");
    }
}

/// A counted loop's store stays raw across a call only when the callee's
/// body, not its `@pure` annotation, proves the vector untouched; a `push`
/// on another vector may run a foreign `__newindex`.
#[test]
fn loop_store_checks_follow_callee_effects() {
    let proven = |src: &str| !emit_inlined(src, "fill").contains("__vec_checked_set(");
    let kept = |src: &str| emit_inlined(src, "fill").contains("__vec_checked_set(");
    assert!(proven(
        "fn weigh(xs: [int], i: int) -> int { return xs.len() + i }
        fn fill(xs: [int]) { for i in 0..xs.len() { xs[i] = weigh(xs, i) } }"
    ));
    assert!(kept(
        "struct Counter { n: int }
        fn bump(c: Counter) -> int { c.n = c.n + 1; return c.n }
        fn fill(xs: [int], c: Counter) { for i in 0..xs.len() { xs[i] = bump(c) } }"
    ));
    assert!(proven(
        "extern(ffi, \"probe\") fn probe_tick(i: int) -> int
        fn fill(xs: [int]) { for i in 0..xs.len() { xs[i] = probe_tick(i) } }"
    ));
    assert!(proven(
        "@nowrite inline(lua) fn stamp(i: int) -> int = \"return i * 2\"
        fn fill(xs: [int]) { for i in 0..xs.len() { xs[i] = stamp(i) } }"
    ));
    assert!(proven(
        "@writes_elems inline(lua) fn poke(ys: [int], i: int) -> int = \"ys._data[1] = i return i\"
        fn fill(xs: [int], ys: [int]) { for i in 0..xs.len() { xs[i] = poke(ys, i) } }"
    ));
    assert!(proven(
        "fn ping(n: int) -> int { if n <= 0 { return 0 }; return pong(n - 1) }
        fn pong(n: int) -> int { return ping(n - 1) + 1 }
        fn fill(xs: [int]) { for i in 0..xs.len() { xs[i] = ping(i) } }"
    ));
    assert!(kept(
        "fn shrink(xs: [int]) -> int { xs.pop(); return 0 }
        fn fill(xs: [int]) { for i in 0..xs.len() { xs[i] = shrink(xs) } }"
    ));
    assert!(kept(
        "struct Bag { items: [int] }
        fn swap(b: Bag) -> int { b.items = [1]; return 0 }
        fn fill(b: Bag) { for i in 0..b.items.len() { b.items[i] = swap(b) } }"
    ));
    assert!(kept(
        "fn apply(f: fn() -> int) -> int { return f() }
        fn fill(xs: [int]) { for i in 0..xs.len() { xs[i] = apply(() => 1) } }"
    ));
    assert!(kept(
        "inline(lua) fn raw(i: int) -> int = \"return i\"
        fn fill(xs: [int]) { for i in 0..xs.len() { xs[i] = raw(i) } }"
    ));
    assert!(kept(
        "fn shrink(xs: [int]) { xs.pop() }
        fn fill(xs: [int]) { for i in 0..xs.len() { xs[i] = i; shrink(xs) } }"
    ));
    assert!(proven(
        "@pure fn weight(i: int) -> int { return i * 3 }
        fn fill(xs: [int]) { for i in 0..xs.len() { xs[i] = weight(i) } }"
    ));
    assert!(kept(
        "@debug_impure fn debug_pop(xs: [int]) -> int { xs.pop(); return 0 }
        @pure fn via(xs: [int]) -> int { return debug_pop(xs) }
        fn fill(xs: [int]) { for i in 0..xs.len() { xs[i] = via(xs) } }"
    ));
    assert!(kept(
        "fn fill(xs: [int], out: [int]) {
            for i in 0..xs.len() { xs[i] = i; if i % 7 == 0 { out.push(i) } }
        }"
    ));
}

#[test]
fn native_writes_invalidate_element_value_bounds() {
    for writer in ["nb_set(indices, 0, 999)", "overwrite(indices.ptr, 999)"] {
        let src = format!(
            r#"
extern(ffi, "probe") fn overwrite(p: Ptr<int>, value: int)
fn lookup(xs: [int], indices: NativeBuffer<int>) -> int {{
    if indices[0] < 0 || indices[0] >= xs.len() {{ return 0 }}
    {writer}
    return xs[indices[0]]
}}
"#
        );
        let lua = emit_inlined(&src, "lookup");
        assert!(lua.contains("__vec_checked_get("), "{writer}:\n{lua}");
    }
}

#[test]
fn map_remove_invalidates_a_bound_read_from_the_map() {
    let lua = emit_inlined(
        "fn lookup(xs: [int], m: Map<string, int>, key: string) -> int {
            if m[key] < 0 || m[key] >= xs.len() { return 0 }
            m.remove(key)
            return xs[m[key]]
        }",
        "lookup",
    );
    assert!(lua.contains("__vec_checked_get("), "{lua}");
}

/// A NativeBuffer access a loop proves indexes the C array raw; the stdlib
/// slot helpers' declared result facts drop the column check after a
/// column-map lookup.
#[test]
fn native_buffer_and_column_map_accesses_drop_their_checks() {
    let lua = emit_inlined(
        "fn fill(b: NativeBuffer<float>) {
            for i in 0..b.len { b[i] = b[i] + 1.0 }
        }",
        "fill",
    );
    assert!(lua.contains(".ptr[c"), "{lua}");
    assert!(!lua.contains("__nb_checked_"), "{lua}");
    let lua = emit_inlined(
        "fn touch(b: NativeBuffer<float>, i: int) -> float { return b[i] }",
        "touch",
    );
    assert!(lua.contains("__nb_checked_get("), "{lua}");
    let lua = emit_inlined(
        "nobox value Mote { x: float, y: float }
        fn lookup(m: Map<string, Mote>, k: string) -> float {
            let v = m.get(k)
            if v != null { return v.x }
            return -1.0
        }",
        "lookup",
    );
    assert!(lua.contains("__map_col_slot_of("), "{lua}");
    assert!(!lua.contains("__vec_checked_index("), "{lua}");
}

/// Lower `src`, mark the binding named `field` as a coroutine-frame field
/// behind a fresh state parameter, and emit.
fn emit_framed_let(src: &str, fn_name: &str, field: &str) -> String {
    let ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic_pass");
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let fid = test_fn(&ws, mid, fn_name);
    let (mut func, interner) = common::lower_test_ref(&ws, mid, fid).expect("lower");
    let cell = func
        .cells
        .iter()
        .find(|(_, c)| c.name.is_some_and(|n| n == field))
        .expect("frame field binding")
        .0;
    let state = func.new_cell(Some(ustr::Ustr::from("state")));
    func.params.insert(0, state);
    let frame = skull_codegen_lua::FrameCtx {
        s_cell: state,
        fields: [cell].into_iter().collect(),
        slots: [(cell, 0)].into_iter().collect(),
    };
    skull_codegen_lua::emit_named_framed(
        &ws,
        &CodegenCtx::new(&ws),
        &interner,
        fn_name,
        &func,
        Some(&frame),
        None,
    )
}

#[test]
fn frame_stores_skip_temporaries_and_guard_only_nullable_values() {
    for (src, guarded) in [
        (
            "fn step(x: float) -> bool { let acc = true; acc = x > 1.0; return acc }",
            false,
        ),
        (
            "fn step(x: int?) -> int? { let acc: int? = 7; acc = x; return acc }",
            true,
        ),
    ] {
        let lua = emit_framed_let(src, "step", "acc");
        assert!(!lua.contains("do local"), "{lua}");
        let guard = if guarded { "~= nil then" } else { "~= nil" };
        assert_eq!(lua.contains(guard), guarded, "{lua}");
        assert!(lua.contains(".c0 = "), "{lua}");
    }
}

#[test]
fn string_interpolation_holes_concatenate_without_tostring() {
    let lua = emit_clean(
        r#"@nomangle fn greet(name: string, nick: string?, n: int) -> string {
    return "hi {name} aka {nick} #{n}"
}"#,
        "greet",
    );
    assert_eq!(lua.matches("tostring(").count(), 1, "{lua}");
    assert!(lua.contains("tostring(c2)"), "{lua}");
    let out = luajit_ok(
        &format!("{lua}\nio.write(greet('a', nil, 3), '|', greet('b', 'c', 4))"),
        &lua,
    );
    assert_eq!(out, "hi a aka nil #3|hi b aka c #4");
}

#[test]
fn vec_literal_carries_no_type_tag() {
    let (lua, rejection) = emit_run("fn run(x: int) -> [int] {\n    [x, 2]\n}\n");
    assert!(rejection.is_none(), "{rejection:?}");
    assert!(lua.contains("{_data = {c0, (2)}, len = 2}"), "{lua}");
    assert!(!lua.contains("__ty"), "{lua}");
}

#[test]
fn nullable_value_lookup_branch_uses_presence_without_boxing() {
    let lua = emit_clean(
        r#"
value Entry { key: int, refs: [int] }
@nomangle fn lookup(m: Map<int, Entry>, k: int) -> int {
    if m.get(k) != null { return 7 }
    return 3
}
"#,
        "lookup",
    );
    assert!(!lua.contains("Entry_vt("), "{lua}");
}

#[test]
fn numeric_for_requires_an_invariant_bound_and_unit_step() {
    let src = include_str!("../../../tests/control_flow/counted_bounds.rock");
    for (name, fors, general) in [
        ("local_bound", 0, true),
        ("vec_bound", 0, true),
        ("field_bound", 0, true),
        ("global_bound", 0, true),
        ("closure_bound", 0, true),
        ("mutable_step", 0, true),
        ("assigned_counter", 0, true),
        ("assigned_parameter", 0, true),
        ("invariant_bound", 1, false),
        ("range_snapshot", 1, false),
        ("bounds_in_nested_loops", 1, true),
        ("reentered_unreset_counter", 1, true),
        ("reentered_reset_counter", 1, true),
        ("locally_reset_counter", 2, false),
    ] {
        let lua = emit_clean(src, name);
        assert_eq!(lua.matches(" for c").count(), fors, "{name}: {lua}");
        assert_eq!(has_general_loop(&lua), general, "{name}: {lua}");
    }
    for name in ["iter_field_bound", "iterator_reads_vec_length_each_turn"] {
        let lua = emit_inlined(src, name);
        assert!(!lua.contains(" for c"), "{name}: {lua}");
        assert!(has_general_loop(&lua), "{name}: {lua}");
    }
}

fn has_general_loop(lua: &str) -> bool {
    lua.contains(" while ") || lua.contains("goto w")
}

#[test]
fn numeric_for_keeps_latch_work_and_respects_continue() {
    let src = include_str!("../../../tests/control_flow/counted_bounds.rock");
    for (name, expected_for) in [
        ("fused_latch", true),
        ("straight_latch", true),
        ("continue_skips_latch", false),
    ] {
        let lua = emit_clean(src, name);
        assert_eq!(lua.contains(" for c"), expected_for, "{name}: {lua}");
        assert_eq!(has_general_loop(&lua), !expected_for, "{name}: {lua}");
    }
}

#[test]
fn numeric_for_snapshots_only_invariant_arithmetic() {
    let src = include_str!("../../../tests/control_flow/counted_bounds.rock");
    let lua = emit_clean(src, "computed_bound");
    let head = lua
        .lines()
        .find(|line| line.starts_with(" for c"))
        .unwrap_or("");
    assert!(
        head.contains(" * ") && head.contains(" + ") && head.contains(" - 1 do"),
        "{lua}"
    );
    for name in ["mutable_computed_bound", "computed_bound_edge"] {
        let lua = emit_clean(src, name);
        assert!(has_general_loop(&lua) && !lua.contains(" for c"), "{lua}");
    }
}

#[test]
fn json_digit_scans_use_numeric_for() {
    let lua = emit_inlined(
        r#"
fn scan_json_number(src: string, at: int) -> (int, bool, bool) {
    return parse_number_scan_at(src, at)
}
"#,
        "scan_json_number",
    );
    assert_eq!(lua.matches(" for c").count(), 3, "{lua}");
    assert!(!lua.contains("goto w"), "{lua}");
}
