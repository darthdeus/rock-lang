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

#[test]
fn vec_of_value_struct_uses_columns_for_push_and_read() {
    let lua = emit_module(VEC_VALUE_LOOP);
    let body = function_body(&lua, "fill");
    assert!(!body.contains("V2_ct("), "{body}");
    assert!(!body.contains("_data"), "{body}");
    assert!(
        body.contains(r#"["x"] = {}"#) && body.contains(r#"["y"] = {}"#),
        "{body}"
    );
    assert!(body.contains(".x") && body.contains(".y"), "{body}");
    assert!(!body.contains("__vec_checked_get("), "{body}");
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

const VEC_IN_SPECIALIZED_LAMBDA: &str = r#"
value V2 { x: float, y: float }

fn go<T>(_tag: T) -> float {
    let xs: [V2] = [V2 { x: 1.0, y: 2.0 }, V2 { x: 3.0, y: 4.0 }]
    let f = () => { let a = 0.0; for i in 0..xs.len { a = a + xs[i].x }; return a }
    let first = f()
    return f() + first - first
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

#[test]
fn column_vec_iteration_works_in_separately_emitted_contexts() {
    for (name, source, selector, required, forbidden) in [
        (
            "specialized lambda",
            VEC_IN_SPECIALIZED_LAMBDA,
            "__lambda",
            &[".x)[(c"] as &[_],
            &["__vec_checked_index"] as &[_],
        ),
        (
            "co fn",
            VEC_IN_CO_FN,
            "__step",
            &[] as &[_],
            &["next_VecIter_T("] as &[_],
        ),
    ] {
        let lua = emit_module(source);
        let body = crate::common::body_of_synthetic(&lua, selector);
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

const VEC_NEW_COLUMN: &str = r#"
value V2 { x: float, y: float }

fn main() {
    let a: [V2] = vec_new()
    a.push(V2 { x: 1.0, y: 2.0 })
    print("{a.len},{a[0].x},{a[0].y}")
}
"#;

#[test]
fn vec_new_at_a_column_element_builds_empty_columns() {
    let lua = crate::common::emit_module(VEC_NEW_COLUMN);
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
