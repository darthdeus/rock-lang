use super::*;

#[test]
fn hover_iteratee_after_typing_for_loop() {
    let source = r#"struct AggressorEntry {
    dmg: int
}

struct Mob {
    aggressors: [AggressorEntry] = [],
}

fn f(m: Mob) {

}"#;
    let mut doc = Document::open(
        Project::single("hover_iteratee_after_typing_for_loop", source),
        source,
    );
    let reply = doc.request(
        "textDocument/hover",
        json!({"position": {"line": 8, "character": 5}}),
    );
    contains_text(
        reply["contents"]
            .as_str()
            .or_else(|| reply["contents"]["value"].as_str())
            .unwrap(),
        &["Mob"],
    );
    doc.type_at(
        9,
        4,
        r#"for a in m.aggressors {
    print(a.dmg)
}"#,
    );
    let reply = doc.request(
        "textDocument/hover",
        json!({"position": {"line": 9, "character": 18}}),
    );
    contains_text(
        reply["contents"]
            .as_str()
            .or_else(|| reply["contents"]["value"].as_str())
            .unwrap(),
        &["[AggressorEntry]"],
    );
}

#[test]
fn hover_new_for_binding_after_typing_sentinel() {
    let source = r#"struct AggressorEntry {
    dmg: int
}

struct Mob {
    aggressors: [AggressorEntry] = [],
}

fn f(m: Mob) {

}"#;
    let mut doc = Document::open(
        Project::single("hover_new_for_binding_after_typing_sentinel", source),
        source,
    );
    let reply = doc.request(
        "textDocument/hover",
        json!({"position": {"line": 8, "character": 5}}),
    );
    contains_text(
        reply["contents"]
            .as_str()
            .or_else(|| reply["contents"]["value"].as_str())
            .unwrap(),
        &["Mob"],
    );
    doc.type_at(
        9,
        4,
        r#"for a in m.aggressors {
    print(a.dmg)
}"#,
    );
    let reply = doc.request(
        "textDocument/hover",
        json!({"position": {"line": 9, "character": 8}}),
    );
    assert!(reply["contents"].is_null(), "{reply}");
}

#[test]
fn hover_for_binding_fresh() {
    let source = r#"struct AggressorEntry {
    dmg: int
}

struct Mob {
    aggressors: [AggressorEntry] = [],
}

fn f(m: Mob) {
    for a in m.aggressors {
        print(a.dmg)
    }
}"#;
    let mut doc = Document::open(Project::single("hover_for_binding_fresh", source), source);
    let reply = doc.request(
        "textDocument/hover",
        json!({"position": {"line": 9, "character": 8}}),
    );
    contains_text(
        reply["contents"]
            .as_str()
            .or_else(|| reply["contents"]["value"].as_str())
            .unwrap(),
        &["AggressorEntry"],
    );
}
