#[test]
fn lambda_slot_keys_do_not_spell_the_source_path() {
    let src = r#"
global KEEP: fn() -> int = () => 0
fn make(x: int) -> fn() -> int {
    fn helper() -> int { return 2 }
    return () => x + helper()
}
fn main() { KEEP = make(1); print(KEEP()) }
"#;
    let lua = crate::common::try_emit_module_with_file(
        src,
        "/checkout/game/main.rock",
        Default::default(),
    )
    .expect("emit");
    let keys: Vec<&str> = lua
        .split("__rock_code_slot_ref(\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter(|key| key.starts_with("lambda:") || key.starts_with("nested:"))
        .collect();
    assert!(!keys.is_empty());
    for key in keys {
        let site = key.rsplit_once('@').expect("site").1;
        assert!(!site.contains("/checkout/"));
    }
}

#[test]
fn trivia_and_sibling_body_edits_preserve_closure_slots() {
    use crate::common::{body_id, edit, ws_with_stdlib};

    let source = r#"
global FIRST: fn() -> int = () => 0
global SECOND: fn() -> int = () => 0
@nomangle fn install(x: int) {
    fn helper() -> int { return 2 }
    FIRST = () => x + helper()
    SECOND = () => x + 3
}
@nomangle fn first() -> fn() -> int { FIRST }
@nomangle fn second() -> fn() -> int { SECOND }
fn main() { install(10); print(first()()); print(second()()) }
"#;
    let before = ws_with_stdlib(source);
    let trivia = ws_with_stdlib(&format!(
        "// Comment above the declarations\n\n{}",
        source
            .replace("    FIRST", "    // First callback\n\n    FIRST")
            .replace("    SECOND", "\n    // Second callback\n    SECOND")
    ));
    let check = |after| edit(&before, after, &[body_id(after, "install")]);
    let comment_edit = check(&trivia);
    let overlay = comment_edit
        .overlay
        .expect("trivia overlay must be compatible");
    assert_eq!(comment_edit.base.lua, comment_edit.incremental.lua);
    assert!(!overlay.runtime_changed);
    for (identity, entry) in comment_edit
        .base
        .code_artifact
        .entries
        .iter()
        .filter(|(_, entry)| entry.escaping)
    {
        assert_eq!(overlay.code_artifact.entries.get(identity), Some(entry));
    }

    let reordered = ws_with_stdlib(&source.replace(
        "    FIRST = () => x + helper()\n    SECOND = () => x + 3",
        "    SECOND = () => x + 3\n    FIRST = () => x + helper()",
    ));
    assert!(
        check(&reordered).overlay.is_err(),
        "moved closures need a restart"
    );

    let changed = ws_with_stdlib(&source.replace("x + helper()", "x + helper() + 100"));
    let body_edit = check(&changed);
    let overlay = body_edit
        .overlay
        .expect("sibling keys must stay compatible");
    let escaping_closures = |emit: &skull::mir_emit::MirEmit| {
        use skull_mir::code_identity::CodeIdentity::{Lambda, Nested};
        emit.code_artifact
            .entries
            .iter()
            .filter(|(key, entry)| entry.escaping && matches!(key, Lambda { .. } | Nested { .. }))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        escaping_closures(&body_edit.base),
        escaping_closures(&overlay)
    );
    let steps = format!(
        "{}\ninstall(10)\nlocal first, second = first(), second()\n{}\nassert(first() == 112 and second() == 13)\nprint('ok')",
        body_edit.base.lua, overlay.lua
    );
    assert_eq!(crate::common::run_luajit(&steps), "ok");
}
