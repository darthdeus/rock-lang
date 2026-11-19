use super::*;

#[test]
fn hover_basic_types() {
    let source = r#"fn main() {
    let number = 42
    let text = "hello"
    let flag = true
    let array = [1, 2, 3]
    let point = Point { x: 1.0, y: 2.0 }
}

struct Point {
    x: float,
    y: float,
}"#;
    let project = Project::single("hover_basic_types", source);
    let reply = project.cli("hover", Some((1, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_function_types() {
    let source = r#"fn add(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let result = add(5, 3)
}"#;
    let project = Project::single("hover_function_types", source);
    let reply = project.cli("hover", Some((0, 3)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        r#"```rock
fn add(a: int, b: int) -> int
```"#
            .trim()
    );
}

#[test]
fn hover_struct_types() {
    let source = r#"struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
}"#;
    let project = Project::single("hover_struct_types", source);
    let reply = project.cli("hover", Some((0, 7)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        r#"struct Point {
  x: float,
  y: float,
}"#
        .trim()
    );
}

#[test]
fn hover_generic_types() {
    let source = r#"fn identity<T>(value: T) -> T {
    return value
}

fn main() {
    let num = identity(42)
    let text = identity("hello")
}"#;
    let project = Project::single("hover_generic_types", source);
    let reply = project.cli("hover", Some((5, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_nullable_types() {
    let source = r#"fn find_user(id: int) -> string? {
    if id == 1 {
        return "Alice"
    } else {
        return null
    }
}

fn main() {
    let user = find_user(1)
}"#;
    let project = Project::single("hover_nullable_types", source);
    let reply = project.cli("hover", Some((9, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: string?".trim()
    );
}

#[test]
fn hover_array_types() {
    let source = r#"fn main() {
    let numbers = [1, 2, 3, 4, 5]
    let texts = ["hello", "world"]
    let matrix = [[1, 2], [3, 4]]
}"#;
    let project = Project::single("hover_array_types", source);
    let reply = project.cli("hover", Some((1, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: [int]".trim()
    );
}

const HOVER_SHORTHAND_X: &str = r#"struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    // Shorthand syntax - hover on 'x' field
    let p = Point { x, y }
}"#;

#[test]
fn hover_shorthand_x() {
    let source = HOVER_SHORTHAND_X;
    let project = Project::single("hover_shorthand_x", source);
    let reply = project.cli("hover", Some((10, 20)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_shorthand_y() {
    let source = &HOVER_SHORTHAND_X.replace(
        "    // Shorthand syntax - hover on 'x' field",
        "    // Shorthand syntax - hover on 'y' field",
    );
    let project = Project::single("hover_shorthand_y", source);
    let reply = project.cli("hover", Some((10, 23)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_shorthand_nested() {
    let source = r#"struct Point {
    x: int
    y: int
}

struct Rectangle {
    top_left: Point
    width: int
    height: int
}

fn main() {
    let x = 100
    let y = 200
    let width = 50
    let height = 75

    let p = Point { x, y }
    let top_left = p

    // Nested shorthand - hover on 'top_left' field
    let rect = Rectangle { top_left, width, height }
}"#;
    let project = Project::single("hover_shorthand_nested", source);
    let reply = project.cli("hover", Some((21, 27)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: struct Point".trim()
    );
}

#[test]
fn hover_mixed_shorthand() {
    let source = r#"struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100

    // Mixed shorthand and explicit - hover on 'x' field
    let p = Point { x, y: 200 }
}"#;
    let project = Project::single("hover_mixed_shorthand", source);
    let reply = project.cli("hover", Some((9, 20)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_for_loop_variable() {
    let source = r#"struct Item {
    name: string
}

fn main() {
    let items: [Item] = vec_new()
    for item in items {
        print(item.name)
    }
}"#;
    let project = Project::single("hover_for_loop_variable", source);
    let reply = project.cli("hover", Some((6, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: struct Item".trim()
    );
}

#[test]
fn hover_lambda_type() {
    let source = r#"fn main() {
    let f = (x: int) => x * 2
}"#;
    let project = Project::single("hover_lambda_type", source);
    let reply = project.cli("hover", Some((1, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: fn(int) -> int".trim()
    );
}

#[test]
fn hover_nullable_type() {
    let source = r#"fn main() {
    let x: int? = null
}"#;
    let project = Project::single("hover_nullable_type", source);
    let reply = project.cli("hover", Some((1, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int?".trim()
    );
}

#[test]
fn hover_enum_variant() {
    let source = r#"enum Color {
    Red,
    Green,
    Blue,
}

fn main() {
    let c = Color::Red
}"#;
    let project = Project::single("hover_enum_variant", source);
    let reply = project.cli("hover", Some((7, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: enum Color".trim()
    );
}

#[test]
fn hover_generic_instance() {
    let source = r#"struct Box<T> {
    item: T
}

fn main() {
    let b = Box { item: 42 }
}"#;
    let project = Project::single("hover_generic_instance", source);
    let reply = project.cli("hover", Some((5, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: Box<int>".trim()
    );
}

#[test]
fn hover_global_assignment() {
    let source = r#"global score: int = 0

fn main() {
    score = 100
}"#;
    let project = Project::single("hover_global_assignment", source);
    let reply = project.cli("hover", Some((3, 4)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_safe_nav_result() {
    let source = r#"struct Player {
    name: string
}

fn main() {
    let p: Player? = null
    let n = p?.name
}"#;
    let project = Project::single("hover_safe_nav_result", source);
    let reply = project.cli("hover", Some((6, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: string?".trim()
    );
}

#[test]
fn hover_null_coalesce_result() {
    let source = r#"fn main() {
    let x: int? = null
    let y = x ?? 42
}"#;
    let project = Project::single("hover_null_coalesce_result", source);
    let reply = project.cli("hover", Some((2, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_tuple_destructure() {
    let source = r#"fn main() {
    let pair = (1, "hello")
    let (a, b) = pair
}"#;
    let project = Project::single("hover_tuple_destructure", source);
    let reply = project.cli("hover", Some((2, 9)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_nested_tuple_destructure() {
    let source = r#"fn main() {
    let pair = ((1, 2), "hello")
    let ((a, b), c) = pair
}"#;
    let project = Project::single("hover_nested_tuple_destructure", source);
    let reply = project.cli("hover", Some((2, 10)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

const HOVER_MATCH_RECORD_BINDING_SHORTHAND: &str = r#"enum Opt {
    Some { value: int },
    None,
}

fn main() {
    let o = Opt::Some { value: 42 }
    match o {
        Opt::Some { value } => { print(value) },
        Opt::None => {},
    }
}"#;

#[test]
fn hover_match_record_binding_shorthand() {
    let source = HOVER_MATCH_RECORD_BINDING_SHORTHAND;
    let project = Project::single("hover_match_record_binding_shorthand", source);
    let reply = project.cli("hover", Some((8, 20)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

fn hover_match_record_binding_explicit_source() -> String {
    HOVER_MATCH_RECORD_BINDING_SHORTHAND.replace(
        "        Opt::Some { value } => { print(value) },",
        "        Opt::Some { value: v } => { print(v) },",
    )
}

#[test]
fn hover_match_record_binding_explicit() {
    let source = &hover_match_record_binding_explicit_source();
    let project = Project::single("hover_match_record_binding_explicit", source);
    let reply = project.cli("hover", Some((8, 27)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_match_enum_tuple_binding() {
    let source = r#"enum Opt {
    Some(int),
    None,
}

fn main() {
    let o = Opt::Some(42)
    match o {
        Opt::Some(v) => { print(v) },
        Opt::None => {},
    }
}"#;
    let project = Project::single("hover_match_enum_tuple_binding", source);
    let reply = project.cli("hover", Some((8, 18)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_while_let_binding() {
    let source = r#"fn main() {
    let x: int? = 5
    while let val = x {
        print(val)
        break
    }
}"#;
    let project = Project::single("hover_while_let_binding", source);
    let reply = project.cli("hover", Some((2, 14)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_if_let_binding() {
    let source = r#"fn main() {
    let x: int? = 5
    if let val = x {
        print(val)
    }
}"#;
    let project = Project::single("hover_if_let_binding", source);
    let reply = project.cli("hover", Some((2, 11)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

const HOVER_MATCH_ARM_RECORD_VARIANT: &str = r#"enum E {
    A(x: int),
    B { y: int, z: int },
}

fn f(e: E) -> int {
    match e {
        E::A(n) => n
        E::B { y, z } => y + z
    }
}"#;

#[test]
fn hover_match_arm_record_variant() {
    let source = HOVER_MATCH_ARM_RECORD_VARIANT;
    let project = Project::single("hover_match_arm_record_variant", source);
    let reply = project.cli("hover", Some((8, 11)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        r#"```rock
E::B { y: int, z: int }
```"#
            .trim()
    );
}

#[test]
fn hover_match_arm_record_variant_field_binding() {
    let source = HOVER_MATCH_ARM_RECORD_VARIANT;
    let project = Project::single("hover_match_arm_record_variant_field_binding", source);
    let reply = project.cli("hover", Some((8, 15)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: int".trim()
    );
}

#[test]
fn hover_match_record_variant_field_name() {
    let source = &hover_match_record_binding_explicit_source();
    let project = Project::single("hover_match_record_variant_field_name", source);
    let reply = project.cli("hover", Some((8, 20)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "value: int (field of Opt::Some)".trim()
    );
}

#[test]
fn hover_struct_pattern_field_name() {
    let source = r#"struct Point { x: int, y: int }

fn main() {
    let p = Point { x: 1, y: 2 }
    match p {
        Point { x: a, y: b } => { print(a + b) },
    }
}"#;
    let project = Project::single("hover_struct_pattern_field_name", source);
    let reply = project.cli("hover", Some((5, 16)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "x: int (field of Point)".trim()
    );
}
