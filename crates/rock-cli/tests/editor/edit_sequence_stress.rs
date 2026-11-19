use super::*;

#[test]
#[ignore = "long editor stress sequence"]
fn stress_type_delete_type_cycle() {
    let source = r#"struct Player {
    health: int,
    mana: int,
    name: string,
}

fn main() {
    let p = Player { health: 100, mana: 50, name: "Hero" }
}"#;
    let mut doc = Document::open(
        Project::single("stress_type_delete_type_cycle", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.type_at(
        9,
        0,
        r#"
fn heal(p: Player, amount: int) -> Player {
    return Player { health: p.health + amount, mana: p.mana, name: p.name }
}"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 50, "{reply}");
    doc.type_at(
        13,
        0,
        r#"
fn cast_spell(p: Player, cost: int) -> Player {
    return Player { health: p.health, mana: p.mana - cost, name: p.name }
}"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 75, "{reply}");
    doc.type_at(
        17,
        0,
        r#"
fn take_damage(p: Player, damage: int) -> Player {
    return Player { health: p.health - damage, mana: p.mana, name: p.name }
}"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 100, "{reply}");
    doc.type_at(
        22,
        4,
        r#"    let p = heal(p, 20)
    let p = cast_spell(p, 10)
    let p = take_damage(p, 30)
    print(p.health.to_string())"#,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 100, "{reply}");
}

#[test]
#[ignore = "long editor stress sequence"]
fn stress_syntax_error_recovery_cycle() {
    let source = r#"struct Item {
    name: string,
    value: int,
}

fn describe(item: Item) -> string {
    return item.name + ": " + item.value.to_string()
}

fn main() {
    let sword = Item { name: "Sword", value: 50 }
    print(describe(sword))
}"#;
    let mut doc = Document::open(
        Project::single("stress_syntax_error_recovery_cycle", source),
        source,
    );
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 50, "{reply}");
    doc.type_at(13, 0, "    let shield = Item { name: \"Shield\", valu");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 5, "{reply}");
    doc.type_at(13, 46, "e: 30 }");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 50, "{reply}");
    doc.type_at(14, 0, "    print(describe(shield))");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 50, "{reply}");
    doc.type_at(15, 0, "    let potion = Item { name: \"");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 5, "{reply}");
    doc.type_at(15, 32, "Potion\", value: 10 }");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 50, "{reply}");
    doc.type_at(16, 0, "    print(describe(potion))");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 50, "{reply}");
}

#[test]
#[ignore = "long editor stress sequence"]
fn stress_many_small_edits() {
    let source = r#"fn main() {
    let x = 1
}"#;
    let mut doc = Document::open(Project::single("stress_many_small_edits", source), source);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 15, "{reply}");
    doc.type_at(2, 0, "    let y = 2");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 15, "{reply}");
    doc.type_at(3, 0, "    let z = 3");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 15, "{reply}");
    doc.type_at(4, 0, "    let a = x + y");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 15, "{reply}");
    doc.type_at(5, 0, "    let b = y + z");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 15, "{reply}");
    doc.type_at(6, 0, "    let c = a + b");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 15, "{reply}");
    doc.type_at(7, 0, "    print(c.to_string())");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 15, "{reply}");
    doc.delete_at(7, 24, 24);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 15, "{reply}");
    doc.delete_at(6, 18, 18);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 15, "{reply}");
    doc.type_at(6, 0, "    let total = x + y + z + a + b");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 15, "{reply}");
    doc.type_at(7, 0, "    print(total.to_string())");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 15, "{reply}");
}

#[test]
#[ignore = "long editor stress sequence"]
fn stress_struct_evolution() {
    let step_1 = r#"struct Config {
    width: int,
}

fn main() {
    let c = Config { width: 800 }
    print(c.width.to_string())
}"#;
    let source = step_1;
    let mut doc = Document::open(Project::single("stress_struct_evolution", source), source);
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.type_at(2, 0, "    height: int,");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.type_at(3, 0, "    title: string,");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.type_at(4, 0, "    fullscreen: bool,");
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 25, "{reply}");
    doc.replace(&step_1.replace("    width: int,\n", r#"    width: int,
    height: int,
    title: string,
    fullscreen: bool,
"#).replace("    let c = Config { width: 800 }\n", "    let c = Config { width: 800, height: 600, title: \"Game\", fullscreen: false }\n").replace("    print(c.width.to_string())\n", r#"    print(c.width.to_string())
    print(c.height.to_string())
    print(c.title)
"#));
    let reply = doc.request("textDocument/semanticTokens/full", json!({}));
    assert!(array(&reply["data"]).len() >= 50, "{reply}");
}
