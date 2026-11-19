use super::*;

#[test]
fn tokens_survive_field_addition() {
    let step_1 = r#"struct Player {
    health: int,
}

fn main() {
    let p = Player { health: 100 }
    print(p.health.to_string())
}"#;
    let source = step_1;
    let mut doc = Document::open(
        Project::single("tokens_survive_field_addition", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.replace(
        &step_1
            .replace(
                "    health: int,\n",
                r#"    health: int,
    name: string,
"#,
            )
            .replace(
                "    let p = Player { health: 100 }\n",
                "    let p = Player { health: 100, name: \"Hero\" }\n",
            )
            .replace(
                "    print(p.health.to_string())\n",
                r#"    print(p.health.to_string())
    print(p.name)
"#,
            ),
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.replace(step_1);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
}

#[test]
fn tokens_recover_after_syntax_error() {
    let step_1 = r#"struct Enemy {
    health: int,
    damage: int,
}

fn attack(e: Enemy) -> int {
    return e.damage
}

fn main() {
    let e = Enemy { health: 50, damage: 10 }
    let d = attack(e)
}"#;
    let source = step_1;
    let mut doc = Document::open(
        Project::single("tokens_recover_after_syntax_error", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 40, "{reply}");
    doc.replace(&step_1.replace("    let d = attack(e)", "    let d = attack("));
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 5, "{reply}");
    doc.replace(step_1);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 40, "{reply}");
}

#[test]
fn tokens_survive_function_lifecycle() {
    let source = r#"fn greet(name: string) -> string {
    return "Hello " + name
}

fn main() {
    print(greet("world"))
}"#;
    let mut doc = Document::open(
        Project::single("tokens_survive_function_lifecycle", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    let step_2 = r#"fn greet(name: string) -> string {
    return "Hello " + name
}

fn farewell(name: string) -> string {
    return "Goodbye " + name
}

fn main() {
    print(greet("world"))
    print(farewell("world"))
}"#;
    doc.replace(step_2);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 40, "{reply}");
    doc.replace(
        &step_2
            .replace(
                r#"fn greet(name: string) -> string {
    return "Hello " + name
}

"#,
                "",
            )
            .replace("    print(greet(\"world\"))\n", ""),
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
}

#[test]
fn tokens_survive_rapid_edits() {
    let source = r#"fn add(a: int, b: int) -> int { return a + b }
fn main() { let x = add(1, 2) }"#;
    let mut doc = Document::open(
        Project::single("tokens_survive_rapid_edits", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.replace(
        r#"fn add(a: int, b: int) -> int { return a + b }
fn sub(a: int, b: int) -> int { return a - b }
fn main() { let x = add(1, 2) + sub(3, 1) }"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.replace(
        r#"fn add(a: int, b: int) -> int { return a + b }
fn sub(a: int, b: int) -> int { return a - b }
fn mul(a: int, b: int) -> int { return a * b }
fn main() { let x = mul(add(1, 2), sub(3, 1)) }"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.replace(
        r#"fn mul(a: int, b: int) -> int { return a * b }
fn main() { let x = mul(2, 3) }"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.replace(
        r#"fn mul(a: int, b: int) -> int { return a * b }
fn div(a: int, b: int) -> int { return a / b }
fn main() { let x = div(mul(6, 2), 3) }"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
}

#[test]
fn tokens_after_type_change() {
    let step_1 = r#"struct FixVec2 {
    x: float,
    y: float,
}

fn distance(a: FixVec2, b: FixVec2) -> float {
    let dx = b.x - a.x
    let dy = b.y - a.y
    return (dx * dx + dy * dy).sqrt()
}"#;
    let source = step_1;
    let mut doc = Document::open(Project::single("tokens_after_type_change", source), source);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 40, "{reply}");
    doc.replace(
        r#"struct FixVec2 {
    x: float,
    y: float,
}

struct FixVec3 {
    x: float,
    y: float,
    z: float,
}

fn distance(a: FixVec2, b: FixVec2) -> float {
    let dx = b.x - a.x
    let dy = b.y - a.y
    return (dx * dx + dy * dy).sqrt()
}

fn distance3d(a: FixVec3, b: FixVec3) -> float {
    let dx = b.x - a.x
    let dy = b.y - a.y
    let dz = b.z - a.z
    return (dx * dx + dy * dy + dz * dz).sqrt()
}"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 75, "{reply}");
    doc.replace(step_1);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 40, "{reply}");
}
