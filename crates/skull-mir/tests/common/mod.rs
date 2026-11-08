//! Workspace builders shared by this harness's tests.
use std::sync::LazyLock;

use ustr::Ustr;

/// The stdlib, parsed once and re-checked per call alongside each fixture.
pub static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// Run the semantic pass over the stdlib plus `src`.
pub fn ws_of(src: &str) -> bone_core::workspace::Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// [`ws_of`] for fixtures that must also type-check cleanly.
pub fn checked_ws_of(src: &str) -> bone_core::workspace::Workspace {
    let ws = ws_of(src);
    let errs = skull_mir::verdict::body_errors(&ws);
    assert!(errs.is_empty(), "fixture must type-check: {errs:?}");
    ws
}
