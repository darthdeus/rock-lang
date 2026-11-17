//! Check Lua lowering and runtime behavior for vectors backed by leaf columns.

use crate::common::{emit_module, function_body};

const VEC_VALUE_LOOP: &str = r#"
value V2 { x: float, y: float }

fn fill(n: int) -> float {
    let xs: [V2] = []
    for i in 0..n {
        xs.push(V2 { x: 1.0, y: 2.0 })
    }
    let acc = 0.0
    for i in 0..xs.len {
        acc = acc + xs[i].x
    }
    return acc
}

fn main() { let _ = fill(8) }
"#;

/// The headline properties: pushes and reads stay in leaf columns without
/// constructing or fetching whole elements.
#[test]
fn vec_of_value_struct_uses_columns_for_push_and_read() {
    let lua = emit_module(VEC_VALUE_LOOP);
    let body = function_body(&lua, "fill");
    assert!(
        !body.contains("V2_ct("),
        "[V2] push boxed the element (`V2_ct(..)`); source:\n{VEC_VALUE_LOOP}\nemitted body:\n{body}"
    );
    assert!(
        !body.contains("_data"),
        "[V2] push still wrote the single `_data` backing; source:\n{VEC_VALUE_LOOP}\nemitted body:\n{body}"
    );
    // The literal builds one array per leaf. Regalloc splits the store itself
    // into `rN = vec.x` / `rN[i] = ..`, so assert the CONSTRUCTION shape (which
    // is stable) rather than a fused `vec.x[i]` text that never appears.
    assert!(
        body.contains(r#"["x"] = {}"#) && body.contains(r#"["y"] = {}"#),
        "[V2] literal did not build per-leaf column arrays; source:\n{VEC_VALUE_LOOP}\nemitted body:\n{body}"
    );
    assert!(
        body.contains(".x") && body.contains(".y"),
        "[V2] push did not reach the leaf columns; source:\n{VEC_VALUE_LOOP}\nemitted body:\n{body}"
    );
    assert!(
        !body.contains("__vec_checked_get("),
        "`xs[i].x` still fetched the whole element; source:\n{VEC_VALUE_LOOP}\nemitted body:\n{body}"
    );
}

const VEC_SPREAD_LITERAL: &str = r#"
value V2 { x: float, y: float }

fn make() -> [V2] {
    return [...(
        V2 { x: 1.0, y: 2.0 },
        V2 { x: 3.0, y: 4.0 },
    )]
}

fn main() { let _ = make() }
"#;

#[test]
fn tuple_spread_literal_never_boxes_column_elements() {
    let lua = emit_module(VEC_SPREAD_LITERAL);
    let body = function_body(&lua, "make");
    assert!(
        !body.contains("V2_ct("),
        "tuple spread built temporary V2 elements before storing their leaves:\n{body}"
    );
    assert!(
        crate::common::has_vec_header(body, "columns", Some("V2")),
        "tuple spread did not produce the marked column representation:\n{body}"
    );
}

const VEC_ELEM_BIND: &str = r#"
value V2 { x: float, y: float }

fn sum(xs: [V2]) -> float {
    let acc = 0.0
    for i in 0..xs.len {
        let v = xs[i]
        acc = acc + v.x + v.y
    }
    return acc
}

fn main() { let _: [V2] = []; let _ = sum([]) }
"#;

/// Binding a whole element (`let v = xs[i]`) is legal and must stay flat: each
/// leaf loads from its column and SROA peels the binding.
///
/// NOTE the assertion is on the FETCH, not on `V2_ct(`. A no-ctor assertion is
/// vacuously green today -- `__vec_checked_get` hands back the element table
/// that storage already boxed, so no *new* ctor is needed and the test would
/// pass while the property is false. Asserting the whole-element fetch is gone
/// is what actually distinguishes flat from boxed.
#[test]
fn binding_a_whole_element_loads_leaves_not_an_element() {
    let lua = emit_module(VEC_ELEM_BIND);
    let body = function_body(&lua, "sum");
    assert!(
        !body.contains("__vec_checked_get("),
        "`let v = xs[i]` fetched a whole element table; under columns the bind \
         loads one slot per leaf:\n{body}"
    );
    assert!(
        !body.contains("V2_ct("),
        "`let v = xs[i]` rematerialized the element:\n{body}"
    );
}

// These contexts emit separate callable bodies. Check the selected body because
// the shared VecIter template remains in the module either way.

const VEC_IN_LAMBDA: &str = r#"
value V2 { x: float, y: float }

fn go() -> float {
    let xs: [V2] = [V2 { x: 1.0, y: 2.0 }, V2 { x: 3.0, y: 4.0 }]
    let f = () => { let a = 0.0; for v in xs { a = a + v.x }; return a }
    return f()
}

fn main() { print("{go()}") }
"#;

const VEC_IN_NESTED_FN: &str = r#"
value V2 { x: float, y: float }

fn go() -> float {
    let xs: [V2] = [V2 { x: 1.0, y: 0.0 }, V2 { x: 3.0, y: 0.0 }]
    fn inner(ys: [V2]) -> float { let a = 0.0; for v in ys { a = a + v.x }; return a }
    return inner(xs)
}

fn main() { print("{go()}") }
"#;

const VEC_IN_SPECIALIZED_LAMBDA: &str = r#"
value V2 { x: float, y: float }

fn go<T>(_tag: T) -> float {
    let xs: [V2] = [V2 { x: 1.0, y: 2.0 }, V2 { x: 3.0, y: 4.0 }]
    let f = () => { let a = 0.0; for i in 0..xs.len { a = a + xs[i].x }; return a }
    return f()
}

fn main() { print("{go(1)}") }
"#;

const VEC_IN_CO_FN: &str = r#"
value V2 { x: float, y: float }

co fn walk(xs: [V2]) -> Coroutine<float> {
    for v in xs {
        yield v.x
    }
}

fn go() -> float {
    let xs: [V2] = [V2 { x: 1.0, y: 0.0 }, V2 { x: 3.0, y: 0.0 }]
    let c = walk(xs)
    let a = c.resume() ?? 0.0
    let b = c.resume() ?? 0.0
    return a + b
}

fn main() { print("{go()}") }
"#;

const VEC_IN_GLOBAL_INIT: &str = r#"
value V2 { x: float, y: float }

global TOTAL: float = {
    let xs: [V2] = [V2 { x: 1.0, y: 0.0 }, V2 { x: 3.0, y: 0.0 }]
    let a = 0.0
    for v in xs { a = a + v.x }
    a
}

fn main() { print("{TOTAL}") }
"#;

#[test]
fn column_vec_iteration_works_in_separately_emitted_contexts() {
    for (name, source, selector, required, forbidden) in [
        (
            "lambda",
            VEC_IN_LAMBDA,
            "__lambda",
            &[".x)[(c"] as &[_],
            &["_data", "__vec_checked_get"] as &[_],
        ),
        (
            "nested fn",
            VEC_IN_NESTED_FN,
            "_inner_",
            &[".x)[(c"],
            &["next_VecIter_T(", "__vec_checked_get"],
        ),
        (
            "specialized lambda",
            VEC_IN_SPECIALIZED_LAMBDA,
            "__lambda",
            &[".x)[(c"],
            &["__vec_checked_index"],
        ),
        ("co fn", VEC_IN_CO_FN, "__step", &[], &["next_VecIter_T("]),
        (
            "global initializer",
            VEC_IN_GLOBAL_INIT,
            "ginit_TOTAL",
            &[],
            &["next_VecIter_T("],
        ),
    ] {
        let lua = emit_module(source);
        let body = crate::common::body_of_synthetic(&lua, selector);
        // A positive column read distinguishes flat storage from a boxed loop
        // that also avoids the unspecialized VecIter template.
        for needle in required {
            assert!(
                body.contains(needle),
                "{name} body is missing `{needle}`:\n{body}"
            );
        }
        for needle in forbidden {
            assert!(
                !body.contains(needle),
                "{name} body still contains `{needle}`:\n{body}"
            );
        }
        assert_eq!(crate::common::run_main(&lua), "4", "{name}");
    }
}

const VEC_STDLIB_SURFACE: &str = r#"
value V2 { x: float, y: float }

fn main() {
    let xs: [V2] = [V2 { x: 1.0, y: 10.0 }, V2 { x: 2.0, y: 20.0 }, V2 { x: 3.0, y: 30.0 }]
    xs.set(0, V2 { x: 9.0, y: 90.0 })
    print("set:{xs[0].x},{xs[0].y}")
    let g = xs.get(1)
    if g != null { print("get:{g.x},{g.y}") }
    let p = xs.pop()
    if p != null { print("pop:{p.x} len{xs.len}") }
    let r = xs.swap_remove(0)
    if r != null { print("swap:{r.x} now{xs[0].x} len{xs.len}") }
    xs.clear()
    print("clear len{xs.len}")
    xs.push(V2 { x: 7.0, y: 70.0 })
    print("repush:{xs[0].x},{xs[0].y} len{xs.len}")
}
"#;

/// Call through the ordinary Vec surface so specialization and lowering both run.
#[test]
fn stdlib_vec_surface_works_over_column_storage() {
    let lua =
        crate::common::try_emit_module(VEC_STDLIB_SURFACE).expect("stdlib Vec surface must emit");
    // The values below are correct under BOXED storage too, so pin the mechanism
    // this test is named for: each plain-generic entry point reaches a standalone
    // The encoded `a_n2_V2_0_z` key is the parameterless nominal `V2`.
    for callee in [
        "vec_set_arr_T_int_T_spec_a_n2_V2_0_z(",
        "pop_arr_T_spec_a_n2_V2_0_z(",
        "clear_arr_T_spec_a_n2_V2_0_z(",
    ] {
        assert!(
            lua.contains(callee),
            "the stdlib Vec surface must call the specialized `{callee}` over column storage"
        );
    }
    assert_eq!(
        crate::common::run_main(&lua),
        "set:9,90\nget:2,20\npop:3 len2\nswap:9 now2 len1\nclear len0\nrepush:7,70 len1"
    );
}

const VEC_BOXED_ELEMS_UNSPLICED: &str = r#"
value WithRef { name: string, k: int }
value Wide { a: int, b: int, c: int, d: int, e: int }

co fn walk_ref(xs: [WithRef]) -> Coroutine<int> {
    for v in xs { yield v.k }
}

fn nested_wide(xs: [Wide]) -> int {
    fn inner(ys: [Wide]) -> int { let a = 0; for v in ys { a = a + v.a }; return a }
    return inner(xs)
}

fn main() {
    let c = walk_ref([WithRef { name: "a", k: 1 }])
    let one = c.resume() ?? 0
    print("{one + nested_wide([Wide { a: 3, b: 0, c: 0, d: 0, e: 0 }])}")
}
"#;

/// The other side of the rejection: it must fire ONLY where the representation
/// differs. `WithRef` has a reference leaf and `Wide` exceeds
/// `MAX_COLUMN_LEAVES`, so both keep the boxed `_data` -- for them the shared
/// generic `next` is CORRECT and an unspliceable context simply forgoes the
/// scalar ABI, exactly as an ordinary call does. Rejecting here would bounce
/// working code, which is what the first cut of the validator did.
#[test]
fn unspliceable_iteration_of_boxed_elements_still_emits() {
    let lua = crate::common::try_emit_module(VEC_BOXED_ELEMS_UNSPLICED)
        .expect("boxed element types must still emit in unspliceable contexts");
    assert_eq!(crate::common::run_main(&lua), "4");
}

const VEC_CONTROLS: &str = r#"
value WithRef { name: string, k: int }
struct Plain { a: int }

fn ints() -> int {
    let xs: [int] = []
    xs.push(3)
    return xs[0]
}

fn refs() -> int {
    let xs: [WithRef] = []
    xs.push(WithRef { name: "a", k: 1 })
    return xs[0].k
}

fn plains() -> int {
    let xs: [Plain] = []
    xs.push(Plain { a: 1 })
    return xs[0].a
}

fn main() { let _ = ints() + refs() + plains() }
"#;

/// Controls: the representation swap is NARROW. A primitive element is already
/// one flat array, a value struct with a REFERENCE leaf is out of scope, and a
/// non-value struct keeps identity. All three keep `_data`.
#[test]
fn ineligible_element_types_keep_todays_representation() {
    let lua = emit_module(VEC_CONTROLS);
    for f in ["ints", "refs", "plains"] {
        let body = function_body(&lua, f);
        assert!(
            body.contains("_data"),
            "{f}: ineligible element type lost the `_data` backing -- the \
             column swap must not reach it:\n{body}"
        );
    }
}

const VEC_NEW_COLUMN: &str = r#"
value V2 { x: float, y: float }

fn main() {
    let a: [V2] = vec_new()
    a.push(V2 { x: 1.0, y: 2.0 })
    print("{a.len},{a[0].x},{a[0].y}")
}
"#;

/// `vec_new()` is the fourth representation-boundary primitive and the odd one
/// out: its element type comes from the CALL's own type, not from a receiver, so
/// the container plan cannot read it off an argument. The concrete boundary
/// plan must therefore construct the columns before the next column write.
#[test]
fn vec_new_at_a_column_element_builds_empty_columns() {
    let lua = crate::common::try_emit_module(VEC_NEW_COLUMN).expect("vec_new at [V2] must emit");
    let body = crate::common::function_body(&lua, "main");
    assert!(
        !body.contains("vec_new()"),
        "vec_new at a column element must not call the boxed stdlib body:\n{body}"
    );
    assert!(
        body.contains("[\"x\"] = {}") && body.contains("[\"y\"] = {}"),
        "vec_new at [V2] must build one empty array per leaf:\n{body}"
    );
    assert_eq!(
        crate::common::run_main(&lua),
        "1,1,2",
        "push onto a vec_new-ed column Vec must land in the columns"
    );
}

const VEC_COMBINATOR_CHAIN: &str = r#"
value V2 { x: float, y: float }

fn main() {
    let a: [V2] = [V2 { x: 1.0, y: 2.0 }, V2 { x: 3.0, y: 4.0 }]
    print("{a.map(_.x).sum_f()}")
}
"#;

#[test]
fn combinator_chain_preserves_column_element_type() {
    let lua = crate::common::try_emit_module(VEC_COMBINATOR_CHAIN).expect("chain must emit");
    let body = crate::common::function_body(&lua, "main");
    assert!(
        !body.contains("__vec_checked_get(") && body.contains("__vec_checked_index("),
        "the wrapped VecIter<V2> must retain V2 and read its columns:\n{body}"
    );
    assert_eq!(crate::common::run_main(&lua), "4");
}

const NOBOX_COLUMN_SORT: &str = r#"
nobox value NP { x: float, y: float }

fn main() {
    let a: [NP] = [NP { x: 3.0, y: 30.0 }, NP { x: 1.0, y: 10.0 }, NP { x: 2.0, y: 20.0 }]
    a.sort(v => v.x)
    print("{a[0].x},{a[0].y},{a[1].x},{a[1].y},{a[2].x},{a[2].y}")
}
"#;

/// Sorting a column Vec needs the key applied to each ELEMENT, which the boxed
/// helper supplies by materializing one. Under `nobox` that box is exactly what
/// the mark forbids -- but the key fn is reached through a fn value, whose ABI
/// is derived from the fn type, so at a `nobox` element it already takes LEAVES.
/// The leaves are the columns, so the sort hands them over directly and no
/// element is ever built.
#[test]
fn nobox_column_sort_never_materializes_an_element() {
    let lua = crate::common::try_emit_module(NOBOX_COLUMN_SORT).expect("[NP].sort must emit");
    let body = crate::common::function_body(&lua, "main");
    assert!(
        !body.contains("NP_ct(") && !body.contains("NP_lct("),
        "[NP].sort built an element; the key fn takes leaves:\n{body}"
    );
    // Runtime sorting and the zero-materialization count are covered by
    // `tests/vectors/column_stdlib_surface.rock`.
}

#[test]
fn nobox_reference_elements_use_columns() {
    let lua = emit_module(
        r#"
nobox value ReferenceRow { payload: [int]?, position: int }
fn fill_reference_rows(payload: [int]) -> int {
    let rows: [ReferenceRow] = []
    rows.push(ReferenceRow { payload, position: 7 })
    return rows[0].position
}
fn main() { print(fill_reference_rows([3])) }
"#,
    );
    let body = function_body(&lua, "fill_reference_rows");
    assert!(body.contains(r#"["payload"] = {}"#), "{body}");
    assert!(body.contains(r#"["position"] = {}"#), "{body}");
    assert!(!body.contains("ReferenceRow_ct("), "{body}");
}

/// Two leaves of one element in one expression share the element's bounds
/// check, on both flat layouts, and load each column they use only once.
#[test]
fn two_leaves_of_one_element_check_once() {
    for (decl, layout, init) in [
        (
            "value P { a: float, b: float }",
            "columns",
            "P { a: 1.0, b: 2.0 }",
        ),
        (
            "nobox value P { a: float, b: float, c: float, d: float, e: float, f: float }",
            "strided",
            "P { a: 1.0, b: 2.0, c: 0.0, d: 0.0, e: 0.0, f: 0.0 }",
        ),
    ] {
        let src = format!(
            "{decl}\n\
             fn sum(xs: [P], i: int) -> float {{\n    return xs[i].a + xs[i].b\n}}\n\
             fn main() {{\n    let xs: [P] = [{init}]\n    print(sum(xs, 0))\n}}\n"
        );
        let lua = emit_module(&src);
        assert!(crate::common::has_vec_header(&lua, layout, None), "{lua}");
        let body = function_body(&lua, "sum");
        assert_eq!(
            body.matches("__vec_checked_index(").count(),
            1,
            "{layout}: `xs[i].a + xs[i].b` checked the element twice:\n{body}"
        );
        if layout == "columns" {
            for col in [".a", ".b"] {
                let loads = body.matches(&format!("{col})")).count()
                    + body.lines().filter(|l| l.ends_with(col)).count();
                assert_eq!(
                    loads, 1,
                    "{layout}: column `{col}` loaded {loads}x:\n{body}"
                );
            }
        }
    }
}

const EMPTY_FLAT_VECS: &str = r#"
value V2 { x: float, y: float }
nobox value Wide { a: float, b: float, c: float, d: float, e: float, f: float }

@nomangle fn empty_columns() -> [V2] { return [] }
@nomangle fn empty_strided() -> [Wide] { return [] }
@nomangle fn reserved_strided() -> [Wide] { return vec_new_with_capacity(4) }
@nomangle fn fill(xs: [V2], ws: [Wide]) -> float {
    xs.push(V2 { x: 1.0, y: 2.0 })
    ws.push(Wide { a: 1.0, b: 2.0, c: 3.0, d: 4.0, e: 5.0, f: 6.0 })
    return xs[0].y + ws[0].f + xs.len + ws.len
}
"#;

/// An empty flat Vec with no reserved capacity never writes its header, so
/// every such Vec of one element type shares one header table.
#[test]
fn empty_flat_vecs_share_their_header() {
    let lua = emit_module(EMPTY_FLAT_VECS);
    for name in ["empty_columns", "empty_strided"] {
        let body = function_body(&lua, name);
        assert!(!body.contains("cap = (0)"), "{body}");
    }
    let driver = format!(
        r#"{lua}
local a, b = empty_columns(), empty_columns()
assert(rawget(a, "@vec") == rawget(b, "@vec"))
local s, t = empty_strided(), empty_strided()
assert(rawget(s, "@vec") == rawget(t, "@vec"))
assert(rawget(s, "@vec").repr == "strided" and rawget(a, "@vec").repr == "columns")
local r = reserved_strided()
assert(rawget(r, "@vec") ~= rawget(s, "@vec"))
assert(fill(a, s) == 10 and fill(b, r) == 10 and fill(a, t) == 11)
assert(rawget(s, "@vec").cap == 0 and not rawget(s, "@vec").reserved)
print("ok")
"#
    );
    assert_eq!(crate::common::run_luajit(&driver), "ok");
}

#[test]
fn flat_vec_bounds_checks_call_helpers_only_on_a_miss() {
    for (decl, fields, layout) in [
        ("value P { a: int, b: int }", "a: n, b: 2", "columns"),
        (
            "nobox value P { a: int, b: int, c: int, d: int, e: int }",
            "a: n, b: 2, c: 3, d: 4, e: 5",
            "strided",
        ),
    ] {
        let append_fields = fields.replace("a: n", "a: 7");
        let src = format!(
            r#"{decl}
@nomangle fn make(n: int) -> [P] {{ return [P {{ {fields} }}] }}
@nomangle fn read(xs: [P], i: int) -> int {{ return xs[i].a }}
@nomangle fn store(xs: [P], i: int, n: int) {{ xs[i] = P {{ {fields} }} }}
@nomangle fn walk(xs: [P], mode: int) -> int {{
    let sum = 0
    for x in xs {{
        sum += x.a
        if mode == 0 {{ xs.push(P {{ {append_fields} }}) }}
        if mode == 1 {{ xs.pop() }}
        if mode == 2 {{ xs.clear() }}
        if mode == 3 {{ xs.remove_at(0) }}
        if mode == 4 {{ xs[1].a = 9 }}
        if mode == 5 {{ xs = make(99) }}
    }}
    return sum
}}
fn main() {{}}
"#,
        );
        let lua = emit_module(&src);
        assert!(crate::common::has_vec_header(&lua, layout, None), "{lua}");
        let driver = format!(
            r#"{lua}
local checks = 0
local checked = __vec_checked_index
__vec_checked_index = function(v, i) checks = checks + 1; return checked(v, i) end
local function oob(f, i, n)
  local ok, err = pcall(f)
  assert(not ok and tostring(err):find("Vec index out of bounds: index " .. i .. " is outside [0, " .. n .. ")", 1, true), tostring(err))
end
local xs = make(1)
assert(read(xs, 0) == 1)
store(xs, 0, 8)
assert(read(xs, 0) == 8)
assert(checks == 0, "in-bounds flat accesses called the helper " .. checks .. " times")
oob(function() read(xs, -1) end, -1, 1)
oob(function() read(xs, 1) end, 1, 1)
oob(function() store(xs, -1, 9) end, -1, 1)
oob(function() store(xs, 1, 9) end, 1, 1)
assert(read(xs, 0) == 8 and xs.len == 1)
-- The live length remains authoritative even if physical storage is stale.
xs.len = 0
oob(function() read(xs, 0) end, 0, 0)
oob(function() store(xs, 0, 9) end, 0, 0)
for mode = 0, 5 do
  xs = make(1)
  store(xs, 0, 2)
  -- Append a second element without depending on the storage layout.
  assert(walk(xs, 0) == 2 and xs.len == 2)
  if mode >= 1 and mode <= 3 then
    oob(function() walk(xs, mode) end, 1, mode == 2 and 0 or 1)
  else
    assert(walk(xs, mode) == (mode == 4 and 11 or 9))
  end
end
print("ok")
"#
        );
        assert_eq!(crate::common::run_luajit(&driver), "ok", "{layout}");
    }
}

#[test]
fn column_vec_false_and_null_leaves_are_in_bounds() {
    let lua = emit_module(
        r#"
nobox value P { flag: bool, payload: [int]? }
@nomangle fn make() -> [P] { return [P { flag: false, payload: null }, P { flag: true, payload: [7] }] }
@nomangle fn flag(xs: [P], i: int) -> bool { return xs[i].flag }
@nomangle fn payload(xs: [P], i: int) -> [int]? { return xs[i].payload }
@nomangle fn reset(xs: [P], i: int) { xs[i] = P { flag: false, payload: null } }
fn main() {}
"#,
    );
    let driver = format!(
        r#"{lua}
local xs = make()
assert(flag(xs, 0) == false and payload(xs, 0) == nil)
assert(flag(xs, 1) == true and payload(xs, 1)._data[1] == 7)
reset(xs, 1)
assert(flag(xs, 1) == false and payload(xs, 1) == nil)
print("ok")
"#
    );
    assert_eq!(crate::common::run_luajit(&driver), "ok");
}

#[test]
fn plain_vec_stores_preserve_holes_bounds_and_argument_evaluation() {
    let lua = emit_module(
        r#"
@nomangle fn store(xs: [int?], i: int, n: int?) { xs[i] = n }
@nomangle fn grow(xs: [int?]) -> int? { xs.push(7); return 9 }
@nomangle fn growing_store(xs: [int?]) { xs[1] = grow(xs) }
fn main() {}
"#,
    );
    let driver = format!(
        r#"{lua}
local xs = {{len = 2, _data = {{false, 3}}}}
store(xs, 0, nil)
store(xs, 1, 8)
assert(xs._data[1] == nil and xs._data[2] == 8)
for _, i in ipairs({{-1, 2}}) do
  local ok, err = pcall(store, xs, i, 9)
  assert(not ok and tostring(err):find("Vec index out of bounds", 1, true))
end
xs = {{len = 1, _data = {{1}}}}
growing_store(xs)
assert(xs.len == 2 and xs._data[2] == 9)
print("ok")
"#
    );
    assert_eq!(crate::common::run_luajit(&driver), "ok");
}
