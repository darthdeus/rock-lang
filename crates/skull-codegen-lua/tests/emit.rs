//! Exercise textual and runtime contracts for individual emitted Lua bodies.

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

fn emitted_symbol(lua: &str) -> &str {
    lua.lines()
        .find_map(|line| line.strip_prefix("function "))
        .expect("function definition")
        .split('(')
        .next()
        .unwrap()
}

fn luajit_loads(lua: &str) {
    let dir = std::env::temp_dir().join(format!(
        "skull_inline_body_limits_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("chunk.lua");
    std::fs::write(&path, lua).expect("write Lua chunk");
    let output = std::process::Command::new("luajit")
        .arg(&path)
        .output()
        .expect("spawn luajit");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "LuaJIT rejected emitted chunk:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn emit_run(src: &str) -> (String, Option<skull_parser::CompilerError>) {
    let (ws, mid) = stdlib_workspace(src);
    let fid = test_fn(&ws, mid, "run");
    let (func, interner) = common::lower_test_ref(&ws, mid, fid).expect("lower");
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
    let fid = test_fn(&ws, mid, fn_name);
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

/// Whether a field store in `lua` carries its nil-write guard. The guard is
/// the only `if .. ~= nil then` line these snippets produce.
fn has_store_guard(lua: &str) -> bool {
    lua.lines().any(|l| {
        let l = l.trim();
        l.starts_with("if ") && l.ends_with(" ~= nil then")
    })
}

#[test]
fn absent_record_field_store_preserves_host_callbacks_and_clearing() {
    let lua = emit_clean(
        "struct Row { payload: string? = null } fn update(row: Row, value: string?) { row.payload = value }",
        "update",
    );
    assert!(has_store_guard(&lua), "{lua}");
    // A host record can observe even a nil store to an absent field.
    assert!(lua.contains("getmetatable("), "{lua}");
    let symbol = emitted_symbol(&lua);
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
    __metatable = false,
    __newindex = function() hits = hits + 1 end,
    __index = function() reads = reads + 1 end,
}})
{symbol}(observed, nil)
{symbol}(observed, nil)
assert(hits == 2, 'each nil write must enter __newindex')
assert(reads == 0, 'a store must not introduce a field read')
{symbol}(observed, 'live')
assert(hits == 3, 'a non-nil write must also enter __newindex')
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
    assert!(!has_store_guard(&lua), "{lua}");
    assert!(lua.contains(".count = (42)"), "{lua}");
}

#[test]
fn field_store_guards_follow_the_value_nullability() {
    for (src, guarded) in [
        (
            "struct Acc { mean: float = 0.0 } fn update(acc: Acc, x: float) -> float { let m = acc.mean * 0.5 + x; acc.mean = m; return m }",
            false,
        ),
        (
            "struct Acc { mean: float = 0.0 } fn update(acc: Acc, x: float) { acc.mean = x }",
            false,
        ),
        (
            "struct Acc { n: int = 0, label: string = \"\" } \
          fn weight(x: int) -> int { if x > 3 { return x * 2 } return x - 1 } \
          fn name(x: int) -> string { if x > 3 { return \"big\" } return \"small\" } \
          fn update(acc: Acc, x: int) { acc.n = weight(x); acc.label = name(x) }",
            false,
        ),
        (
            "struct Inner { v: int } struct Acc { inner: Inner? = null, xs: Vec<int>? = null } \
          fn make(x: int) -> Inner { if x > 3 { return Inner { v: x } } return Inner { v: 0 } } \
          fn list(x: int) -> Vec<int> { if x > 3 { return [x] } return [] } \
          fn update(acc: Acc, x: int) { acc.inner = make(x); acc.xs = list(x) }",
            false,
        ),
        (
            "struct Acc { n: int? = null } \
          fn weight(x: int) -> int? { if x > 3 { return x * 2 } return null } \
          fn update(acc: Acc, x: int) { acc.n = weight(x) }",
            true,
        ),
        (
            "struct Acc { count: int = 0 } \
          fn update(acc: Acc, a: int, b: int) { let n = if a > b { a } else { b }; acc.count = n }",
            false,
        ),
    ] {
        let lua = emit_clean(src, "update");
        assert_eq!(has_store_guard(&lua), guarded, "{src}:\n{lua}");
    }
}

#[test]
fn value_param_leaf_stores_need_no_nil_guard() {
    // An embedded value field stores one dotted leaf key per ABI-split
    // parameter cell. The leaves of a non-nullable value shape are
    // non-nullable scalars, so none of the stores can write nil.
    let lua = emit_clean(
        "value Rect { x: float, y: float } \
         struct Holder { embed r: Rect } \
         fn update(h: Holder, p: Rect) { h.r = p }",
        "update",
    );
    assert!(!has_store_guard(&lua), "{lua}");
    assert!(lua.contains("[\"r.x\"] = "), "{lua}");
}

#[test]
fn nullable_value_param_record_leaf_stores_preserve_callbacks() {
    // Even an absent payload writes through each host-visible record slot.
    let lua = emit_clean(
        "value Rect { x: float, y: float } \
         struct Holder { embed r: Rect? } \
         fn update(h: Holder, p: Rect?) { h.r = p }",
        "update",
    );
    assert!(has_store_guard(&lua), "{lua}");
}

#[test]
fn nil_literal_record_store_keeps_guard_without_a_presence_read() {
    let lua = emit_clean(
        "struct Row { tag: int? = null } fn update(row: Row) { row.tag = null }",
        "update",
    );
    assert!(has_store_guard(&lua), "{lua}");
    assert!(!lua.contains("nil) ~= nil"), "{lua}");
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
        "nil needs no register even with readers in different blocks:\n{lua}"
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
            "filter chain must stay zero-alloc, fully inlined:\n{lua}"
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
            "map+filter chain must stay zero-alloc and fully inlined:\n{lua}"
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
fn constant_bitwise_masks_fold_before_lua_emission() {
    let lua = emit_clean(
        "const A: int = 1; const B: int = 2; const C: int = 4; const D: int = 8; fn masked(value: int) -> int { value & (A | B | C | D) }",
        "masked",
    );
    assert!(!lua.contains("bit.bor"), "{lua}");
    assert!(lua.contains("(15)"), "{lua}");
    let symbol = emitted_symbol(&lua);
    luajit_ok(
        &format!("{lua}\nfor i = -10000, 10000 do assert({symbol}(i) == bit.band(i, 15)) end"),
        &lua,
    );
}

#[test]
fn constant_bitwise_expressions_keep_signed_and_unsigned_results() {
    for (ty, expr, expected) in [
        ("int", "1 | 2 | 4", "7"),
        ("int", "(2147483647 & 7) ^ 3", "4"),
        ("int", "~2147483647", "-2147483648"),
        ("uint", "let a: uint = 0; ~a", "4294967295"),
        ("uint", "let a: uint = 2147483648; a | 1", "2147483649"),
        (
            "uint",
            "let a: uint = 4294967295; let b: uint = 2147483648; a ^ b",
            "2147483647",
        ),
    ] {
        let lua = emit_clean(
            &format!("fn folded_mask() -> {ty} {{ {expr} }}"),
            "folded_mask",
        );
        assert!(!lua.contains("bit.b"), "{expr}: {lua}");
        let symbol = emitted_symbol(&lua);
        luajit_ok(&format!("{lua}\nassert({symbol}() == {expected})"), &lua);
    }
    let lua = emit_clean("fn wide() -> int { 2147483648 | 1 }", "wide");
    assert!(lua.contains("bit.bor"), "{lua}");
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
            "take_while chain must stay zero-alloc, fully inlined:\n{lua}"
        );
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
    let lua = emit_clean(
        "fn choose(a: int, b: int) -> int { return if a < b { a } else { b } }",
        "choose",
    );
    assert!(lua.contains("math.min("), "{lua}");
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

/// Like [`emit_std`] but runs the shipped non-inlining body chain
/// (`skull_transform::body_chain::convert_body`) before emitting. Needed to
/// assert allocation-shape claims (a dead `MakeTuple` left by the raw lowering is
/// removed here, so `no table` assertions reflect what actually runs).
fn emit_clean(src: &str, fn_name: &str) -> String {
    emit_clean_with(src, fn_name, |_| {})
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

/// [`emit_clean`] under a context `configure` adjusts first.
fn emit_clean_with(src: &str, fn_name: &str, configure: impl FnOnce(&mut CodegenCtx)) -> String {
    let (ws, mid) = stdlib_workspace(src);
    let fid = test_fn(&ws, mid, fn_name);
    let (mut func, interner, _) = common::lower_test_ref_with_graph(&ws, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message));
    convert(&ws, mid, fid, &mut func);
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

/// Stdlib-loaded: needed for `__op_*` arithmetic overloads.
fn emit_std(src: &str, fn_name: &str) -> String {
    let modules = stdlib_with(src);
    emit_dump(modules, fn_name)
}

#[test]
fn int_literal_and_return() {
    // The `5` is a single-use pure value -> tree-folded into the return (no
    // register, no `local`).
    let lua = emit("fn five() -> int { 5 }", "five");
    assert!(lua.contains("do return (5) end"), "{lua}");
    assert!(!lua.contains("local "), "{lua}");
}

#[test]
fn repeated_nil_is_written_at_each_use_without_a_register() {
    let lua = emit_clean(
        "struct Pair { a: int?, b: int? } fn absent(flag: bool) -> Pair { let value: int? = null; if flag { return Pair { a: value, b: value } } return Pair { a: value, b: null } }",
        "absent",
    );
    assert!(
        !lua.lines()
            .any(|line| line.trim_start().starts_with("local r")),
        "discarded validators must not create a factory:\n{lua}"
    );
    let symbol = emitted_symbol(&lua);
    let driver = format!(
        "{lua}\nfor i = 1, 10000 do local p = {symbol}(i % 2 == 0); assert(p.a == nil and p.b == nil) end"
    );
    luajit_ok(&driver, &lua);
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
    let lua = emit("fn id(x: int) -> int { x }", "id");
    assert!(lua.contains("function id_int(c0)"), "{lua}");
    assert!(lua.contains("do return c0 end"), "{lua}");
}

#[test]
fn int_arithmetic() {
    let lua = emit_std("fn add(a: int, b: int) -> int { a + b }", "add");
    assert!(lua.contains("function add_int_int(c0, c1)"), "{lua}");
    assert!(lua.contains("do return (c0 + c1) end"), "{lua}");
}

#[test]
fn inline_lua_with_inline_annotation_is_spliced() {
    // `@inline inline(lua)` is SPLICED at the call site (a read-only param
    // reads the caller's local, tail `return X` becomes `<result> = X`) instead of emitting a
    // `callee(args)` call -- so the body runs in the caller's scope (LuaJIT can
    // sink any cdata it produces; a call boundary blocks that).
    let body = emit(
        "@inline inline(lua) fn dbl(x: int) -> int = \"return x * 2\"\n\
         fn use_it(n: int) -> int { return dbl(n) }",
        "use_it",
    );
    assert!(!body.contains("dbl_int("), "{body}");
    assert!(
        body.contains(" = c0 * 2\n") && !body.contains("local x"),
        "a read-only param should read the caller's local:\n{body}"
    );
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
fn fake_spliced_control_in_long_strings_and_comments_does_not_count() {
    let fake_control = "goto bb0\n::bb0::\n".repeat(16_370);
    let src = format!(
        "@inline inline(lua) fn many() = \"\"\"\nlocal text = [=[\n{fake_control}]=]\n\
         --[=[\n{fake_control}]=]\n\"\"\"\nfn run() {{ many() }}"
    );
    let (lua, rejected) = emit_run(&src);
    assert!(
        rejected.is_none(),
        "inert long-string and comment text caused a rejection"
    );
    luajit_loads(&lua);
}

#[test]
fn fake_spliced_assignments_in_a_long_string_do_not_extend_a_real_jump() {
    let fake_assignments = "r0 = 1\n".repeat(32_768);
    let src = format!(
        "@inline inline(lua) fn many() = \"\"\"\nlocal text\ngoto bb0\n\
         text = [=[\n{fake_assignments}]=]\n::bb0::\n\"\"\"\nfn run() {{ many() }}"
    );
    let (lua, rejected) = emit_run(&src);
    assert!(
        rejected.is_none(),
        "inert assignments extended the emitted jump"
    );
    luajit_loads(&lua);
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
    assert!(body.contains("dbl_int("), "{body}");
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
    assert!(lua.contains("((1), (7), (0.5), (\"x\"), (true))"), "{lua}");
    let lua = emit_clean(src, "go_named");
    assert!(lua.contains("((1), (7), (0.5), (\"y\"), (true))"), "{lua}");
}

#[test]
fn a_null_default_emits_no_callee_prologue_branch() {
    let src = "fn g(a: int, b: int? = null) -> int { a }";
    let lua = emit_clean(src, "g");
    assert!(!lua.contains("(nil)"), "{lua}");
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
fn uint_checks_emit_no_closures() {
    for src in [
        "fn checked(value: int) -> uint { value as uint }",
        "fn checked(value: float) -> uint { value as uint }",
        "fn checked(value: uint, count: uint) -> uint { value << count }",
        "fn checked(value: uint, count: uint) -> uint { value >> count }",
    ] {
        let lua = emit(src, "checked");
        assert!(
            !lua.lines().skip(1).any(|line| line.contains("function")),
            "{lua}"
        );
    }
}

#[test]
fn uint_constant_shift_counts_need_no_helper() {
    for op in ["<<", ">>"] {
        for count in [0, 1, 16, 31] {
            let src = format!("fn shift(value: uint) -> uint {{ value {op} {count} }}");
            for lua in [emit(&src, "shift"), emit_clean_bound(&src, "shift")] {
                assert!(!lua.contains("__rock_u32_shift_count"), "{lua}");
            }
        }
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
fn emits_interpolation_as_flat_concat() {
    let body = emit(
        "fn label(a: int, b: int) -> string { \"x={a}/{b} done\" }",
        "label",
    );
    assert!(!body.contains("((("), "{body}");
    assert!(body.matches(" .. ").count() >= 4, "{body}");
}

// --- Multi-return ABI for tuple-returning fns ---

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
    assert!(!body.contains("[1]") && !body.contains("[2]"), "{body}");
    assert!(body.contains("= pair_0()"), "{body}");
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
 if (c0 < (0)) then
 c0 = (0)
 else
 c0 = (1)
 end
 do return c0 end
end
"
    );
}

/// The shipped body chain with inlining (`skull_transform::body_chain::prepare_body`),
/// then emit. The honest artifact for zero-alloc claims: an `@inline`
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
fn for_user_iter_inline_is_counted_and_zero_alloc() {
    let lua = emit_inlined(ZERO_ALLOC_ITER_SRC, "sum_range");
    assert_counted_zero_alloc(&lua);
    assert!(!lua.contains(">="), "{lua}");
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
fn for_vec_backed_iter_inline_is_counted_and_zero_alloc() {
    let lua = emit_inlined(ZERO_ALLOC_VEC_BACKED_SRC, "sum_myvec");
    assert_counted_zero_alloc(&lua);
    assert!(!lua.lines().any(|l| l.contains(">=")), "{lua}");
}

// --- A REAL `[T]` for-loop routes through the stdlib
// VecIter protocol (no skull template) and still compiles to the same
// zero-alloc native `for` the deleted template emitted. Parity measured at
// ~1.0x, 0 bytes/elem (benchmarks/iterator-zero-alloc/vec_parity.rock).
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

/// `[T]` loops of every common shape print as counted `for`s with no
/// protocol call and no back edge: a value element, `continue` and `break`,
/// a string element and an early return.
const VEC_LOOP_SHAPES_SRC: &str = "
value P { x: float, y: float }
fn sum_p(ps: [P]) -> float {
    let t = 0.0
    for p in ps { t += p.x * p.y }
    return t
}
fn skip_odd(xs: [int]) -> int {
    let t = 0
    for x in xs {
        if x % 2 == 1 { continue }
        if x > 100 { break }
        t += x
    }
    return t
}
fn total_len(ss: [string]) -> int {
    let n = 0
    for s in ss { n += s.len() }
    return n
}
fn find(xs: [int], key: int) -> int {
    for x in xs { if x == key { return x * 10 } }
    return -1
}
";

#[test]
fn vec_loop_shapes_are_counted_with_no_back_edges() {
    for name in ["sum_p", "skip_odd", "total_len", "find"] {
        let lua = emit_inlined(VEC_LOOP_SHAPES_SRC, name);
        assert!(lua.contains("for "), "{lua}");
        assert!(!lua.contains("next"), "{lua}");
        assert!(backward_gotos(&lua).is_empty(), "{lua}");
    }
}

/// A `[T]` loop is counted when lowered, not only after inlining recovers
/// it: a body that never inlines still prints a numeric `for`.
#[test]
fn vec_loop_is_counted_without_inlining() {
    let lua = emit_clean(
        "fn sum_vec(xs: [int]) -> int { let total = 0; for x in xs { total += x }; return total }",
        "sum_vec",
    );
    assert!(lua.contains("for "), "{lua}");
    assert!(!lua.contains("next") && !lua.contains("iter"), "{lua}");
    let lua = emit_clean(
        "fn sum_pairs(xs: [(int, int)]) -> int { let total = 0; for (a, b) in xs { total += a * b }; return total }",
        "sum_pairs",
    );
    assert!(lua.contains("for "), "{lua}");
    assert!(!lua.contains("next") && !lua.contains("iter"), "{lua}");
}

#[test]
fn iterator_chains_are_counted_zero_alloc_and_fully_inlined() {
    for chain in [
        "map(x => x * 2)",
        "filter(x => x > 2)",
        "map(x => x * 2).filter(x => x > 2)",
        "take_while(x => x < 10)",
        "filter(x => x % 2 == 0).take_while(x => x < 100)",
    ] {
        let lua = emit_inlined(
            &format!(
                "fn sum(xs: [int]) -> int {{ let total = 0; for x in xs.iter().{chain} {{ total += x }}; return total }}"
            ),
            "sum",
        );
        assert_counted_zero_alloc(&lua);
    }
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

// -- Emitted tail calls -----------------------------------------------------
//
// A call in tail position is forwarded as `do return f(...) end`, which keeps
// Lua's tail-call elimination for recursive calls.

#[test]
fn a_tail_call_is_forwarded() {
    let src = "fn leaf(a: int) -> int { a + 1 }\nfn forward(a: int) -> int { return leaf(a) }";
    let lua = emit_clean(src, "forward");
    assert!(lua.contains("do return leaf_int(c0) end"), "{lua}");
}

#[test]
fn a_self_recursive_tail_call_is_forwarded() {
    let src = "fn down(n: int, acc: int) -> int { if n == 0 { return acc }\n  return down(n - 1, acc + 1) }";
    let lua = emit_clean(src, "down");
    assert!(lua.contains("do return down_int_int("), "{lua}");
}

// Callee binding

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
    convert(&ws, mid, fid, &mut func);
    check(&ws, &func);
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
fn callee_binding_ignores_calls_removed_by_cleanup() {
    let src = "fn discarded(a: int) -> int { a + 2 }\nfn live(a: int) -> int { a + 1 }\nfn caller(a: int) -> int { if false { discarded(a) } else { live(a) } }";
    let lua = emit_clean_bound_checked(src, "caller", |ws, func| {
        use skull_mir::mir::Inst;
        let is_discarded = |inst: &Inst| {
            matches!(inst,
                Inst::Call { func: callee, .. }
                    if declared_name_matches(
                        ws.module(callee.module).functions[callee.func].name,
                        "discarded",
                    )
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
        lua.starts_with("__rock_mk_caller_int = function(b0) return function("),
        "{lua}"
    );
    assert!(!lua.contains("discarded_int"), "{lua}");
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
        "{lua}"
    );
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
        assert!(!lua.contains("SlotMetadata_vt("), "{lua}");
    }
}

/// The bound shape: a factory named for the function, one parameter per
/// distinct callee, body calls through those parameters, and the binding line
/// that calls the factory with the callee globals.
#[test]
fn callee_binding_upvalue_wraps_the_body_in_a_factory() {
    let src =
        "fn leaf(a: int) -> int { a + 1 }\nfn caller(a: int) -> int { leaf(a) + leaf(a + 1) }";
    let unbound = emit_clean(src, "caller");
    let bound = emit_clean_bound(src, "caller");
    assert!(
        bound.starts_with("__rock_mk_caller_int = function("),
        "expected the factory header:\n{bound}"
    );
    let header = bound.lines().next().expect("factory header");
    let param = header
        .split_once("= function(")
        .expect("factory header")
        .1
        .split_once(") return function(")
        .expect("factory header")
        .0;
    assert!(
        !param.is_empty() && !param.contains(", "),
        "expected one captured callee parameter:\n{header}"
    );
    assert!(
        bound.contains("caller_int = __rock_mk_caller_int(leaf_int)\n"),
        "expected the binding line:\n{bound}"
    );
    // Only direct calls are rewritten to read the factory parameter. The
    // binding line still reads the global callee at initialization time.
    let body = unbound
        .strip_prefix("function caller_int")
        .expect("unbound definition")
        .trim_end_matches('\n');
    let rewritten_body = body.replace("leaf_int", param);
    assert!(
        bound.contains(&format!("return function{rewritten_body} end\n")),
        "expected the bound body with direct calls rewritten:\n{bound}\n--- body ---\n{rewritten_body}"
    );
    assert!(
        bound.starts_with("__rock_mk_caller_int = function(b0) return function("),
        "expected a short private factory parameter:\n{bound}"
    );
    assert_eq!(bound.matches("b0(").count(), 2, "{bound}");
    assert!(
        bound.contains("caller_int = __rock_mk_caller_int(leaf_int)\n"),
        "the binding line must retain the global callee name:\n{bound}"
    );
}

#[test]
fn callee_binding_short_names_skip_public_globals() {
    let src = "fn leaf(a: int) -> int { a + 1 }\n\
               @nomangle fn b0(a: int) -> int { a + 2 }\n\
               fn caller(a: int) -> int { leaf(a) + b0(a) }";
    let bound = emit_clean_bound(src, "caller");
    assert!(
        bound.starts_with("__rock_mk_caller_int = function(b1) return function("),
        "the short private parameter must skip the public b0 global:\n{bound}"
    );
    assert!(
        bound.contains("b1(c0)") && bound.contains("b0(c0)"),
        "expected the private leaf alias and public call:\n{bound}"
    );
    assert!(
        bound.contains("caller_int = __rock_mk_caller_int(leaf_int)\n"),
        "expected the binding line to read the leaf global:\n{bound}"
    );
}

/// A factory alias never takes a name that spliced inline(lua) text reads
/// freely: the alias would capture the text's global.
#[test]
fn callee_binding_aliases_skip_names_inline_lua_reads() {
    let src = "fn leaf(a: int) -> int { a + 1 }\n\
               @inline inline(lua) fn raw(a: int) -> int = \"return b0(a)\"\n\
               fn caller(a: int) -> int { leaf(a) + raw(a) }";
    let bound = emit_clean_bound(src, "caller");
    assert!(
        bound.starts_with("__rock_mk_caller_int = function(b1) return function("),
        "the alias must skip the inline text's free name:\n{bound}"
    );
    assert!(
        bound.contains("b1(c0)") && bound.contains("r1 = b0(c0)"),
        "expected the aliased capture and the live inline Lua body:\n{bound}"
    );
    assert!(
        bound.contains("caller_int = __rock_mk_caller_int(leaf_int)\n"),
        "expected the factory to read the global leaf function:\n{bound}"
    );
}

/// Aliased captures called around a spliced inline(lua) body inside a loop,
/// where the splice declares a local spelled like an alias, run the same as
/// the source says.
#[test]
fn aliased_captures_around_a_spliced_body_in_a_loop_run_correctly() {
    let src = "fn leaf(a: int) -> int { a + 1 }\n\
               fn other(a: int) -> int { a * 3 }\n\
               @inline inline(lua) fn twice(a: int) -> int = \"local b0 = a * 2\\nreturn b0\"\n\
               fn caller(n: int) -> int {\n\
                 let s = 0\n\
                 for i in 0..n { s = s + leaf(twice(i)) + other(i) }\n\
                 return s\n\
               }";
    let bound = emit_clean_bound(src, "caller");
    assert!(
        bound.starts_with("__rock_mk_caller_int = function(b1, b2) return function("),
        "expected aliased captures that skip the splice's local:\n{bound}"
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
        bound.starts_with("__rock_mk_is_odd_int = function(b0) return "),
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
        bound.starts_with("__rock_mk_down_int = function(b0) return "),
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
    assert_eq!(
        params.split(", ").collect::<Vec<_>>(),
        (0..cap).map(|i| format!("b{i}")).collect::<Vec<_>>(),
        "expected the first {cap} callees assigned short names in name order:\n{params}"
    );
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

/// An element of a vector type is a table, so like a scalar it fills every
/// live slot: reading one cannot reach a backing metamethod, and the loop's
/// proof survives it.
#[test]
fn loops_over_vector_elements_read_raw() {
    let lua = emit_inlined(
        "fn sum(rows: [[int]]) -> int {
            let t = 0
            for row in rows { for x in row { t += x } }
            return t
        }",
        "sum",
    );
    assert!(!lua.contains("__vec_checked_get("), "{lua}");
}

/// A `[bool]` read the loop proves is the slot itself, even beside an
/// unproven read of a vector of vectors, which cannot reach a metamethod.
#[test]
fn proven_bool_reads_beside_table_element_reads_are_raw() {
    let lua = emit_inlined(
        "fn sum(live: [bool], rows: [[int]]) -> int {
            let t = 0
            for i in 0..live.len() { if live[i] { t += rows[i].len() } }
            return t
        }",
        "sum",
    );
    assert_eq!(lua.matches("__vec_checked_get(").count(), 1, "{lua}");
    assert!(
        lua.lines()
            .filter(|l| l.contains("if "))
            .all(|l| !l.contains("__vec_checked_get(")),
        "{lua}"
    );
}

/// The arena walk reads its occupancy column under the loop's own bound. A
/// heap record field read may run a host getter that shrinks the column, so
/// with one in the body the read stays checked, but a free (`false`) slot
/// still makes no call.
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
fn vec_loop_reads_its_element_directly() {
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
    println!("{lua}");
    assert_eq!(lua.matches("pick").count(), 1, "{lua}");
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
fn whole_gc_snapshots_read_only_demanded_leaves() {
    let src = include_str!("../../../tests/values/demanded_value_copy.rock");
    for name in ["consume", "read_only", "read_produced"] {
        let lua = emit_clean(src, name);
        assert!(
            !lua.contains(".ignored"),
            "unused child read in {name}:\n{lua}"
        );
        assert!(
            !lua.contains("unused7"),
            "unused leaf read in {name}:\n{lua}"
        );
        assert!(
            !lua.contains(".spare"),
            "unused nested leaf in {name}:\n{lua}"
        );
        assert!(
            !lua.contains("Detail_ct("),
            "child repack in {name}:\n{lua}"
        );
        assert!(
            !lua.contains("Snapshot_vt("),
            "root repack in {name}:\n{lua}"
        );
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

    let register = "fn g(a: int, b: int) -> float { let s = a * b; let y = s as float; y * y + y + (s + 1) as float }";
    let lua = emit_clean(register, "g");
    assert_eq!(register_moves(&lua), Vec::<&str>::new(), "{lua}");
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

#[test]
fn residual_rock_calls_and_returns_drop_trailing_nils() {
    // Lua reads a missing trailing argument or return value as nil, so a call
    // to a fixed-arity Rock function and a multi-value return need not spell
    // them. The first returned value stays so the return keeps its arity.
    let lua = emit_clean(
        "value Pair { a: int, b: int } \
         fn keep(a: int, b: int?, c: string?) -> int { if b == null { return a } return a + 1 } \
         fn pick(x: int) -> Pair? { if x > 3 { return null } return Pair { a: x, b: x } } \
         fn run(x: int) -> int { return keep(x, null, null) + keep(x, 1, null) }",
        "run",
    );
    assert!(!lua.contains("(nil)"), "{lua}");
    let lua = emit_clean(
        "value Pair { a: int, b: int } \
         fn pick(x: int) -> Pair? { if x > 3 { return null } return Pair { a: x, b: x } }",
        "pick",
    );
    assert!(lua.contains("do return (false) end"), "{lua}");
    assert!(!lua.contains("(nil)"), "{lua}");
}

#[test]
fn foreign_calls_keep_trailing_nils() {
    // An extern Lua function may count its arguments.
    let lua = emit_clean(
        "extern(lua) fn host(a: int, b: int?) -> int \
         inline(lua) fn wrap(a: int, b: int?) -> int = \"return select('#', a, b)\" \
         fn run(x: int) -> int { return host(x, null) + wrap(x, null) }",
        "run",
    );
    assert_eq!(lua.matches("(nil))").count(), 2, "{lua}");
}

#[test]
fn specialized_symbols_are_spelled_per_context() {
    // One instance key names a different function in each workspace. A worker
    // that spelled it for one context must not answer the next from its table.
    let interner = skull_mir::monoty::MonoInterner::new();
    let int = interner.intern(skull_mir::monoty::MonoTy::Int);
    let spell = |src: &str| {
        let mut modules =
            vec![skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse")];
        let ws =
            bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
        let mid = *ws
            .module_ids
            .get(&ustr::Ustr::from("<test>"))
            .expect("user module id");
        let (fid, _) = ws
            .module(mid)
            .functions
            .iter()
            .next()
            .expect("the declared function");
        let inst = skull_mir::instance::Instance {
            func: FuncRef {
                module: mid,
                func: fid,
            },
            args: vec![int],
        };
        let ctx = CodegenCtx::new(&ws);
        let symbol = ctx.instance_symbol(&ws, &interner, &inst);
        assert_eq!(symbol.as_str(), inst.symbol(&ws, &interner));
        assert_eq!(ctx.instance_symbol(&ws, &interner, &inst), symbol);
        (inst, symbol)
    };
    let (first_inst, first) = spell("fn first<T>(x: T) -> T { return x }");
    let (second_inst, second) = spell("fn second<T>(x: T) -> T { return x }");
    assert_eq!(first_inst, second_inst);
    assert_ne!(first, second);
}

/// `xs.len()` and `vec_len(xs)` on a `[T]` lower to the length read itself:
/// with no inlining there is still no call to make.
#[test]
fn vec_len_lowers_to_the_length_read() {
    let src = "fn n(xs: [int], ys: [string]) -> int { return xs.len() + vec_len(ys) }";
    let lua = emit_clean(src, "n");
    assert!(lua.contains("c0.len") && lua.contains("c1.len"), "{lua}");
    assert!(!lua.contains("len_") && !lua.contains("vec_len"), "{lua}");
}

/// `xs.push(x)` is the append itself, not a Rock-bodied forward to
/// `vec_push` that inlining has to splice first.
#[test]
fn vec_push_method_is_the_append_without_inlining() {
    let src = "fn add(xs: [int], x: int) -> int { xs.push(x); xs.push(x + 1); return xs.len() }";
    let lua = emit_clean(src, "add");
    assert!(!lua.contains("push"), "{lua}");
    assert_eq!(lua.matches("c0.len = c0.len + 1").count(), 2, "{lua}");
}

/// A `[T]` store whose index a counted loop keeps below the vector's length
/// is the raw backing write: no check, no call.
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

/// A call in the body may shrink the vector, so the store keeps its check. A
/// `@pure` callee keeps it too: the annotation admits `@debug_impure` callees
/// and unverified opaque bodies, so it does not prove the vector untouched.
#[test]
fn a_call_in_the_loop_body_keeps_the_check_even_when_pure() {
    let lua = emit_inlined(
        "fn shrink(xs: [int]) { xs.pop() }
        fn fill(xs: [int]) {
            for i in 0..xs.len() { xs[i] = i; shrink(xs) }
        }",
        "fill",
    );
    assert!(lua.contains("__vec_checked_set("), "{lua}");
    // A `@pure` callee is proven harmless by reading its body, not by the
    // annotation: a `@pure` wrapper around a `@debug_impure` pop keeps the
    // check, since the effect walk sees the pop.
    let lua = emit_inlined(
        "@pure fn weight(i: int) -> int { return i * 3 }
        fn fill(xs: [int]) {
            for i in 0..xs.len() { xs[i] = weight(i) }
        }",
        "fill",
    );
    assert!(!lua.contains("__vec_checked_set("), "{lua}");
    let lua = emit_inlined(
        "@debug_impure fn debug_pop(xs: [int]) -> int { xs.pop(); return 0 }
        @pure fn via(xs: [int]) -> int { return debug_pop(xs) }
        fn fill(xs: [int]) {
            for i in 0..xs.len() { xs[i] = via(xs) }
        }",
        "fill",
    );
    assert!(lua.contains("__vec_checked_set("), "{lua}");
}

/// A foreign vector's backing table can call __newindex when push appends.
#[test]
fn a_push_in_the_loop_body_keeps_the_check() {
    let lua = emit_inlined(
        "fn fill(xs: [int], out: [int]) {
            for i in 0..xs.len() { xs[i] = i; if i % 7 == 0 { out.push(i) } }
        }",
        "fill",
    );
    assert!(lua.contains("__vec_checked_set("), "{lua}");
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

/// A call in the loop body keeps the proof when the whole-program effect
/// table says the callee writes nothing the loop's facts depend on: a body
/// that only reads, a body whose writes name other fields, a foreign C
/// function, an opaque body marked `@nowrite`, and one marked
/// `@writes_elems`. A body that pops, or writes the field the vector is
/// read from, keeps the check.
#[test]
fn a_callee_that_provably_leaves_the_vector_alone_keeps_the_proof() {
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
fn map_remove_keeps_the_check_on_a_vector_element_index() {
    let lua = emit_inlined(
        "fn lookup(xs: [int], indices: [int], m: Map<string, int>, key: string) -> int {
            if indices[0] < 0 || indices[0] >= xs.len() { return 0 }
            m.remove(key)
            return xs[indices[0]]
        }",
        "lookup",
    );
    assert_eq!(lua.matches("__vec_checked_get(").count(), 3, "{lua}");
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

#[test]
fn tuple_lookup_can_shrink_a_guarded_vector() {
    let src = r#"
fn lookup(xs: [int], t: any) -> int {
    if xs.len() <= 0 { return -1 }
    let value: int = tuple_nth(t, 1)
    return xs[0] + value
}
"#;
    let lua = emit_inlined(src, "lookup");
    assert!(lua.contains("__vec_checked_get("), "{lua}");
}

#[test]
fn a_registered_ffi_callback_can_run_through_another_binding() {
    let lua = emit_inlined(
        r#"
extern(ffi, "probe") fn register(callback: fn())
extern(ffi, "probe") fn tick() -> int
fn fill(xs: [int]) { for i in 0..xs.len() { xs[i] = tick() } }
"#,
        "fill",
    );
    assert!(lua.contains("__vec_checked_set("), "{lua}");
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
/// behind a fresh state parameter, and emit. For frame-assignment shapes.
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
