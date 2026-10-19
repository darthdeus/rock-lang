//! A host without a filesystem installs the stdlib from memory. The editor
//! must then name stdlib files by the host's display path, never by the
//! checkout the compiler was built from.

use bone_query::BoneWorkspace;
use lsp_types::Position;

#[test]
fn installed_stdlib_is_named_by_its_display_path() {
    let dir = rock_stdlib::stdlib_dir();
    let files = rock_stdlib::STDLIB_NAMES.iter().map(|name| {
        (name.to_string(), std::fs::read_to_string(dir.join(name)).expect("stdlib readable"))
    });
    rock_stdlib::install_stdlib("/stdlib", files).expect("first install");

    // A scratch buffer with no file on disk, as a browser host has.
    let main = "/scratch/main.rock";
    let src = "fn main() -> int {\n  let xs = [1]\n  xs.push(2)\n  xs.len()\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(main, src.to_string());
    let def = bone.definition(main, Position::new(2, 5)).expect("push resolves");
    assert_eq!(def.uri.as_str(), "file:///stdlib/unified.rock");
}
