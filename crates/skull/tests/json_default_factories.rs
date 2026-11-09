use crate::common::{V1Fragment, emit_v1_package, run_luajit, ws_of};
use bone_semantic::hir::BodyOwner;
use skull::mir_emit::{emit_full, emit_incremental, emit_overlay};

#[test]
fn json_default_factory_updates_with_its_called_function() {
    let source =
        "fn initial() -> int { return 7 }\nstruct Row { @json_default value: int = initial() }";
    let before = ws_of(source);
    let mid = skull::user_module_id(&before).unwrap();
    let (full, mut incremental_cache) = emit_full(&before, mid).unwrap();
    let (_, mut overlay_cache) = emit_full(&before, mid).unwrap();
    let after = ws_of(&source.replace("return 7", "return 13"));
    let id = after
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "initial")
        .unwrap()
        .0;
    let edited = [(mid, BodyOwner::Function(id))];
    let incremental = emit_incremental(&after, &mut incremental_cache, &edited).unwrap();
    assert_eq!(incremental.lua, emit_full(&after, mid).unwrap().0.lua);
    let overlay = emit_overlay(&after, &mut overlay_cache, &edited).unwrap();
    let read = "_G[_G.__ROCK_TYPE_TABLE.Row.fields[1].json_default]()";
    let script = format!(
        "{}\nassert({read} == 7)\n{}\nassert({read} == 13)\nprint('ok')",
        full.lua, overlay.lua
    );
    assert_eq!(run_luajit(&script), "ok");
}

#[test]
fn json_default_factories_keep_module_scope() {
    let emitted = emit_v1_package(
        "jsondefaults",
        &[
            V1Fragment {
                file: "a.rock",
                src: "mod a;\nfn initial() -> int { return 7 }\nstruct Row { @json_default value: int = initial() }",
            },
            V1Fragment {
                file: "b.rock",
                src: "mod b;\nfn initial() -> int { return 13 }\nstruct Row { @json_default value: int = initial() }",
            },
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
        vec![SessionFile::legacy("json-defaults.rock", source)],
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
