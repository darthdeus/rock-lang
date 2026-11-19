use crate::common::{body_id, edit, emit_v1_package, full_lua, run_luajit, ws_of};
use skull::mir_emit::emit_full;

#[test]
fn json_default_factory_updates_with_its_called_function() {
    let source =
        "fn initial() -> int { return 7 }\nstruct Row { @json_default value: int = initial() }";
    let after = ws_of(&source.replace("return 7", "return 13"));
    let edit = edit(&ws_of(source), &after, &[body_id(&after, "initial")]);
    let overlay = edit.overlay.expect("overlay");
    let read = "_G[_G.__ROCK_TYPE_TABLE.Row.fields[1].json_default]()";
    let script = format!(
        "{}\nassert({read} == 7)\n{}\nassert({read} == 13)\nprint('ok')",
        edit.base.lua, overlay.lua
    );
    assert_eq!(run_luajit(&script), "ok");
}

#[test]
fn json_default_factories_keep_module_scope() {
    let emitted = emit_v1_package(
        "jsondefaults",
        &[
            (
                "a.rock",
                "mod a;\nfn initial() -> int { return 7 }\nstruct Row { @json_default value: int = initial() }",
            ),
            (
                "b.rock",
                "mod b;\nfn initial() -> int { return 13 }\nstruct Row { @json_default value: int = initial() }",
            ),
        ],
    );
    let script = format!(
        "{}\nlocal values = {{}}\nfor _, decl in pairs(_G.__ROCK_TYPE_TABLE) do for _, field in ipairs(decl.fields or {{}}) do if field.json_default then values[#values + 1] = _G[field.json_default]() end end end\ntable.sort(values)\nassert(#values == 2 and values[1] == 7 and values[2] == 13)\nprint('ok')",
        emitted.lua
    );
    assert_eq!(run_luajit(&script), "ok");
}

#[test]
fn json_default_expression_edit_rebuilds_the_interface_and_factory() {
    use bone_incremental::session::{Session, SessionFile, UpdateTier};
    let source = "struct Row { @json_default value: int = 7 }";
    let mut session = Session::new(
        skull_parser::stdlib::cached().as_ref().unwrap().clone(),
        vec![SessionFile::standalone("json-defaults.rock", source)],
        &Default::default(),
    )
    .unwrap();
    let mid = session.workspace().module_ids[&ustr::Ustr::from("json-defaults.rock")];
    let updated = session
        .update_file("json-defaults.rock", &source.replace("= 7", "= 13"))
        .unwrap();
    assert_eq!(updated.tier, UpdateTier::Interface);
    let rebuilt = emit_full(session.workspace(), mid).unwrap().0;
    let script = format!(
        "{}\nassert(_G[_G.__ROCK_TYPE_TABLE.Row.fields[1].json_default]() == 13)\nprint('ok')",
        rebuilt.lua
    );
    assert_eq!(run_luajit(&script), "ok");
}

#[test]
fn json_default_factories_materialize_nobox_payloads_at_metadata_boundary() {
    let source = r#"
nobox value Point { x: float, y: float }
struct Row {
    @json_default point: Point = Point { x: 3.0, y: 4.0 },
    @json_skip cache: Point = Point { x: 5.0, y: 6.0 },
    @json_default optional: Point? = Point { x: 7.0, y: 8.0 },
}
"#;
    let ws = ws_of(source);
    let emitted_lua = full_lua(&ws);
    let script = format!(
        "{}\nlocal fields = _G.__ROCK_TYPE_TABLE.Row.fields\nfor i = 1, 3 do local p = _G[fields[i].json_default](); assert(p.x == i * 2 + 1 and p.y == i * 2 + 2) end\nprint('ok')",
        emitted_lua
    );
    assert_eq!(run_luajit(&script), "ok");
}

#[test]
fn json_default_factory_preserves_nested_value_return_abi() {
    let ws = ws_of(
        r#"
nobox value Point { x: float, y: float }
struct Row {
    @json_skip callback: fn() -> Point = () => Point { x: 3.0, y: 4.0 },
}
"#,
    );
    let emitted_lua = full_lua(&ws);
    let script = format!(
        "{}\nlocal callback = _G[_G.__ROCK_TYPE_TABLE.Row.fields[1].json_default](); local x, y = callback(); assert(x == 3 and y == 4)\nprint('ok')",
        emitted_lua
    );
    assert_eq!(run_luajit(&script), "ok");
}
