//! Programs compiled by the shipped pipeline, then driven from Lua: each
//! driver is a Lua expression over the `<test>` module's functions, called by
//! their source names, so a test can hand a body boundary values Rock cannot
//! spell (a raw table, an out-of-range number, a metatable that mutates).

use crate::common;
use std::collections::HashMap;

/// Emit `src` as a program, alias each uniquely named `<test>` function to its
/// source name, and print the Lua expression `driver` under luajit.
fn run(src: &str, driver: &str) -> String {
    common::require_luajit();
    let ws = common::ws_with_stdlib(src);
    let mid = common::module_id(&ws, "<test>");
    let lua = skull::mir_emit::emit_workspace_mir(&ws, mid)
        .expect("emit")
        .lua;
    let functions = &ws.module(mid).functions;
    let mut counts: HashMap<_, usize> = HashMap::new();
    for (_, f) in functions.iter() {
        *counts.entry(f.name).or_default() += 1;
    }
    let mut aliases = String::new();
    for (_, f) in functions.iter() {
        let mangled = skull_mir::mangle::mangle_fn(&ws, f);
        if counts[&f.name] == 1 && mangled != f.name.as_str() {
            aliases.push_str(&format!("_G[\"{}\"] = {mangled}\n", f.name));
        }
    }
    common::run_luajit(&format!("{lua}\n{aliases}print({driver})\n"))
}

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
fn exec_uint_constant_shifts_preserve_unsigned_results() {
    let mut src = String::new();
    let mut driver = String::from("(function() ");
    for count in [0, 1, 16, 31] {
        src.push_str(&format!(
            "fn left{count}(value: uint) -> uint {{ value << {count} }}\n\
             fn right{count}(value: uint) -> uint {{ value >> {count} }}\n"
        ));
        for value in [0_u32, 1, 2147483648, u32::MAX] {
            driver.push_str(&format!(
                "assert(left{count}({value}) == {}); assert(right{count}({value}) == {}); ",
                value << count,
                value >> count,
            ));
        }
    }
    driver.push_str("return true end)()");
    assert_eq!(run(&src, &driver), "true");
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

const VEC_FALSY_SLOTS_SRC: &str = "
fn count_false(xs: [bool]) -> int {
    let n = 0
    for x in xs { if !x { n += 1 } }
    return n
}
fn count_null(xs: [int?]) -> int {
    let n = 0
    for x in xs { if x == null { n += 1 } }
    return n
}
fn at(xs: [int], i: int) -> int { return xs[i] }
fn probe() -> string {
    let a = count_false([true, false, false])
    let b = count_null([1, null, 3, null, null])
    return \"{a} {b} {at([4, 5, 6], 2)}\"
}
fn past_end() -> int { return at([4, 5, 6], 3) }
fn before_start() -> int { return at([4, 5, 6], -1) }";

#[test]
fn vec_reads_of_false_and_null_slots_and_out_of_bounds_indices() {
    assert_eq!(run(VEC_FALSY_SLOTS_SRC, "probe()"), "2 3 6");
    for driver in ["past_end()", "before_start()"] {
        let out = run(
            VEC_FALSY_SLOTS_SRC,
            &format!("select(2, pcall(function() return {driver} end))"),
        );
        assert!(out.contains("Vec index out of bounds"), "{driver}: {out}");
    }
}

/// A user function that shares the name is still called.
#[test]
fn a_user_len_over_a_vector_is_still_called() {
    let src = "
struct Row { n: int }
fn len(rows: [Row]) -> int { return 40 + rows.len }
fn probe() -> int { return len([Row { n: 1 }, Row { n: 2 }]) }";
    assert_eq!(run(src, "probe()"), "42");
}

/// Under luajit the raw stores write the slots and a check the compiler kept
/// still raises.
#[test]
fn proven_stores_compute_the_same_under_luajit() {
    let src = "\
fn fill(xs: [int]) -> int {\n\
    for i in 0..xs.len() { xs[i] = xs[i] + i }\n\
    return xs[xs.len() - 1]\n\
}\n\
fn probe(n: int) -> int {\n\
    let xs: [int] = []\n\
    for i in 0..n { xs.push(10) }\n\
    fill(xs)\n\
    return fill(xs)\n\
}";
    assert_eq!(run(src, "probe(5)"), "18");
    assert_eq!(run(src, "probe(1)"), "10");
}

#[test]
fn map_remove_can_mutate_a_vector_index_through_lua_metatable() {
    let src = "
fn held(xs: [int], indices: [int], m: Map<string, [int]>) -> int {
    if indices[0] < 0 || indices[0] >= xs.len() { return -1 }
    m.remove(\"held\")
    return xs[indices[0]]
}

fn removed(xs: [int], m: Map<string, int>) -> int {
    if m[\"key\"] < 0 || m[\"key\"] >= xs.len() { return -1 }
    m.remove(\"key\")
    return xs[m[\"key\"]]
}
fn shrunk(xs: [int], m: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    m.remove(\"missing\")
    return xs[0]
}";
    assert_eq!(
        run(
            src,
            r#"(function()
                local xs = { len = 2, _data = { 11, 22 } }
                local indices = { len = 1, _data = { 1 } }
                local m = { held = indices }
                assert(held(xs, indices, m) == 22 and m.held == nil)
                local ok, err = pcall(removed, xs, { key = 1 })
                assert(not ok and tostring(err):find("attempt to compare nil", 1, true), tostring(err))
                local changed = { len = 1, _data = { 1 } }
                local foreign = setmetatable({}, { __index = function()
                    changed._data[1] = 99
                    return nil
                end })
                ok, err = pcall(held, xs, changed, foreign)
                assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), tostring(err))
                local target = { len = 1, _data = { 7 } }
                local shrinking = setmetatable({}, { __index = function()
                    target.len = 0
                    target._data[1] = nil
                    return nil
                end })
                ok, err = pcall(shrunk, target, shrinking)
                assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), tostring(err))
                return "ok"
            end)()"#,
        ),
        "ok"
    );
}

#[test]
fn map_get_metatable_can_shrink_a_guarded_vector() {
    let src = "fn read_after_get(xs: [int], m: Map<string, int>) -> int {
        if xs.len() <= 0 { return -1 }
        let old = m.get(\"missing\")
        if old != null { return old }
        return xs[0]
    }";
    let out = run(
        src,
        r#"(function()
            local xs = { len = 1, _data = { 7 } }
            local m = setmetatable({}, { __index = function()
                xs.len = 0
                xs._data[1] = nil
                return nil
            end })
            local ok, err = pcall(read_after_get, xs, m)
            assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), tostring(err))
            return "ok"
        end)()"#,
    );
    assert_eq!(out, "ok");
}

#[test]
fn table_stores_can_shrink_a_guarded_vector() {
    let src = r#"
fn via_table_set(xs: [int], t: any) -> int {
    if xs.len() <= 0 { return -1 }
    table_set(t, "missing", 1)
    return xs[0]
}

fn via_table_unset(xs: [int], t: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    table_unset(t, "missing")
    return xs[0]
}
fn via_vec_set(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    __vec_data_set(ys, 0, 1)
    return xs[0]
}
fn via_vec_unset(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    __vec_data_unset(ys, 0)
    return xs[0]
}
"#;
    let out = run(
        src,
        r#"(function()
            local function check(f, t)
                local xs = { len = 1, _data = { 7 } }
                local mt = { __newindex = function()
                    xs.len = 0
                    xs._data[1] = nil
                end }
                local ok, err = pcall(f, xs, t(mt))
                assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), tostring(err))
            end
            check(via_table_set, function(mt) return setmetatable({}, mt) end)
            check(via_table_unset, function(mt) return setmetatable({}, mt) end)
            check(via_vec_set, function(mt) return { len = 1, _data = setmetatable({}, mt) } end)
            check(via_vec_unset, function(mt) return { len = 1, _data = setmetatable({}, mt) } end)
            return "ok"
        end)()"#,
    );
    assert_eq!(out, "ok");
}

#[test]
fn map_index_metamethods_can_shrink_a_guarded_vector() {
    let src = r#"
fn map_read(m: Map<string, int>) -> int { return m["missing"] }
fn via_read(xs: [int], m: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    let value = m["missing"]
    return xs[0] + value
}

fn via_nested_read(xs: [int], m: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    let value = map_read(m)
    return xs[0] + value
}
fn via_write(xs: [int], m: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    m["missing"] = 1
    return xs[0]
}
fn via_repeated_read(xs: [int], m: Map<string, int>) -> int {
    if xs.len() <= 0 { return -1 }
    let first = xs[0]
    let value = m["missing"]
    return xs[0] + first + value
}
"#;
    let out = run(
        src,
        r#"(function()
            local function check(f, key)
                local xs = { len = 1, _data = { 7 } }
                local mt = { [key] = function()
                    xs.len = 0
                    xs._data[1] = nil
                    return 1
                end }
                local m = setmetatable({}, mt)
                local ok, err = pcall(f, xs, m)
                assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), tostring(err))
            end
            check(via_read, "__index")
            check(via_nested_read, "__index")
            check(via_write, "__newindex")
            check(via_repeated_read, "__index")
            return "ok"
        end)()"#,
    );
    assert_eq!(out, "ok");
}

#[test]
fn nullable_vec_index_metamethods_can_shrink_a_guarded_vector() {
    let src = r#"
fn read_nullable(ys: [int?]) -> int? { return ys[0] }
fn write_nullable(ys: [int?]) { ys[0] = 1 }
fn via_read(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    let value = ys[0]
    if value != null { return -2 }
    return xs[0]
}
fn via_nested_read(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    let value = read_nullable(ys)
    if value != null { return -2 }
    return xs[0]
}
fn via_write(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    ys[0] = 1
    return xs[0]
}
fn via_nested_write(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    write_nullable(ys)
    return xs[0]
}
fn via_discarded_read(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 || ys.len() <= 0 { return -1 }
    ys[0]
    return xs[0]
}
"#;
    let out = run(
        src,
        r#"(function()
            local function check(f, key)
                local xs = { len = 1, _data = { 7 } }
                local mt = { [key] = function()
                    xs.len = 0
                    xs._data[1] = nil
                    return nil
                end }
                local ys = { len = 1, _data = setmetatable({}, mt) }
                local ok, err = pcall(f, xs, ys)
                assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), tostring(err))
            end
            check(via_read, "__index")
            check(via_nested_read, "__index")
            check(via_write, "__newindex")
            check(via_nested_write, "__newindex")
            check(via_discarded_read, "__index")
            return "ok"
        end)()"#,
    );
    assert_eq!(out, "ok");
}

#[test]
fn foreign_vec_push_can_shrink_a_guarded_vector() {
    let src = r#"
fn via_push(xs: [int], ys: [int]) -> int {
    if xs.len() <= 0 { return -1 }
    ys.push(1)
    return xs[0]
}
fn via_vec_push(xs: [int], ys: [int]) -> int {
    if xs.len() <= 0 { return -1 }
    vec_push(ys, 1)
    return xs[0]
}
"#;
    let out = run(
        src,
        r#"(function()
            local function check(f)
                local xs = { len = 1, _data = { 7 } }
                local ys = { len = 0, _data = setmetatable({}, { __newindex = function()
                    xs.len = 0
                    xs._data[1] = nil
                end }) }
                local ok, err = pcall(f, xs, ys)
                assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), tostring(err))
            end
            check(via_push)
            check(via_vec_push)
            return "ok"
        end)()"#,
    );
    assert_eq!(out, "ok");
}

#[test]
fn equality_helper_can_shrink_a_guarded_vector() {
    let src = r#"
fn external_equal(a: any, b: any) -> bool { return a == b }
fn via_helper(xs: [int], a: any, b: any) -> int {
    if xs.len() <= 0 { return -1 }
    let equal = external_equal(a, b)
    if equal { return xs[0] }
    return -2
}
"#;
    let out = run(
        src,
        r#"(function()
            __op_eq = function(a, b) return a == b end
            local xs = { len = 1, _data = { 7 } }
            local mt = { __eq = function()
                xs.len = 0
                xs._data[1] = nil
                return true
            end }
            local a, b = setmetatable({}, mt), setmetatable({}, mt)
            local ok, err = pcall(via_helper, xs, a, b)
            assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), tostring(err))
            return "ok"
        end)()"#,
    );
    assert_eq!(out, "ok");
}

#[test]
fn vec_loop_shapes_compute_the_same() {
    let src = "
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
fn probe() -> string {
    let a = sum_p([P { x: 1.5, y: 2.0 }, P { x: 3.0, y: 4.0 }])
    let b = skip_odd([1, 2, 3, 4, 200, 6])
    let c = total_len([\"ab\", \"\", \"cde\"])
    return \"{a} {b} {c} {find([4, 5], 5)} {find([], 5)}\"
}";
    assert_eq!(run(src, "probe()"), "15 6 5 50 -1");
}

// Callee binding binds direct calls, so these run with inlining off to keep
// the calls the binding is about.

#[test]
fn callee_binding_ignores_calls_removed_by_cleanup() {
    let _inline_off = common::InlineOffGuard::on();
    let src = "fn discarded(a: int) -> int { a + 2 }\nfn live(a: int) -> int { a + 1 }\nfn caller(a: int) -> int { if false { discarded(a) } else { live(a) } }";
    assert_eq!(run(src, "caller(7)"), "8");
}

#[test]
fn callee_binding_ignores_discarded_uint_helpers() {
    let _inline_off = common::InlineOffGuard::on();
    let src = "fn checked(value: float, count: uint) -> uint { if false { (value as uint) << count } else { count } }";
    assert_eq!(run(src, "checked(-1, 3)"), "3");
}

#[test]
fn callee_binding_short_names_skip_public_globals() {
    let _inline_off = common::InlineOffGuard::on();
    let src = "fn leaf(a: int) -> int { a + 1 }\n\
               @nomangle fn b0(a: int) -> int { a + 2 }\n\
               fn caller(a: int) -> int { leaf(a) + b0(a) }";
    assert_eq!(run(src, "caller(3)"), "9");
}

/// A factory alias never takes a name that spliced inline(lua) text reads
/// freely: the alias would capture the text's global.
#[test]
fn callee_binding_aliases_skip_names_inline_lua_reads() {
    let _inline_off = common::InlineOffGuard::on();
    let src = "fn leaf(a: int) -> int { a + 1 }\n\
               @inline inline(lua) fn raw(a: int) -> int = \"return b0(a)\"\n\
               fn caller(a: int) -> int { leaf(a) + raw(a) }";
    assert_eq!(
        run(
            src,
            "(function() b0 = function(a) return a + 10 end; return caller(3) end)()"
        ),
        "17"
    );
}

/// Aliased captures called around a spliced inline(lua) body inside a loop,
/// where the splice declares a local spelled like an alias, run the same as
/// the source says.
#[test]
fn aliased_captures_around_a_spliced_body_in_a_loop_run_correctly() {
    let _inline_off = common::InlineOffGuard::on();
    let src = "fn leaf(a: int) -> int { a + 1 }\n\
               fn other(a: int) -> int { a * 3 }\n\
               @inline inline(lua) fn twice(a: int) -> int = \"local b0 = a * 2\\nreturn b0\"\n\
               fn caller(n: int) -> int {\n\
                 let s = 0\n\
                 for i in 0..n { s = s + leaf(twice(i)) + other(i) }\n\
                 return s\n\
               }";
    // sum over i < 4 of (2i + 1) + 3i
    assert_eq!(run(src, "caller(4)"), "34");
}

/// LuaJIT refuses to parse a function with more upvalues than
/// `MAX_BOUND_CALLEES`, so a body with more bindable callees must still load.
#[test]
fn callee_binding_caps_at_the_lua_upvalue_limit() {
    let _inline_off = common::InlineOffGuard::on();
    let count = skull_codegen_lua::MAX_BOUND_CALLEES + 7;
    let mut src = String::new();
    for i in 0..count {
        src.push_str(&format!("fn leaf{i:03}(a: int) -> int {{ a + {i} }}\n"));
    }
    src.push_str("fn caller(a: int) -> int {\n  let t = 0\n");
    for i in 0..count {
        src.push_str(&format!("  t = t + leaf{i:03}(a)\n"));
    }
    src.push_str("  t\n}\n");
    let expected: i64 = (0..count as i64).map(|i| 7 + i).sum();
    assert_eq!(run(&src, "caller(7)"), expected.to_string());
}

#[test]
fn reused_int_to_float_aliases_compute_the_same() {
    let param = "fn f(x: int) -> float { let y = x as float; y * y + y }";
    assert_eq!(run(param, "f(3)"), "12");
    let register = "fn g(a: int, b: int) -> float { let s = a * b; let y = s as float; y * y + y + (s + 1) as float }";
    assert_eq!(run(register, "g(2, 3)"), "49");
}

#[test]
fn vec_len_reads_the_length() {
    let src = "fn n(xs: [int], ys: [string]) -> int { return xs.len() + vec_len(ys) }";
    assert_eq!(
        run(
            src,
            "n({_data = {1, 2}, len = 2}, {_data = {'a'}, len = 1})"
        ),
        "3"
    );
}

#[test]
fn vec_push_method_appends() {
    let src = "fn add(xs: [int], x: int) -> int { xs.push(x); xs.push(x + 1); return xs.len() }";
    assert_eq!(run(src, "add({_data = {}, len = 0}, 7)"), "2");
}

#[test]
fn table_lookups_can_shrink_a_guarded_vector() {
    let src = r#"
fn via_table(xs: [int], t: any) -> int {
    if xs.len() <= 0 { return -1 }
    let value: int = cast(table_get(t, "missing"))
    return xs[0] + value
}
fn via_vec_data(xs: [int], ys: [int?]) -> int {
    if xs.len() <= 0 { return -1 }
    let value: int = cast(__vec_data_get(ys, 0))
    return xs[0] + value
}
"#;
    let out = run(
        src,
        r#"(function()
            local function check(f, t)
                local xs = { len = 1, _data = { 7 } }
                local mt = { __index = function()
                    xs.len = 0
                    xs._data[1] = nil
                    return 1
                end }
                local ok, err = pcall(f, xs, t(mt))
                assert(not ok and tostring(err):find("Vec index out of bounds", 1, true), tostring(err))
            end
            check(via_table, function(mt) return setmetatable({}, mt) end)
            check(via_vec_data, function(mt) return { len = 1, _data = setmetatable({}, mt) } end)
            return "ok"
        end)()"#,
    );
    assert_eq!(out, "ok");
}
