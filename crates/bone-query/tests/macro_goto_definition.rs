//! Goto-definition on a macro invocation name jumps to the `macro` decl, not the expansion.

use crate::fixture::pos;
use bone_query::BoneWorkspace;

fn probe_path(name: &str) -> String {
    if cfg!(windows) {
        format!("C:/probe/{name}")
    } else {
        format!("/probe/{name}")
    }
}

#[test]
fn def_macro_invocation_jumps_to_the_macro_decl_cross_file() {
    let (def_path, use_path) = (probe_path("def.rock"), probe_path("use.rock"));
    let use_src = "fn main() {\n    twice(41)\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(
        &def_path,
        "macro twice(x: expr) {\n    let _ = x + x\n}\n".into(),
    );
    bone.update(&use_path, use_src.into());
    let loc = bone
        .definition(&use_path, pos(use_src, "twice(41)", 2))
        .expect("goto-def on the macro invocation name");
    assert!(
        loc.uri.as_str().ends_with("def.rock"),
        "{}",
        loc.uri.as_str()
    );
    assert_eq!((loc.range.start.line, loc.range.start.character), (0, 6));
}

#[test]
fn macro_definition_after_a_body_edit_matches_a_fresh_workspace() {
    let path = probe_path("main.rock");
    let before = "fn earlier() -> int {\n    1\n}\nmacro twice(x: expr) { let _ = x + x }\nfn main() { twice(41) }\n";
    let after = before.replace("    1", "    let one = 1\n    one");
    let query = |bone: &mut BoneWorkspace, source: &str| {
        bone.update(&path, source.into());
        bone.definition(&path, pos(source, "twice(41)", 2)).unwrap()
    };
    let mut warm = BoneWorkspace::new();
    assert_eq!(query(&mut warm, before).range.start.line, 3);
    let fresh = query(&mut BoneWorkspace::new(), &after);
    assert_eq!(fresh.range.start.line, 4);
    assert_eq!(query(&mut warm, &after), fresh);
}
