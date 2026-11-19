use super::*;

#[test]
fn type_function_call_char_by_char() {
    let source = r#"struct Player {
    health: int,
    name: string,
}

fn main() {
    let p = Player { health: 100, name: "Hero" }

}"#;
    let mut doc = Document::open(
        Project::single("type_function_call_char_by_char", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.type_at(7, 4, "print(p.health.to_string())");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
}

#[test]
fn type_new_function_char_by_char() {
    let source = r#"struct FixVec2 {
    x: float,
    y: float,
}

fn main() {
    let a = FixVec2 { x: 1.0, y: 2.0 }
}"#;
    let mut doc = Document::open(
        Project::single("type_new_function_char_by_char", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.type_at(
        5,
        0,
        r#"fn length(v: FixVec2) -> float {
    return (v.x * v.x + v.y * v.y).sqrt()
}
"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 40, "{reply}");
}

#[test]
fn type_struct_field_then_use_it() {
    let source = r#"struct Enemy {
    health: int,
}

fn main() {
    let e = Enemy { health: 50 }
}"#;
    let mut doc = Document::open(
        Project::single("type_struct_field_then_use_it", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.type_at(2, 0, "    damage: int,");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.type_at(7, 0, "    print(e.damage.to_string())");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
}

#[test]
fn type_and_delete_cycles() {
    let source = r#"fn greet(name: string) -> string {
    return "Hello " + name
}

fn main() {
    let msg = greet("world")
    print(msg)
}"#;
    let mut doc = Document::open(Project::single("type_and_delete_cycles", source), source);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.type_at(
        4,
        0,
        r#"fn farewell(name: string) -> string {
    return "Bye " + name
}
"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 40, "{reply}");
    doc.delete_at(7, 1, 36);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 5, "{reply}");
    doc.delete_at(6, 1, 32);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 5, "{reply}");
    doc.delete_at(5, 0, 1);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
}

#[test]
fn type_match_expression_char_by_char() {
    let source = r#"enum Direction {
    Up,
    Down,
    Left,
    Right,
}

fn main() {
    let d = Direction::Up

}"#;
    let mut doc = Document::open(
        Project::single("type_match_expression_char_by_char", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.type_at(
        9,
        0,
        r#"    match d {
        Direction::Up => print("up"),
        Direction::Down => print("down"),
        Direction::Left => print("left"),
        Direction::Right => print("right"),
    }"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 50, "{reply}");
}

#[test]
fn type_long_expression_with_dots() {
    let source = r#"struct Pos {
    x: float,
    y: float,
}

struct Entity {
    pos: Pos,
    name: string,
}

fn main() {
    let e = Entity { pos: Pos { x: 1.0, y: 2.0 }, name: "test" }

}"#;
    let mut doc = Document::open(
        Project::single("type_long_expression_with_dots", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 40, "{reply}");
    doc.type_at(
        12,
        4,
        "print(e.pos.x.to_string() + \", \" + e.pos.y.to_string())",
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 40, "{reply}");
}

#[test]
fn type_in_middle_of_existing_code() {
    let source = r#"fn add(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let result = add(1, 2)
    print(result.to_string())
}"#;
    let mut doc = Document::open(
        Project::single("type_in_middle_of_existing_code", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.type_at(
        4,
        0,
        r#"
fn multiply(a: int, b: int) -> int {
    return a * b
}"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 40, "{reply}");
    doc.type_at(
        10,
        0,
        r#"    let product = multiply(3, 4)
    print(product.to_string())"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 50, "{reply}");
}
