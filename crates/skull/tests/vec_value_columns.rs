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
