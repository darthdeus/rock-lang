use super::*;

#[test]
fn inlay_hints_let_bindings() {
    let source = r#"fn main() {
    let x = 42
    let y = "hello"
    let z = [1, 2, 3]
    let w = true
}"#;
    let project = Project::single("inlay_hints_let_bindings", source);
    let reply = project.cli("inlay-hints", None, &[]);
    contains_items(
        &reply["hints"],
        &[
            json!({"position": {"line": 1}, "label": {"text": "int", "strip_prefix": ":"}}),
            json!({"position": {"line": 2}, "label": {"text": "string", "strip_prefix": ":"}}),
            json!({"position": {"line": 3}, "label": {"text": "[int]", "strip_prefix": ":"}}),
            json!({"position": {"line": 4}, "label": {"text": "bool", "strip_prefix": ":"}}),
        ],
    );
}

#[test]
fn inlay_hints_complex_types() {
    let source = r#"struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
    let points = [p, Point { x: 3.0, y: 4.0 }]
    let map = map_new()
    map.insert("origin", p)
}"#;
    let project = Project::single("inlay_hints_complex_types", source);
    let reply = project.cli("inlay-hints", None, &[]);
    contains_items(
        &reply["hints"],
        &[
            json!({"position": {"line": 6}, "label": {"text": "Point", "strip_prefix": ":"}}),
            json!({"position": {"line": 7}, "label": {"text": "[Point]", "strip_prefix": ":"}}),
            json!({"position": {"line": 8}, "label": {"text": "Map<string, Point>", "strip_prefix": ":"}}),
        ],
    );
}

#[test]
fn inlay_hints_function_parameters() {
    let source = r#"fn calculate(x: int, y: int, operation: string) -> int {
    if operation == "add" {
        return x + y
    } else {
        return x - y
    }
}

fn main() {
    let result = calculate(10, 20, "add")
}"#;
    let project = Project::single("inlay_hints_function_parameters", source);
    let reply = project.cli("inlay-hints", None, &[]);
    contains_items(
        &reply["hints"],
        &[
            json!({"position": {"line": 9}, "label": {"text": "int", "strip_prefix": ":"}}),
            json!({"position": {"line": 9}, "label": {"text": "x:", "strip_prefix": ":"}}),
            json!({"position": {"line": 9}, "label": {"text": "y:", "strip_prefix": ":"}}),
            json!({"position": {"line": 9}, "label": {"text": "operation:", "strip_prefix": ":"}}),
        ],
    );
}

#[test]
fn inlay_hints_generic_types() {
    let source = r#"fn identity<T>(value: T) -> T {
    return value
}

fn main() {
    let num = identity(42)
    let text = identity("hello")
    let list = identity([1, 2, 3])
}"#;
    let project = Project::single("inlay_hints_generic_types", source);
    let reply = project.cli("inlay-hints", None, &[]);
    contains_items(
        &reply["hints"],
        &[
            json!({"position": {"line": 5}, "label": {"text": "int", "strip_prefix": ":"}}),
            json!({"position": {"line": 5}, "label": {"text": "value:", "strip_prefix": ":"}}),
            json!({"position": {"line": 6}, "label": {"text": "string", "strip_prefix": ":"}}),
            json!({"position": {"line": 6}, "label": {"text": "value:", "strip_prefix": ":"}}),
            json!({"position": {"line": 7}, "label": {"text": "[int]", "strip_prefix": ":"}}),
        ],
    );
}

#[test]
fn inlay_hints_nullable_types() {
    let source = r#"fn find_user(id: int) -> string? {
    if id == 1 {
        return "Alice"
    } else {
        return null
    }
}

fn main() {
    let user = find_user(1)
    let missing = find_user(999)
}"#;
    let project = Project::single("inlay_hints_nullable_types", source);
    let reply = project.cli("inlay-hints", None, &[]);
    contains_items(
        &reply["hints"],
        &[
            json!({"position": {"line": 9}, "label": {"text": "string?", "strip_prefix": ":"}}),
            json!({"position": {"line": 9}, "label": {"text": "id:", "strip_prefix": ":"}}),
            json!({"position": {"line": 10}, "label": {"text": "string?", "strip_prefix": ":"}}),
            json!({"position": {"line": 10}, "label": {"text": "id:", "strip_prefix": ":"}}),
        ],
    );
}

#[test]
fn inlay_hints_closures() {
    let source = r#"fn main() {
    let add = (a, b) => a + b
    let multiply = (x: int, y: int) => { return x * y }
    let greet = (name) => print("Hello, " + name)
}"#;
    let project = Project::single("inlay_hints_closures", source);
    let reply = project.cli("inlay-hints", None, &[]);
    contains_items(
        &reply["hints"],
        &[
            json!({"position": {"line": 2}, "label": {"text": "fn(int, int) -> int", "strip_prefix": ":"}}),
        ],
    );
}

#[test]
fn inlay_hints_for_loop_variable() {
    let source = r#"struct Item { name: string }

fn main() {
    let items: [Item] = vec_new()
    for item in items {
        print(item.name)
    }
}"#;
    let project = Project::single("inlay_hints_for_loop_variable", source);
    let reply = project.cli("inlay-hints", None, &[]);
    contains_items(
        &reply["hints"],
        &[json!({"position": {"line": 4}, "label": {"text": "Item", "strip_prefix": ":"}})],
    );
}

#[test]
fn inlay_hints_match_arm_enum_variant() {
    let source = r#"struct Point { x: int, y: int }

enum Opt {
    None,
    Some(Point),
}

fn main() {
    let o = Opt::Some(Point { x: 1, y: 2 })
    match o {
        Opt::Some(p) => print(p.x),
        Opt::None => {},
    }
}"#;
    let project = Project::single("inlay_hints_match_arm_enum_variant", source);
    let reply = project.cli("inlay-hints", None, &[]);
    contains_items(
        &reply["hints"],
        &[
            json!({"position": {"line": 8}, "label": {"text": "Opt", "strip_prefix": ":"}}),
            json!({"position": {"line": 10}, "label": {"text": "Point", "strip_prefix": ":"}}),
        ],
    );
}

#[test]
fn inlay_hints_dform_receiver() {
    let source = r#"struct Mob { health: float }

fn Mob.take_damage(m, amount: float) {
    m.health = m.health - amount
}

fn Vec<T>.first(v) -> T {
    return v[0]
}"#;
    let project = Project::single("inlay_hints_dform_receiver", source);
    let reply = project.cli("inlay-hints", None, &[]);
    contains_items(
        &reply["hints"],
        &[
            json!({"position": {"line": 2}, "label": {"text": "Mob", "strip_prefix": ":"}}),
            json!({"position": {"line": 6}, "label": {"text": "Vec<T>", "strip_prefix": ":"}}),
        ],
    );
}
