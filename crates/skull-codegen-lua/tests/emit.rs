//! Emitted-Lua contracts for the dumb printer. Each lowers a snippet to MIR and
//! asserts its Lua. Naming is raw-arena-index based (`v<raw>`/`c<raw>`/`bb<raw>`),
//! so value numbers skip the indices consumed by effect instructions (`Store`).
//!
//! Execution under luajit is the deeper oracle, wired at the Phase-6 CLI cutover
//! (`rock {run,test} --backend skull`); these pin the textual shape until then.

mod common;

use bone_core::prelude::ModuleId;
use bone_core::workspace::Workspace;
use skull_codegen_lua::{CodegenCtx, emit_function};
use skull_mir::mir::FuncRef;
use std::sync::LazyLock;

/// Lower `fn_name` from `modules` (last = the user `<test>` module), emit Lua.
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

fn test_fn(
    ws: &Workspace,
    mid: ModuleId,
    name: &str,
) -> comfy_la_arena::Idx<bone_core::hir::Function> {
    ws.module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
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

fn emit_dump(modules: Vec<skull_parser::Ast>, fn_name: &str) -> String {
    emit_dump_with(modules, fn_name, |_| {})
}

/// [`emit_dump`] under a context `configure` adjusts first.
fn emit_dump_with(
    mut modules: Vec<skull_parser::Ast>,
    fn_name: &str,
    configure: impl FnOnce(&mut CodegenCtx),
) -> String {
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
    let mut ctx = CodegenCtx::new(&ws);
    configure(&mut ctx);
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

/// Stdlib-free: literals, vars, `let`, builtin comparisons, control flow.
fn emit(src: &str, fn_name: &str) -> String {
    let ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse");
    emit_dump(vec![ast], fn_name)
}

#[test]
fn absent_field_store_skips_nil_key_insertion_and_preserves_clearing() {
    common::require_luajit();
    let lua = emit_clean(
        "struct Row { payload: string? = null } fn update(row: Row, value: string?) { row.payload = value }",
        "update",
    );
    assert!(
        lua.contains(" ~= nil or ") && lua.contains(".payload ~= nil then"),
        "{lua}"
    );
    let symbol = lua
        .lines()
        .find_map(|line| line.strip_prefix("function "))
        .expect("function definition")
        .split('(')
        .next()
        .unwrap();
    let driver = format!(
        r#"{lua}
local row = {{}}
{symbol}(row, nil)
assert(row.payload == nil)
{symbol}(row, 'live')
assert(row.payload == 'live')
{symbol}(row, nil)
assert(row.payload == nil)
local hits, reads = 0, 0
local observed = setmetatable({{}}, {{
    __newindex = function() hits = hits + 1 end,
    __index = function() reads = reads + 1 end,
}})
{symbol}(observed, nil)
{symbol}(observed, nil)
assert(hits == 2 and reads == 0)
"#
    );
    luajit_ok(&driver, "");
}

#[test]
fn non_nil_constant_field_store_keeps_direct_assignment() {
    let lua = emit_clean(
        "struct Row { count: int = 0 } fn update(row: Row) { row.count = 42 }",
        "update",
    );
    assert!(!lua.contains("getmetatable("), "{lua}");
    assert!(lua.contains(".count = (42)"), "{lua}");
}

#[test]
fn sinking_preserves_dynamic_format_error_order() {
    common::require_luajit();
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
    common::require_luajit();
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
        "fn choose(a: float) -> float { let lo = -1.0; return if a < lo { lo } else if a > 1.0 { 1.0 } else { a } }",
        ustr::Ustr::from("<test>"),
    ).expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic_pass");
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name == "choose")
        .unwrap()
        .0;
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
        .position(|id| matches!(func.insts[*id], Inst::Store { cell, .. } if cell == lo))
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
    common::require_luajit();
    let driver = format!(
        "local flushes = 0\njit.attach(function(what) if what == 'flush' then flushes = flushes + 1 end end, 'trace')\nfunction mutate(s) s.c0 = 0 end\n{lua}\nassert(choose({{}}, -0.5) == 0)\nprint('frame-ok flushes=' .. flushes)"
    );
    luajit_ok(&driver, &lua);
}

fn check_selection_values(lua: &str, reference: &str) {
    common::require_luajit();
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
    let run = luajit_ok(&driver, "");
    let stdout = run;
    assert!(stdout.starts_with("selection-ok flushes="), "{stdout}");
    eprint!("{stdout}");
}

#[test]
fn scalar_constants_are_literals_at_use_sites() {
    for (declarations, result, literal) in [
        ("const NUMBER: float = -2.5", "float", "-2.5"),
        (
            "const ORIGINAL: int = 42; const NUMBER: int = ORIGINAL",
            "int",
            "42",
        ),
        ("const NUMBER: bool = true", "bool", "true"),
        (
            "const NUMBER: string = \"literal\"",
            "string",
            "\"literal\"",
        ),
    ] {
        let lua = emit_clean(
            &format!("{declarations}\nfn choose() -> {result} {{ return NUMBER }}"),
            "choose",
        );
        assert!(
            !lua.contains("NUMBER") && !lua.contains("ORIGINAL"),
            "{lua}"
        );
        assert!(lua.contains(literal), "{lua}");
    }
    let lua = emit_clean(
        "global NUMBER: int = 42\nfn choose() -> int { NUMBER }",
        "choose",
    );
    assert!(lua.contains("NUMBER"), "{lua}");
    let lua = emit_clean(
        "fn init() -> int { 42 }\nconst NUMBER: int = init()\nfn choose() -> int { NUMBER }",
        "choose",
    );
    assert!(lua.contains("NUMBER"), "{lua}");
}

#[test]
fn derived_scalar_constants_fold_arithmetic_without_runtime_reads() {
    for (declarations, result, expected) in [
        ("const NUMBER: float = 1.0 + 0.000001", "float", "1.000001"),
        (
            "const BASE: int = 8; const NUMBER: int = BASE * 128",
            "int",
            "1024",
        ),
        (
            "const BASE: float = 4.5; const NUMBER: float = (BASE - 1.5) * 2.0",
            "float",
            "6.0",
        ),
        ("const NUMBER: float = 7 / 2", "float", "3.5"),
    ] {
        let lua = emit_clean(
            &format!("{declarations}\nfn choose() -> {result} {{ NUMBER }}"),
            "choose",
        );
        assert!(!lua.contains("NUMBER") && !lua.contains("BASE"), "{lua}");
        assert!(lua.contains(expected), "{lua}");
    }
}

#[test]
fn scalar_arithmetic_keeps_effects_checks_and_unrepresentable_values_at_runtime() {
    for (declarations, result) in [
        (
            "global BASE: int = 8; const NUMBER: int = BASE * 128",
            "int",
        ),
        (
            "fn input() -> int { 8 }; const NUMBER: int = input() * 128",
            "int",
        ),
        (
            "const BASE: uint = 4294967295; const ONE: uint = 1; const NUMBER: uint = BASE + ONE",
            "uint",
        ),
        ("const NUMBER: float = 1.0 / 0.0", "float"),
        ("const NUMBER: float = 1e308 * 1e308", "float"),
        (
            "const BASE: int = 9007199254740991; const NUMBER: int = BASE + 2",
            "int",
        ),
    ] {
        let lua = emit_clean(
            &format!("{declarations}\nfn choose() -> {result} {{ NUMBER }}"),
            "choose",
        );
        assert!(lua.contains("NUMBER"), "{lua}");
    }
    let lua = emit_clean(
        "const NUMBER: float = -0.0 * 2.0; fn choose() -> float { NUMBER }",
        "choose",
    );
    assert!(lua.contains("-0.0") && !lua.contains("NUMBER"), "{lua}");
}

#[test]
fn numeric_selection_uses_native_minmax() {
    let lua = emit_clean(
        "fn choose(a: float, b: float) -> float { return if a < b { a } else { b } }",
        "choose",
    );
    assert!(lua.contains("math.min("), "{lua}");
    let lua = emit_clean(
        "fn choose(a: float, b: float) -> float { return if a < b { b } else { a } }",
        "choose",
    );
    assert!(lua.contains("math.max("), "{lua}");
    let lua = emit_clean(
        "fn choose(a: int, b: int) -> int { return if a < b { a } else { b } }",
        "choose",
    );
    assert!(lua.contains("math.min("), "{lua}");
    let lua = emit_clean(
        "fn choose(a: int, b: int) -> int { if b > a { a = b }; return a }",
        "choose",
    );
    assert!(lua.contains("math.max("), "{lua}");
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
fn numeric_selection_preserves_special_values_and_guarded_assignment() {
    common::require_luajit();
    for (condition, yes, no, native) in [
        ("<", "a", "b", "min"),
        ("<", "b", "a", "max"),
        (">", "a", "b", "max"),
        (">", "b", "a", "min"),
    ] {
        let lua = emit_clean(
            &format!(
                "@nomangle fn choose(a: float, b: float) -> float {{ return if a {condition} b {{ {yes} }} else {{ {no} }} }}"
            ),
            "choose",
        );
        assert!(lua.contains(&format!("math.{native}(")), "{lua}");
        let driver = format!(
            r#"{lua}
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
"#
        );
        let result = luajit_ok(&driver, "");
        assert_eq!(result.trim(), "selection-ok");
    }
    let lua = emit_clean(
        "fn choose(a: float, b: float) -> float { if b > a { a = b }; return a }",
        "choose",
    );
    assert!(lua.contains("math.max("), "{lua}");
}

#[test]
fn numeric_selection_preserves_writes_before_the_comparison() {
    common::require_luajit();
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

/// Like [`emit_std`] but runs `skull_transform::cleanup` (copy-prop + DCE) before
/// emitting -- the artifact the REAL pipeline ships (`skull::mir_emit`). Needed to
/// assert allocation-shape claims (a dead `MakeTuple` left by the raw lowering is
/// removed here, so `no table` assertions reflect what actually runs).
fn emit_clean(src: &str, fn_name: &str) -> String {
    emit_clean_with(src, fn_name, |_| {})
}

/// [`emit_clean`] under a context `configure` adjusts first.
fn emit_clean_with(src: &str, fn_name: &str, configure: impl FnOnce(&mut CodegenCtx)) -> String {
    let (ws, mid) = stdlib_workspace(src);
    let fid = test_fn(&ws, mid, fn_name);
    let (mut func, interner, _) = common::lower_test_ref_with_graph(&ws, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    skull_transform::cleanup(&mut func).unwrap();
    let mut ctx = CodegenCtx::new(&ws);
    configure(&mut ctx);
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

/// Emit the graph-reached lowerable instances for the `<test>` module's user
/// roots, run a Lua `driver` expression (e.g. `"sign(-5)"`) under luajit, and
/// return its trimmed stdout. This keeps inlining disabled and delivers the
/// residual stdlib instances instead of depending on a splice for completeness.
/// Panics if luajit is absent: a host without it must fail these tests, never pass them.
///
/// Assembled the way `skull::mir_emit::assemble` assembles a program: every
/// definition first, then the callee binding pass in component-rank order,
/// with the components populated from this harness's own instance graph.
fn run(src: &str, driver: &str) -> String {
    common::require_luajit();
    let (ws, mid) = stdlib_workspace(src);
    let mut ctx = CodegenCtx::new(&ws);
    let roots = ws
        .module(mid)
        .functions
        .iter()
        .map(|(fid, _)| FuncRef {
            module: mid,
            func: fid,
        })
        .collect();
    let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(&ws, roots, false, interner.clone());
    ctx.set_call_sccs(common::call_sccs(&graph));
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(interner);
    let concrete = skull_graph::concrete_body::build(&ws, &graph);
    let mut code_artifact = skull_mir::code_identity::CodeArtifact::default();
    // The binding pass, in ascending component rank -- the same order
    // `skull::mir_emit::callee_binding_pass_lua` emits, and for the same
    // reason: a captured callee must already be its final closure.
    let mut binds: Vec<skull_codegen_lua::CalleeBindingPlan> = Vec::new();
    // Capturing lambdas lower to `setmetatable({..}, __rock_fn_mt)` env records; the record
    // metatable lives in skull's
    // runtime prelude, which this bare harness does not emit -- pull in the
    // canonical definition (one copy, shared with `skull::runtime_prelude`).
    let mut lua = format!(
        "local bit = require('bit')\nlocal ffi = require('ffi')\n{}\n{}\n{}\nfunction __nb_checked_get(b, i) if i < 0 or i >= b.len then error('NativeBuffer index out of bounds') end return b.ptr[i] end\nfunction __nb_checked_set(b, i, v) if i < 0 or i >= b.len then error('NativeBuffer index out of bounds') end b.ptr[i] = v end\n",
        skull_codegen_lua::FN_MT_PRELUDE,
        skull_codegen_lua::UINT_CHECK_PRELUDE,
        skull_codegen_lua::VEC_CHECK_SHIMS
    );
    for inst in graph.expanded() {
        let f = &ws.module(inst.func.module).functions[inst.func.func];
        if f.inline_body.is_some() {
            continue;
        }
        let Some(plan) = concrete.plans.get(inst) else {
            continue;
        };
        if let Ok(mut func) =
            skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &concrete.plans)
        {
            skull_transform::cleanup(&mut func).unwrap();
            skull_transform::recover_counted_loops(&ws, &mut func);
            let name = inst.symbol(&ws, scx.interner());
            let pre_nested = func.nested_fns.len();
            skull_transform::convert_closures(&mut func, &name);
            let mut unit_artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree(&func);
            unit_artifact.set_body_names(&func, &name);
            code_artifact.merge(unit_artifact);
            let plan = skull_codegen_lua::callee_binding_plan(
                &ws,
                &ctx,
                scx.interner(),
                &name,
                inst.func,
                &func,
            );
            lua.push_str(&skull_codegen_lua::emit_named(
                &ws,
                &ctx,
                scx.interner(),
                &name,
                &func,
                plan.as_ref(),
            ));
            if let Some(plan) = plan {
                binds.push(plan);
            }
            for nf in func.nested_fns[pre_nested..].iter_mut() {
                skull_transform::cleanup_with_nobox(&ws, nf).unwrap();
                skull_transform::recover_counted_loops(&ws, nf);
                lua.push_str(&skull_codegen_lua::emit_named(
                    &ws,
                    &ctx,
                    scx.interner(),
                    nf.name.as_str(),
                    nf,
                    None,
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
            let body = entry
                .body_name
                .expect("escaping CodeRef has no emitted body");
            lua.push_str(&format!(
                "__rock_code_slot_stage({}, {}, {})\n",
                skull_codegen_lua::lua_string(&entry.identity.logical_key(&ws, scx.interner())),
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
    let modules = stdlib_with(src);
    emit_dump(modules, fn_name)
}

#[test]
fn int_literal_and_return() {
    // The `5` is a single-use pure value -> tree-folded into the return (no
    // register, no `local`).
    assert_eq!(
        emit("fn five() -> int { 5 }", "five"),
        "\
function five_0()
  ::bb0::
  do return (5) end
end
"
    );
}

#[test]
fn zero_argument_call_preserves_value_without_a_bare_global() {
    let src = "fn five() -> int { 5 } fn answer() -> int { five() }";
    let lua = format!("{}{}", emit(src, "five"), emit(src, "answer"));
    let run = std::process::Command::new(rock_host::project::luajit_path())
        .arg("-e")
        .arg(format!(
            "{lua}\nassert(_G.five == nil and _G.answer == nil)\nprint(answer_0())"
        ))
        .output()
        .expect("run emitted zero-argument call");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "5");
}

#[test]
fn param_cell_read() {
    assert_eq!(
        emit("fn id(x: int) -> int { x }", "id"),
        "\
function id_int(c0)
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
function add_int_int(c0, c1)
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
    assert_eq!(
        run("fn f(a: int, b: int) -> int { a ./ b }", "f(-7, 2)"),
        "-4"
    );
    assert_eq!(
        run("fn f(a: int, b: int) -> int { a * b + 1 }", "f(6, 7)"),
        "43"
    );
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
    assert_eq!(
        run(src, "total({_data = {4, 5, 6}, len = 3, __ty = \"Vec\"})"),
        "15"
    );
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
        !body.contains("dbl_int("),
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
        body.contains("dbl_int("),
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

// --- Call-site default fill --
//
// The callee keeps its `if p == nil { p = default }` prologue for calls the
// emitter cannot see (a function value), but every DIRECT call passes a
// literal scalar default explicitly, so under the JIT the prologue's nil branch
// is never the direction a trace takes and never a doomed side exit with the
// caller's floats live.

#[test]
fn direct_calls_pass_literal_scalar_defaults_explicitly() {
    let src = "
fn f(a: int, b: int = 7, c: float = 0.5, d: string = \"x\", e: bool = true) -> int { a + b }
fn go() -> int { f(1) }
fn go_named() -> int { f(1, d: \"y\") }";
    let lua = emit_clean(src, "go");
    assert!(
        lua.contains("((1), (7), (0.5), (\"x\"), (true))"),
        "trailing defaults must be passed:\n{lua}"
    );
    let lua = emit_clean(src, "go_named");
    assert!(
        lua.contains("((1), (7), (0.5), (\"y\"), (true))"),
        "a named-arg gap and the trailing default must both be passed:\n{lua}"
    );
}

#[test]
fn a_null_default_emits_no_callee_prologue_branch() {
    let src = "fn g(a: int, b: int? = null) -> int { a }";
    let lua = emit_clean(src, "g");
    assert!(
        !lua.contains("(nil)"),
        "omitted IS null; no branch may test for it:\n{lua}"
    );
}

#[test]
fn a_non_literal_default_stays_callee_filled() {
    let src = "
global K: int = 3
fn h(a: int, b: int = K) -> int { a + b }
fn go() -> int { h(1) }";
    let lua = emit_clean(src, "go");
    assert!(
        lua.contains("((1))"),
        "a global-valued default is the callee's to evaluate:\n{lua}"
    );
    let callee = emit_clean(src, "h");
    assert!(
        callee.contains("(nil)"),
        "the callee prologue must remain:\n{callee}"
    );
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
fn uint_checked_casts_emit_no_closure() {
    for ty in ["int", "float"] {
        let lua = emit(
            &format!("fn cast(value: {ty}) -> uint {{ value as uint }}"),
            "cast",
        );
        assert!(
            !lua.lines().skip(1).any(|line| line.contains("function")),
            "{lua}"
        );
    }
}

#[test]
fn uint_checked_shifts_emit_no_closure() {
    for op in ["<<", ">>"] {
        let lua = emit(
            &format!("fn shift(value: uint, count: uint) -> uint {{ value {op} count }}"),
            "shift",
        );
        assert!(
            !lua.lines().skip(1).any(|line| line.contains("function")),
            "{lua}"
        );
    }
}

#[test]
fn uint_constant_casts_emit_folded_numbers() {
    for (value, expected) in [("0", "0"), ("4294967295", "4294967295"), ("3.9", "3")] {
        let lua = emit(&format!("fn cast() -> uint {{ {value} as uint }}"), "cast");
        assert!(
            lua.contains(&format!("do return ({expected}) end")),
            "{lua}"
        );
    }
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
}

#[test]
fn exec_uint_checks_preserve_ranges_and_single_evaluation() {
    let src = r#"
fn cast(value: float) -> uint { value as uint }
fn from_int(value: int) -> uint { value as uint }
fn left(value: uint, count: uint) -> uint { value << count }
fn right(value: uint, count: uint) -> uint { value >> count }
fn take(values: [uint]) -> uint {
    let value = values[0]
    values[0] += 1
    value
}
fn cast_once(values: [uint]) -> uint { (take(values) as float) as uint }
fn left_once(values: [uint]) -> uint { take(values) << take(values) }
fn right_once(values: [uint]) -> uint { take(values) >> take(values) }
"#;
    let driver = r#"(function()
local function rejects(fn, message, ...)
  local ok, err = pcall(fn, ...)
  assert(not ok and tostring(err):sub(-#message) == message, tostring(err))
end
assert(cast(0) == 0 and cast(3.9) == 3 and cast(4294967295) == 4294967295)
assert(1 / cast(-0.0) == -math.huge)
assert(from_int(0) == 0 and from_int(4294967295) == 4294967295)
for _, value in ipairs({-1, 4294967296}) do
  rejects(from_int, 'uint conversion out of range', value)
end
for _, value in ipairs({-0.1, 4294967295.5, 0/0, math.huge, -math.huge}) do
  rejects(cast, 'uint conversion out of range', value)
end
assert(left(1, 0) == 1 and left(1, 31) == 2147483648)
assert(right(4294967295, 0) == 4294967295 and right(4294967295, 31) == 1)
for _, fn in ipairs({left, right}) do
  for _, count in ipairs({32, 4294967295}) do
    rejects(fn, 'uint shift count out of range', 0, count)
  end
end
local values = {_data={1}, len=1}
assert(cast_once(values) == 1 and values._data[1] == 2)
values._data[1] = 1
assert(left_once(values) == 4 and values._data[1] == 3)
values._data[1] = 1
assert(right_once(values) == 0 and values._data[1] == 3)
return true
end)()"#;
    assert_eq!(run(src, driver), "true");
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
// Rock suite (`tests/patterns/enums.rock`). The per-function `run` harness here
// can't emit that scaffolding (it would need a dep on `skull`), so it can't
// execute a scalarized-enum match; per-function match shape is pinned by
// skull-mir's `match_enum_*` lowering tests.

// --- Multi-return ABI for tuple-returning fns ---

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
    assert!(
        !body.contains("{"),
        "tail tuple must not build a table:\n{body}"
    );
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
        body.lines()
            .any(|l| l.contains(", ") && l.contains("= pair_0(")),
        "expected a multi-assign call line `a, b = pair_0(...)`:\n{body}"
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
    assert!(
        body.contains("= pair_0()"),
        "multi-assign call expected:\n{body}"
    );
}

#[test]
fn if_as_value_branch_and_cell_join() {
    // A local consumes the if's value, so it retains a cell join even though
    // terminal ifs forward their return destination. Cell coloring coalesces
    // the dead param with result cells; pure values fold into their uses.
    assert_eq!(
        emit(
            "fn sign(x: int) -> int { let value = if x < 0 { 0 } else { 1 }; value }",
            "sign"
        ),
        "\
function sign_int(c0)
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
  c0 = r0
  r0 = c0
  do return r0 end
end
"
    );
}

/// Full pre-emit pipeline (mirrors `skull::mir_emit`): plan-backed lower -> inline
/// -> `cleanup` -> emit. The honest artifact for zero-alloc claims: an `@inline`
/// `iter`/`next` is only spliced (and its cursor struct scalarized) once the
/// inliner has run, which `emit_clean` does NOT do.
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
        false,
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
    let ctx = CodegenCtx::new(&ws);
    emit_function(&ws, &ctx, scx.interner(), self_ref, &func, None)
}

// --- An @inline user iterator driven by `for`
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
// read is `data[i+1]`, no cursor table. This is the shape the stdlib `VecIter`
// gives real `[T]` (proven here via a wrapper type).
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

// --- The desugared+inlined counted loop is rewritten
// into the codegen CountedLoop shape and prints as a native numeric `for` (FORL
// bytecode). The goto-LOOP form of the same loop measured 1.8x slower on a
// type-stable 2M-element sum.
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
        !lua.lines().any(|l| l.contains(">=")),
        "the slot exit-compare must be absorbed into the `for` header:\n{lua}"
    );
}

// --- A REAL `[T]` for-loop routes through the stdlib
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

// --- Combinator chains flatten. The bounded
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
// `for + skip-branch`. The goto
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
    let for_line = lines
        .iter()
        .position(|l| l.starts_with("for "))
        .unwrap_or_else(|| panic!("no for:\n{lua}"));
    let inner_decl = lines[for_line + 1];
    assert!(
        inner_decl.starts_with("local r"),
        "loop-scoped registers expected right after the for:\n{lua}"
    );
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
                assert!(
                    for_line < i && i < end_line,
                    "loop-scoped `{kill}` outside its for block at line {i}:\n{lua}"
                );
            }
        }
    }
    assert!(
        lines.iter().any(|l| l.ends_with(" = nil")),
        "no kills emitted at all:\n{lua}"
    );
}

// -- Emitted tail calls -----------------------------------------------------
//
// A call in tail position is forwarded as `do return f(...) end`, which keeps
// Lua's tail-call elimination for recursive calls.

#[test]
fn a_tail_call_is_forwarded() {
    let src = "fn leaf(a: int) -> int { a + 1 }\nfn forward(a: int) -> int { return leaf(a) }";
    let lua = emit_clean(src, "forward");
    assert!(
        lua.contains("do return leaf_int(c0) end"),
        "expected the forwarded tail call:\n{lua}"
    );
}

#[test]
fn a_self_recursive_tail_call_is_forwarded() {
    let src = "fn down(n: int, acc: int) -> int { if n == 0 { return acc }\n  return down(n - 1, acc + 1) }";
    let lua = emit_clean(src, "down");
    assert!(
        lua.contains("do return down_int_int("),
        "expected the recursive tail call kept:\n{lua}"
    );
}

/// The runtime obligation: a tail recursion deeper than the Lua stack survives.
/// Measured on the shipped LuaJIT, a non-tail recursion overflows between
/// 10,000 and 60,000 frames, so 200,000 is a real witness rather than a margin.
#[test]
fn deep_tail_recursion_survives() {
    let src = "fn count_down(n: int, acc: int) -> int { if n == 0 { return acc }\n  return count_down(n - 1, acc + 1) }";
    assert_eq!(run(src, "count_down(200000, 0)"), "200000");
}

#[test]
fn a_multi_return_forward_delivers_its_values() {
    let src = "fn pair(a: int) -> (int, int) { (a, a + 1) }\n\
               fn forward_pair(a: int) -> (int, int) { return pair(a) }\n\
               fn total(a: int) -> int { let (x, y) = forward_pair(a); x * 100 + y }";
    assert_eq!(run(src, "total(7)"), "708");
}

// --- Callee binding as immutable upvalues ------------------
//
// Callee binding defines a function as a factory whose parameters
// are named exactly like the callees they replace, so the body text is
// unchanged and every call reads an immutable upvalue (2 constant-pool slots on
// GC64) instead of a global (5, plus a guard). The assembler emits the
// matching `<name> = __rock_mk_<name>(<callees>)` line after every definition.

/// [`emit_clean`] with its callee binding plan, the call-graph components
/// populated from this harness's own instance graph exactly as
/// `skull::mir_emit` populates them from the whole-program one.
fn emit_clean_bound(src: &str, fn_name: &str) -> String {
    emit_clean_bound_checked(src, fn_name, |_, _| {})
}

fn emit_clean_bound_checked(
    src: &str,
    fn_name: &str,
    check: impl FnOnce(&bone_core::workspace::Workspace, &skull_mir::mir::MirFunc),
) -> String {
    let (ws, mid) = stdlib_workspace(src);
    let fid = test_fn(&ws, mid, fn_name);
    let self_ref = FuncRef {
        module: mid,
        func: fid,
    };
    let (mut func, interner, graph) = common::lower_test_ref_with_graph(&ws, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    skull_transform::cleanup(&mut func).unwrap();
    check(&ws, &func);
    let mut ctx = CodegenCtx::new(&ws);
    ctx.set_call_sccs(common::call_sccs(&graph));
    let name = skull_codegen_lua::mangle_fn(&ws, &ws.module(mid).functions[fid]);
    let plan = skull_codegen_lua::callee_binding_plan(&ws, &ctx, &interner, &name, self_ref, &func);
    let lua =
        skull_codegen_lua::emit_function(&ws, &ctx, &interner, self_ref, &func, plan.as_ref());
    match plan {
        Some(plan) => format!("{lua}{}", plan.bind_line()),
        None => lua,
    }
}

#[test]
fn callee_binding_ignores_calls_removed_by_cleanup() {
    let src = "fn discarded(a: int) -> int { a + 2 }\nfn live(a: int) -> int { a + 1 }\nfn caller(a: int) -> int { if false { discarded(a) } else { live(a) } }";
    let lua = emit_clean_bound_checked(src, "caller", |ws, func| {
        use skull_mir::mir::Inst;
        let is_discarded = |inst: &Inst| {
            matches!(inst,
                Inst::Call { func: callee, .. }
                    if ws.module(callee.module).functions[callee.func].name.as_str() == "discarded"
            )
        };
        assert!(
            func.insts.iter().any(|(_, inst)| is_discarded(inst)),
            "fixture must retain the discarded call in its arena"
        );
        assert!(
            !skull_mir::cfg::reverse_postorder(func, func.entry())
                .iter()
                .flat_map(|bid| func.blocks[*bid].insts.iter())
                .any(|id| is_discarded(&func.insts[*id])),
            "cleanup must remove the call from emitted instructions"
        );
    });
    assert!(
        lua.starts_with("__rock_mk_caller_int = function(live_int) return function("),
        "only the live callee should consume a binding slot:\n{lua}"
    );
    assert!(
        !lua.contains("discarded_int"),
        "discarded call must not be bound:\n{lua}"
    );
    assert_eq!(run(src, "caller(7)"), "8");
}

#[test]
fn callee_binding_ignores_discarded_uint_helpers() {
    let src = "fn checked(value: float, count: uint) -> uint { if false { (value as uint) << count } else { count } }";
    let lua = emit_clean_bound_checked(src, "checked", |_, func| {
        assert!(
            func.insts
                .iter()
                .any(|(_, inst)| matches!(inst, skull_mir::mir::Inst::FloatToUint(_))),
            "fixture must retain the discarded conversion in its arena"
        );
    });
    assert!(
        !lua.contains(skull_codegen_lua::CALLEE_FACTORY_PREFIX),
        "discarded validators must not create a factory:\n{lua}"
    );
    assert_eq!(run(src, "checked(-1, 3)"), "3");
}

#[test]
fn read_only_nullable_value_lookup_does_not_rebuild_the_wrapper() {
    // #827: a let-bound nullable GC value struct copied out of a
    // container binds as presence/field lanes; read-only whole uses
    // (presence test, unwrap field reads) must not pack the lanes
    // straight back into a fresh `_vt` wrapper.
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
        assert!(
            !lua.contains("SlotMetadata_vt("),
            "{name} must not materialize a fresh wrapper:\n{lua}"
        );
    }
}

/// The bound shape: a factory named for the function, one parameter per
/// distinct callee spelled exactly like the global it replaces, the body
/// verbatim, and the binding line that calls the factory with those globals.
#[test]
fn callee_binding_upvalue_wraps_the_body_in_a_factory() {
    let src =
        "fn leaf(a: int) -> int { a + 1 }\nfn caller(a: int) -> int { leaf(a) + leaf(a + 1) }";
    let unbound = emit_clean(src, "caller");
    let bound = emit_clean_bound(src, "caller");
    assert!(
        bound.starts_with("__rock_mk_caller_int = function(leaf_int) return function("),
        "expected the factory header:\n{bound}"
    );
    assert!(
        bound.contains("caller_int = __rock_mk_caller_int(leaf_int)\n"),
        "expected the binding line:\n{bound}"
    );
    // The body is the ONLY copy and is byte-identical to the unbound one: the
    // factory parameter is named like the global, so no call site is rewritten.
    let body = unbound
        .strip_prefix("function caller_int")
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
    let bound = emit_clean_bound(src, "is_odd");
    assert!(
        bound.starts_with("__rock_mk_is_odd_int = function(leaf_int) return "),
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
    let bound = emit_clean_bound(src, "down");
    assert!(
        bound.starts_with("__rock_mk_down_int = function(leaf_int) return "),
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
    let bound = emit_clean_bound(&src, "caller");
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
        params.starts_with("leaf000_int, ") && params.ends_with(&format!("leaf{:03}_int", cap - 1)),
        "expected the first {cap} callees in name order:\n{params}"
    );
    for i in cap..count {
        assert!(
            !params.contains(&format!("leaf{i:03}_int")),
            "expected `leaf{i:03}` left global:\n{params}"
        );
    }
    // The cap is a correctness rule, not a tuning knob: LuaJIT refuses to parse
    // a function with more than `MAX_BOUND_CALLEES` upvalues, so an uncapped
    // binding would make this program fail to load at all.
    let expected: i64 = (0..count as i64).map(|i| 7 + i).sum();
    assert_eq!(
        run(&src, "caller(7)"),
        expected.to_string(),
        "a body with more bindable callees than LuaJIT allows upvalues must still load and run"
    );
}

/// The bound program computes the right results through real LuaJIT over the
/// two shapes the binding rule is about: a mutually recursive pair (whose legs
/// stay global lookups) and a call through a function value (which has no
/// statically known callee at all).
#[test]
fn callee_binding_computes_through_recursion_and_function_values() {
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
    for (driver, expected) in [
        ("total(6)", "1160"),
        ("total(7)", "69"),
        ("total(0)", "1106"),
    ] {
        assert_eq!(run(src, driver), expected, "`{driver}`");
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

/// An iterator loop nested inside another loop: its preheader is reachable
/// from its body through the enclosing back edge, which a body-reachability
/// split read as a second back edge and bailed on. Back edges are the head's
/// predecessors that the head DOMINATES; the preheader never is. Both loops
/// print as counted `for`s and no backward goto survives.
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
    assert_eq!(
        fors, 2,
        "both iterator loops must print as counted `for`s:\n{lua}"
    );
    assert!(
        backward_gotos(&lua).is_empty(),
        "no back edge may remain:\n{lua}"
    );
}

/// The recovered nested iterator loop computes what the goto loop computed:
/// every inner bound is re-read per outer iteration, an empty inner vec runs
/// zero times, and an early `return` out of the inner loop still fires.
#[test]
fn recovered_nested_iterator_loop_computes_the_same_under_luajit() {
    let src = "\
struct B { slot: int }\n\
struct D { bindings: [B] }\n\
fn f(ds: [D], stop: int) -> int {\n\
    let t = 0\n\
    for d in ds { for b in d.bindings { if b.slot == stop { return t - 1000 }; t = t + b.slot } }\n\
    t\n\
}\n\
fn probe(stop: int) -> int {\n\
    let ds = [D { bindings: [B { slot: 1 }, B { slot: 2 }] }, D { bindings: [] }, D { bindings: [B { slot: 3 }, B { slot: 4 }, B { slot: 5 }] }]\n\
    f(ds, stop)\n\
}";
    assert_eq!(run(src, "probe(99)"), "15");
    assert_eq!(run(src, "probe(3)"), "-997");
    assert_eq!(run(src, "probe(1)"), "-1000");
}

#[test]
fn vec_index_read_calls_the_checked_helper() {
    let lua = emit("fn f(xs: [int], i: int) -> int { return xs[i] }", "f");
    assert!(lua.contains("__vec_checked_get("), "{lua}");
}

#[test]
fn ordinary_nullable_value_returns_do_not_materialize_boxes() {
    let src = r#"
value Pair { x: int, y: int }
value Label { text: string, active: bool }
struct Row { marker: int }
fn pair(present: bool, x: int) -> Pair? {
    if !present { return null }
    return Pair { x: x, y: x + 1 }
}
fn label(present: bool, text: string) -> Label? {
    if !present { return null }
    return Label { text: text, active: false }
}
fn entity(present: bool, slot: int) -> Entity<Row>? {
    if !present { return null }
    return Entity { _scene_id: 11, _arena_id: 12, _slot: slot, _generation: 13 }
}
fn local_pair(present: bool, x: int) -> int {
    let p = pair(present, x)
    if let v = p { return v.x + v.y }
    return -1
}
fn coalesced_pair(present: bool, x: int) -> int {
    let p = pair(present, x) ?? Pair { x: 3, y: 4 }
    return p.x + p.y
}
fn absent_pair(present: bool, x: int) -> bool { return pair(present, x) == null }
"#;
    let materializes =
        |lua: &str| lua.contains("_ct(") || lua.contains("_vt(") || lua.contains("setmetatable(");
    let mut materialized = Vec::new();
    for name in [
        "pair",
        "label",
        "entity",
        "local_pair",
        "coalesced_pair",
        "absent_pair",
    ] {
        let lua = emit_clean(src, name);
        if materializes(&lua) {
            materialized.push(format!("{name}: {lua}"));
        }
    }
    assert!(
        materialized.is_empty(),
        "nullable returns materialize boxes: {materialized:#?}"
    );
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

/// `rN = rM` / `rN = cM` lines: a register filled from another local unchanged.
fn register_moves(lua: &str) -> Vec<&str> {
    lua.lines()
        .map(str::trim)
        .filter(|line| {
            let Some((dst, src)) = line.split_once(" = ") else {
                return false;
            };
            let local = |s: &str, p: char| {
                s.strip_prefix(p)
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            };
            local(dst, 'r') && (local(src, 'r') || local(src, 'c'))
        })
        .collect()
}

#[test]
fn reused_int_to_float_aliases_its_operand() {
    // `x as float` is the identity on LuaJIT numbers; a conversion read more
    // than once names its operand's local instead of copying it into its own.
    let param = "fn f(x: int) -> float { let y = x as float; y * y + y }";
    let lua = emit_clean(param, "f");
    assert_eq!(register_moves(&lua), Vec::<&str>::new(), "{lua}");
    assert_eq!(run(param, "f(3)"), "12");

    let register = "fn g(a: int, b: int) -> float { let s = a * b; let y = s as float; y * y + y + (s + 1) as float }";
    let lua = emit_clean(register, "g");
    assert_eq!(register_moves(&lua), Vec::<&str>::new(), "{lua}");
    assert_eq!(run(register, "g(2, 3)"), "49");
}

#[test]
fn unreachable_block_traps_instead_of_returning() {
    common::require_luajit();
    let lua = emit_clean(
        "enum Dir { Up, Down } fn pick(d: Dir) -> int { match d { Dir::Up => { return 1 } Dir::Down => { return 2 } } }",
        "pick",
    );
    let symbol = lua
        .lines()
        .find_map(|line| line.strip_prefix("function "))
        .expect("function definition")
        .split('(')
        .next()
        .unwrap();
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

/// The (line, chunk name) LuaJIT 2.1's lexer assigns to each line of `lua`,
/// `None` for a line directive itself. The rule, from LuaJIT's `lex_linedir`:
/// `# <line> ["<file>"]` at column 0 numbers the NEXT line `<line>` and, with
/// a file, renames the chunk to `@<file>`; every other line is one more than
/// the line before it.
fn lexer_lines(lua: &str) -> Vec<Option<(u32, String)>> {
    let mut line = 1;
    let mut chunk = String::from("=chunk");
    let mut out = Vec::new();
    for text in lua.lines() {
        let directive = text
            .strip_prefix('#')
            .map(str::trim_start)
            .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
        let Some(rest) = directive else {
            out.push(Some((line, chunk.clone())));
            line += 1;
            continue;
        };
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        line = rest[..digits].parse().expect("directive line");
        let file = rest[digits..].trim();
        if !file.is_empty() {
            let name = file
                .strip_prefix('"')
                .and_then(|f| f.strip_suffix('"'))
                .expect("quoted file");
            chunk = if name.starts_with(['=', '@', ' ']) {
                name.to_string()
            } else {
                format!("@{name}")
            };
        }
        out.push(None);
    }
    out
}

const LINE_DIRECTIVE_SRC: &str = "fn f(a: int) -> int {
    let x = a * 3
    if x > 10 {
        return x - 7
    }
    return x + 5
}";

#[test]
fn line_directives_number_each_statement_with_its_rock_line() {
    let lua = emit_clean_with(LINE_DIRECTIVE_SRC, "f", |ctx| ctx.set_line_directives(true));
    let lines = lexer_lines(&lua);
    let at = |needle: &str| -> (u32, String) {
        let index = lua
            .lines()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` in\n{lua}"));
        lines[index].clone().expect("a statement, not a directive")
    };
    let chunk = String::from("@<test>");
    assert_eq!(at("* (3)"), (2, chunk.clone()), "{lua}");
    assert_eq!(at("- (7)"), (4, chunk.clone()), "{lua}");
    assert_eq!(at("+ (5)"), (6, chunk.clone()), "{lua}");
    // The header opens the prototype, so its line is in the function too.
    assert_eq!(at("function ").1, chunk, "{lua}");
}

#[test]
fn line_directives_off_writes_no_directive() {
    let lua = emit_clean_with(LINE_DIRECTIVE_SRC, "f", |ctx| {
        ctx.set_line_directives(false)
    });
    assert!(lexer_lines(&lua).iter().all(Option::is_some), "{lua}");
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
