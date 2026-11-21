use crate::common::{emit_module, function_body, run_fn};

const VALUE_NESTED_ENUM_FIELDS: &str = r#"
enum Left { A, B }
enum Right { C, D }
enum Kind { Plain, Left(Left), Right(Right) }

value Row {
    kind: Kind,
    key: string,
    count: int,
}

fn make_row(kind: Kind, n: int) -> Row {
    return match kind {
        Kind::Plain => Row { kind, key: "plain", count: n + 1 },
        Kind::Left(_) => Row { kind, key: "left", count: n + 1 },
        Kind::Right(_) => Row { kind, key: "right", count: n + 1 },
    }
}

fn consume_row(row: Row) -> int {
    return match row.kind {
        Kind::Plain => row.count,
        Kind::Left(value) => match value {
            Left::A => row.count,
            Left::B => row.count + 4,
        },
        Kind::Right(_) => row.count,
    }
}

fn route_row(n: int) -> int { return consume_row(make_row(Kind::Left(Left::B), n)) }

fn store_row(n: int) -> int {
    let rows: [Row] = []
    rows.push(make_row(Kind::Left(Left::B), n))
    return consume_row(rows[0])
}

@test(output = "9 9")
fn t() { print("{route_row(4)} {store_row(4)}") }

fn main() { t() }
"#;

#[test]
fn nested_flat_enums_cross_an_ordinary_value_wire_without_boxes() {
    let lua = emit_module(VALUE_NESTED_ENUM_FIELDS);
    assert_eq!(run_fn(&lua, "t"), "9 9");
    let make = function_body(&lua, "make_row");
    assert!(!make.contains("{__variant = "));
    let route = function_body(&lua, "route_row");
    assert!(!route.contains("{__variant = "));
}
