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

#[test]
fn edited_callback_forwarding_rebuilds_callers_and_preserves_other_slots() {
    use crate::common::{body_id, edit, function_body, ws_with_stdlib};

    let source = r#"
global SAVED: fn() -> int = () => 0
global OTHER: fn() -> int = () => 0
fn leaf(f: fn() -> int) -> int { SAVED = f; return f() }
fn forward(f: fn() -> int) -> int { return leaf(f) }
@nomangle fn install(x: int) -> int {
    let result = forward(() => x + 1)
    OTHER = () => x + 2
    return result
}
fn main() { install(5); print(SAVED(), OTHER()) }
"#;
    let escaped = ws_with_stdlib(source);
    let synchronous = ws_with_stdlib(&source.replace("SAVED = f; return f()", "return f()"));
    let edit_to = |from, to| edit(from, to, &[body_id(to, "leaf")]);
    let to_synchronous = edit_to(&escaped, &synchronous);
    let to_escaped = edit_to(&synchronous, &escaped);
    for update in [&to_synchronous, &to_escaped] {
        let before = function_body(&update.base.lua, "install");
        let after = function_body(&update.incremental.lua, "install");
        assert_ne!(before, after);
    }
    let escaped_body = function_body(&to_synchronous.base.lua, "install");
    let synchronous_body = function_body(&to_synchronous.incremental.lua, "install");
    let code_slots = |body: &str| body.matches("__rock_code_slot_ref(").count();
    assert_eq!(code_slots(escaped_body), 2);
    assert_eq!(code_slots(synchronous_body), 1);
    let other_slot = escaped_body
        .lines()
        .filter_map(|line| line.split("__rock_code_slot_ref(\"").nth(1))
        .next_back()
        .and_then(|rest| rest.split('"').next())
        .expect("the escaping sibling has a code slot");
    assert!(synchronous_body.contains(other_slot));
}

#[test]
fn recursive_callback_lift_rebuilds_the_caller_and_keeps_its_sibling_slot() {
    use crate::common::{body_id, edit, function_body, run_luajit, ws_with_stdlib};

    let source = r#"
global saved: fn() -> int = () => 0

fn follow(n: int, f: fn() -> int) -> int {
    if n == 0 { return f() }
    return follow(n - 1, f)
}

@nomangle fn install(x: int) -> int {
    let result = follow(3, () => x + 1)
    saved = () => x + 10
    return result
}

@nomangle fn read_saved() -> fn() -> int { return saved }
fn main() { let _ = install(0); let _ = read_saved() }
"#;
    let before = ws_with_stdlib(source);
    let changed = ws_with_stdlib(&source.replace("return f()", "return f() + 100"));
    let update = edit(&before, &changed, &[body_id(&changed, "follow")]);
    let overlay = update.overlay.expect("compatible recursive body edit");
    let install = function_body(&update.base.lua, "install");
    assert_eq!(install.matches("__rock_code_slot_ref(").count(), 1);
    assert!(install.contains("__rock_lifted_callback_"));
    assert_ne!(install, function_body(&update.incremental.lua, "install"));
    let steps = format!(
        "{}\nassert(install(2) == 3)\nlocal old = read_saved()\n{}\nassert(install(2) == 103 and old() == 12)\nprint('ok')",
        update.base.lua, overlay.lua
    );
    assert_eq!(run_luajit(&steps), "ok");

    let escaping = ws_with_stdlib(&source.replace("return f()", "saved = f; return f()"));
    let escaped = edit(&before, &escaping, &[body_id(&escaping, "follow")]);
    let install = function_body(&escaped.incremental.lua, "install");
    assert_eq!(install.matches("__rock_code_slot_ref(").count(), 2);
    assert!(!install.contains("__rock_lifted_callback_"));
}

#[test]
fn recursive_lift_keeps_boxed_delivery_when_the_lua_wire_is_too_wide() {
    use crate::common::{run_main, try_emit_module};

    for (parameter_count, capture_count) in [(115, 95), (115, 8)] {
        let parameters: Vec<_> = (0..parameter_count)
            .map(|index| format!("a{index}"))
            .collect();
        let arguments = parameters.join(", ");
        let types = parameters
            .iter()
            .map(|name| format!("{name}: int"))
            .collect::<Vec<_>>()
            .join(", ");
        let zeroes = std::iter::repeat_n("0", parameters.len())
            .collect::<Vec<_>>()
            .join(", ");
        let captures: Vec<_> = (0..capture_count)
            .map(|index| format!("v{index}"))
            .collect();
        let declarations = captures
            .iter()
            .enumerate()
            .map(|(index, name)| format!("let {name} = value + {index}\n"))
            .collect::<String>();
        let sum = captures.join(" + ");
        let src = format!(
            r#"
fn forward(n: int, callback: fn() -> int, {types}) -> int {{
    if n == 0 {{ return callback() + a0 }}
    return forward(n - 1, callback, {arguments})
}}
fn invoke(value: int) -> int {{
    {declarations}
    return forward(3, () => {sum}, {zeroes})
}}
fn main() {{ print(invoke(2)) }}
"#
        );
        let lua = try_emit_module(&src).expect("the ordinary closure fits LuaJIT's wire");
        let expected = (2..2 + captures.len()).sum::<usize>().to_string();
        assert_eq!(run_main(&lua), expected);
    }
}
