use crate::common::run_luajit;
use skull::mir_emit::emit_full;

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
