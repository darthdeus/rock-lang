//! The fixture workspace, shared by the integration tests and the crate's
//! unit tests.

use bone_core::prelude::ModuleId;
use bone_core::workspace::Workspace;
use skull_parser::Ast;
use ustr::Ustr;

pub fn stdlib() -> &'static [Ast] {
    skull_parser::stdlib::cached()
        .as_ref()
        .expect("stdlib parse")
}

pub fn workspace_with(stdlib: &[Ast], src: &str) -> (Workspace, ModuleId) {
    let mut modules = stdlib.to_vec();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    (ws, mid)
}

pub fn workspace(src: &str) -> (Workspace, ModuleId) {
    workspace_with(stdlib(), src)
}
