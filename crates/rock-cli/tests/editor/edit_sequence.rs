use super::*;

const ENEMY: &str = r#"struct Enemy {
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

#[test]
fn tokens_after_whole_text_changes_match_a_fresh_open() {
    let with_field = ENEMY
        .replace(
            "    damage: int,\n",
            "    damage: int,\n    name: string,\n",
        )
        .replace("damage: 10 }", "damage: 10, name: \"Orc\" }")
        + "\n\nfn name_of(e: Enemy) -> string {\n    return e.name\n}";
    let without_attack = ENEMY.replace(
        "fn attack(e: Enemy) -> int {\n    return e.damage\n}\n\n",
        "",
    );
    let mut doc = Document::single(ENEMY);
    doc.assert_tokens_match_fresh();
    for step in [
        ENEMY.replace("    let d = attack(e)", "    let d = attack("),
        ENEMY.to_owned(),
        with_field,
        without_attack,
        ENEMY.to_owned(),
    ] {
        doc.replace(&step);
        doc.assert_tokens_match_fresh();
    }
}

#[test]
fn tokens_after_typing_and_deleting_match_a_fresh_open() {
    let source = r#"fn greet(name: string) -> string {
    return "Hello " + name
}

fn main() {
    let msg = greet("world")
    print(msg)
}"#;
    let mut doc = Document::single(source);
    doc.type_at(
        4,
        0,
        "fn farewell(name: string) -> string {\n    return \"Bye \" + name\n}\n",
    );
    doc.assert_tokens_match_fresh();
    doc.delete_at(7, 1, 36);
    doc.assert_tokens_match_fresh();
    doc.delete_at(6, 1, 32);
    doc.assert_tokens_match_fresh();
    doc.delete_at(5, 0, 1);
    doc.assert_tokens_match_fresh();
    doc.type_at(
        7,
        4,
        "    match msg.len() {\n        0 => print(\"empty\"),\n        _ => print(msg),\n    }",
    );
    doc.assert_tokens_match_fresh();
}

#[test]
fn hover_after_typing_a_for_loop() {
    let source = r#"struct AggressorEntry {
    dmg: int
}

struct Mob {
    aggressors: [AggressorEntry] = [],
}

fn f(m: Mob) {

}"#;
    let mut doc = Document::single(source);
    contains_text(&doc.hover_text(8, 5), "Mob");
    doc.type_at(9, 0, "for a in m.aggressors {\n    print(a.dmg)\n}");
    contains_text(&doc.hover_text(9, 4), "AggressorEntry");
    contains_text(&doc.hover_text(9, 14), "[AggressorEntry]");
}

#[test]
fn references_shift_after_typing_and_deleting_a_line_in_another_fn() {
    let source = r#"fn alpha() -> int {
    return 1
}

fn beta() -> int {
    let count = 2
    return count + count
}"#;
    let mut doc = Document::single(source);
    let references = doc.at("textDocument/references", 5, 8);
    reference_starts("count", &references, &[(5, 8), (6, 11), (6, 19)]);
    doc.type_at(1, 12, "\n    let pad = 0");
    let references = doc.at("textDocument/references", 6, 8);
    reference_starts("count", &references, &[(6, 8), (7, 11), (7, 19)]);
    doc.delete_at(2, 15, 16);
    let references = doc.at("textDocument/references", 5, 8);
    reference_starts("count", &references, &[(5, 8), (6, 11), (6, 19)]);
}
