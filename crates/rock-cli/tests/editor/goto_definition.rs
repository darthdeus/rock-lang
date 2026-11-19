use super::*;

#[test]
fn goto_definition_function() {
    let source = r#"fn calculate(x: int, y: int) -> int {
    return x + y
}

fn main() {
    let result = calculate(5, 3)
}"#;
    let project = Project::single("goto_definition_function", source);
    let reply = project.cli("definition", Some((5, 17)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 3);
}

#[test]
fn goto_definition_variable() {
    let source = r#"fn main() {
    let number = 42
    let doubled = number * 2
    print(doubled)
}"#;
    let project = Project::single("goto_definition_variable", source);
    let reply = project.cli("definition", Some((2, 18)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 1);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 8);
}

#[test]
fn goto_definition_struct_field() {
    let source = r#"struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
    let coordinate = p.x
}"#;
    let project = Project::single("goto_definition_struct_field", source);
    let reply = project.cli("definition", Some((7, 23)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 1);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 4);
}

#[test]
fn goto_definition_struct_type() {
    let source = r#"struct Vector {
    x: float,
    y: float,
}

fn create_vector() -> Vector {
    return Vector { x: 0.0, y: 0.0 }
}"#;
    let project = Project::single("goto_definition_struct_type", source);
    let reply = project.cli("definition", Some((5, 22)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn goto_definition_parameter() {
    let source = r#"fn process(data: string, count: int) -> string {
    let result = data + count.to_string()
    return result
}"#;
    let project = Project::single("goto_definition_parameter", source);
    let reply = project.cli("definition", Some((1, 17)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 11);
}

#[test]
fn goto_definition_local_variable() {
    let source = r#"fn calculate() {
    let base_value = 10
    let multiplier = 3
    let result = base_value * multiplier
    print(result)
}"#;
    let project = Project::single("goto_definition_local_variable", source);
    let reply = project.cli("definition", Some((3, 17)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 1);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 8);
}

const GOTO_DEF_SHORTHAND_X_VALUE: &str = r#"struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    // Goto definition on 'x' value part (should go to variable)
    let p = Point { x, y }
}"#;

#[test]
fn goto_def_shorthand_x_value() {
    let source = GOTO_DEF_SHORTHAND_X_VALUE;
    let project = Project::single("goto_def_shorthand_x_value", source);
    let reply = project.cli("definition", Some((10, 20)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 6);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 8);
}

#[test]
fn goto_def_shorthand_y_value() {
    let source = &GOTO_DEF_SHORTHAND_X_VALUE.replace(
        "    // Goto definition on 'x' value part (should go to variable)",
        "    // Goto definition on 'y' value part",
    );
    let project = Project::single("goto_def_shorthand_y_value", source);
    let reply = project.cli("definition", Some((10, 23)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 7);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 8);
}

#[test]
fn goto_def_mixed_shorthand_x() {
    let source = r#"struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100

    // Mixed shorthand - goto def on 'x'
    let p = Point { x, y: 200 }
}"#;
    let project = Project::single("goto_def_mixed_shorthand_x", source);
    let reply = project.cli("definition", Some((9, 20)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 6);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 8);
}

#[test]
fn goto_def_struct_literal_name() {
    let source = r#"struct Player {
    name: string
    health: int
}

fn main() {
    let player = Player { name: "Hero", health: 100 }
}"#;
    let project = Project::single("goto_def_struct_literal_name", source);
    let reply = project.cli("definition", Some((6, 17)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn goto_def_struct_literal_name_assignment() {
    let source = r#"struct Enemy {
    kind: string
}

fn main() {
    let e: Enemy = Enemy { kind: "goblin" }
}"#;
    let project = Project::single("goto_def_struct_literal_name_assignment", source);
    let reply = project.cli("definition", Some((5, 19)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn goto_def_generic_struct_literal() {
    let source = r#"struct Box<T> {
    item: T
}

fn main() {
    let b = Box { item: 42 }
}"#;
    let project = Project::single("goto_def_generic_struct_literal", source);
    let reply = project.cli("definition", Some((5, 12)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn goto_def_enum_record_variant() {
    let source = r#"enum Shape {
    Circle { radius: float },
    Rect { w: float, h: float },
}

fn main() {
    let s = Shape::Circle { radius: 5.0 }
}"#;
    let project = Project::single("goto_def_enum_record_variant", source);
    let reply = project.cli("definition", Some((6, 19)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 1);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 4);
}

#[test]
fn goto_def_function_call() {
    let source = r#"fn double(x: int) -> int {
    return x * 2
}

fn main() {
    let x = 5
    let y = double(x)
}"#;
    let project = Project::single("goto_def_function_call", source);
    let reply = project.cli("definition", Some((6, 12)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 3);
}

#[test]
fn goto_def_for_loop_iterable() {
    let source = r#"fn main() {
    let items = [1, 2, 3]
    for x in items {
        print(x)
    }
}"#;
    let project = Project::single("goto_def_for_loop_iterable", source);
    let reply = project.cli("definition", Some((2, 13)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 1);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 8);
}

#[test]
fn goto_def_closure_captured_var() {
    let source = r#"fn main() {
    let base = 10
    let f = (x: int) => x + base
}"#;
    let project = Project::single("goto_def_closure_captured_var", source);
    let reply = project.cli("definition", Some((2, 28)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 1);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 8);
}

#[test]
fn goto_def_nested_field_access() {
    let source = r#"struct Inner {
    value: int
}

struct Outer {
    inner: Inner
}

fn main() {
    let o = Outer { inner: Inner { value: 42 } }
    let v = o.inner.value
}"#;
    let project = Project::single("goto_def_nested_field_access", source);
    let reply = project.cli("definition", Some((10, 20)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 1);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 4);
}

#[test]
fn goto_def_enum_simple_variant() {
    let source = r#"enum Color {
    Red,
    Green,
    Blue,
}

fn main() {
    let c = Color::Red
}"#;
    let project = Project::single("goto_def_enum_simple_variant", source);
    let reply = project.cli("definition", Some((7, 19)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 1);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 4);
}

#[test]
fn goto_def_enum_qualified_variant() {
    let source = r#"enum Direction {
    Up,
    Down,
}

fn main() {
    let d = Direction::Down
}"#;
    let project = Project::single("goto_def_enum_qualified_variant", source);
    let reply = project.cli("definition", Some((6, 23)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 2);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 4);
}

#[test]
fn goto_def_enum_qualifier_path_segment() {
    let source = r#"enum Color { Red, Green }
fn f() -> Color {
    Color::Red
}"#;
    let project = Project::single("goto_def_enum_qualifier_path_segment", source);
    let reply = project.cli("definition", Some((2, 4)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 5);
}

#[test]
fn goto_def_global_variable_assignment() {
    let source = r#"global counter: int = 0

fn main() {
    counter = 42
}"#;
    let project = Project::single("goto_def_global_variable_assignment", source);
    let reply = project.cli("definition", Some((3, 4)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn goto_def_struct_field_in_literal() {
    let source = r#"struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
}"#;
    let project = Project::single("goto_def_struct_field_in_literal", source);
    let reply = project.cli("definition", Some((6, 20)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 1);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 4);
}

#[test]
fn goto_def_field_ref_key() {
    let source = r#"struct Mob { health: int, attack_co: int? }
fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    kill_co(mob, .attack_co)
}"#;
    let project = Project::single("goto_def_field_ref_key", source);
    let reply = project.cli("definition", Some((3, 19)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 26);
}

const GOTO_DEF_BUILTIN_COROUTINE_JUMPS_TO_STDLIB_RESUME_LANDMARK: &str = r#"co fn producer() -> Coroutine<int> {
    yield 42
}"#;

#[test]
fn goto_def_builtin_coroutine_jumps_to_stdlib_resume_landmark() {
    let source = GOTO_DEF_BUILTIN_COROUTINE_JUMPS_TO_STDLIB_RESUME_LANDMARK;
    let project = Project::single(
        "goto_def_builtin_coroutine_jumps_to_stdlib_resume_landmark",
        source,
    );
    let reply = project.cli("definition", Some((0, 20)), &[]);
    assert!(array(&reply["definitions"]).len() >= 1, "{reply}");
}

#[test]
fn goto_def_keyword_yield_returns_empty() {
    let source = GOTO_DEF_BUILTIN_COROUTINE_JUMPS_TO_STDLIB_RESUME_LANDMARK;
    let project = Project::single("goto_def_keyword_yield_returns_empty", source);
    let reply = project.cli("definition", Some((1, 4)), &[]);
    assert!(array(&reply["definitions"]).is_empty(), "{reply}");
}

const GOTO_DEF_PRIMITIVE_TYPE_RETURNS_EMPTY: &str = r#"fn make() -> Vec<int> {
    return vec_new()
}"#;

#[test]
fn goto_def_primitive_type_returns_empty() {
    let source = GOTO_DEF_PRIMITIVE_TYPE_RETURNS_EMPTY;
    let project = Project::single("goto_def_primitive_type_returns_empty", source);
    let reply = project.cli("definition", Some((0, 17)), &[]);
    assert!(array(&reply["definitions"]).is_empty(), "{reply}");
}

#[test]
fn goto_def_builtin_map_jumps_to_stdlib_map_new() {
    let source = r#"fn make() -> Map<string, int> {
    return map_new()
}"#;
    let project = Project::single("goto_def_builtin_map_jumps_to_stdlib_map_new", source);
    let reply = project.cli("definition", Some((0, 13)), &[]);
    assert!(array(&reply["definitions"]).len() >= 1, "{reply}");
}

#[test]
fn goto_def_builtin_coroutine_jumps_to_stdlib_resume() {
    let source = r#"fn use_coroutine(c: Coroutine<int>) -> int? {
    return resume(c)
}"#;
    let project = Project::single("goto_def_builtin_coroutine_jumps_to_stdlib_resume", source);
    let reply = project.cli("definition", Some((0, 20)), &[]);
    assert!(array(&reply["definitions"]).len() >= 1, "{reply}");
}

#[test]
fn goto_def_builtin_vec_jumps_to_stdlib_vec_struct() {
    let source = GOTO_DEF_PRIMITIVE_TYPE_RETURNS_EMPTY;
    let project = Project::single("goto_def_builtin_vec_jumps_to_stdlib_vec_struct", source);
    let reply = project.cli("definition", Some((0, 13)), &[]);
    assert!(array(&reply["definitions"]).len() >= 1, "{reply}");
}

#[test]
fn goto_def_builtin_native_buffer_jumps_to_stdlib() {
    let source = r#"fn make() -> NativeBuffer<int> {
    return native_buffer_int(10)
}"#;
    let project = Project::single("goto_def_builtin_native_buffer_jumps_to_stdlib", source);
    let reply = project.cli("definition", Some((0, 13)), &[]);
    assert!(array(&reply["definitions"]).len() >= 1, "{reply}");
}

#[test]
fn goto_def_builtin_native_buffer_u8_jumps_to_stdlib() {
    let source = r#"fn make() -> NativeBufferU8 {
    return native_buffer_u8(10)
}"#;
    let project = Project::single("goto_def_builtin_native_buffer_u8_jumps_to_stdlib", source);
    let reply = project.cli("definition", Some((0, 13)), &[]);
    assert!(array(&reply["definitions"]).len() >= 1, "{reply}");
}

#[test]
fn goto_def_builtin_primitive_int_returns_nothing() {
    let source = r#"fn make() -> int {
    return 42
}"#;
    let project = Project::single("goto_def_builtin_primitive_int_returns_nothing", source);
    let reply = project.cli("definition", Some((0, 13)), &[]);
    assert!(array(&reply["definitions"]).is_empty(), "{reply}");
}

#[test]
fn goto_def_record_variant_pattern_field_name() {
    let source = r#"enum Opt {
    Some { value: int },
    None,
}

fn main() {
    let o = Opt::Some { value: 42 }
    match o {
        Opt::Some { value: v } => { print(v) },
        Opt::None => {},
    }
}"#;
    let project = Project::single("goto_def_record_variant_pattern_field_name", source);
    let reply = project.cli("definition", Some((8, 20)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 1);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 11);
}

#[test]
fn goto_def_struct_pattern_field_name() {
    let source = r#"struct Point { x: int, y: int }

fn main() {
    let p = Point { x: 1, y: 2 }
    match p {
        Point { x: a, y: b } => { print(a + b) },
    }
}"#;
    let project = Project::single("goto_def_struct_pattern_field_name", source);
    let reply = project.cli("definition", Some((5, 16)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 15);
}

#[test]
fn goto_definition_macro_invocation_name() {
    let source = r#"macro twice(x: expr) {
    let _ = x + x
}

fn main() {
    twice(41)
}"#;
    let project = Project::single("goto_definition_macro_invocation_name", source);
    let reply = project.cli("definition", Some((5, 6)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 6);
}

#[test]
fn goto_definition_macro_overload_resolves_by_arity() {
    let source = r#"macro pick(a: expr) {
    let _ = a
}
macro pick(a: expr, b: expr) {
    let _ = a
    let _ = b
}
fn main() {
    pick(1, 2)
}"#;
    let project = Project::single("goto_definition_macro_overload_resolves_by_arity", source);
    let reply = project.cli("definition", Some((8, 5)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 3);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 6);
}
