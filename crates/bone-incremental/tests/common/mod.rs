//! Comparator pieces shared by the incremental oracle and the modules-v1 differential harness.

use bone_semantic::prelude::*;

pub fn batch_check(
    warm: &[skull_parser::Ast],
    files: &[bone_incremental::session::SessionFile],
    aliases: &bone_core::modules::NsAliases,
) -> Result<Workspace, CompilerError> {
    let mut modules = warm.to_vec();
    for file in files {
        let mut ast = skull_parser::parse_module(&file.text, Ustr::from(file.path.as_str()))?;
        // `Session::new` stamps these same facts on every parse.
        ast.package = file.package.clone();
        ast.global_policy = file.global_policy;
        modules.push(ast);
    }
    semantic_pass_with_aliases(&mut modules, aliases)
}

pub fn mid_for(ws: &Workspace, path: &str) -> ModuleId {
    ws.module_ids[&Ustr::from(path)]
}

/// Rendered type at every `stride`-th byte offset of `path` (None where `type_at` has no node).
pub fn type_profile(ws: &Workspace, path: &str, text: &str, stride: usize) -> Vec<Option<String>> {
    let mid = mid_for(ws, path);
    let Some(inf) = ws.inference(mid) else {
        return vec![None; text.len().div_ceil(stride)];
    };
    (0..text.len())
        .step_by(stride)
        .map(|offset| {
            bone_semantic::query::type_at(ws, mid, inf, offset).map(|ta| ws.render(ta.body, ta.ty))
        })
        .collect()
}

pub const PACKAGE: &str = "livev1";

pub fn package_tag(id: u32, name: &str) -> skull_parser::PackageTag {
    skull_parser::PackageTag {
        id,
        self_name: name.into(),
        identity_storage: Default::default(),
        root: String::new(),
    }
}

pub fn in_package(
    package: u32,
    name: &str,
    path: &str,
    text: &str,
) -> bone_incremental::session::SessionFile {
    bone_incremental::session::SessionFile {
        path: path.into(),
        text: text.into(),
        package: Some(package_tag(package, name)),
        global_policy: skull_parser::GlobalPolicy::Any,
    }
}

pub fn v1(path: &str, text: &str) -> bone_incremental::session::SessionFile {
    in_package(1, PACKAGE, path, text)
}

/// Single-package fixtures share the same package facts on every parse.
pub fn files(list: &[(&str, &str)]) -> Vec<bone_incremental::session::SessionFile> {
    list.iter().map(|(path, text)| v1(path, text)).collect()
}

pub fn session(list: &[(&str, &str)]) -> bone_incremental::Session {
    bone_incremental::Session::new(vec![], files(list), &Default::default())
        .expect("session builds")
}

#[track_caller]
pub fn with_warm(
    warm: Vec<skull_parser::Ast>,
    files: Vec<bone_incremental::session::SessionFile>,
) -> bone_incremental::Session {
    bone_incremental::Session::new(warm, files, &Default::default()).expect("session builds")
}

#[track_caller]
pub fn assert_clean(session: &bone_incremental::Session) {
    assert!(
        session.diagnostics().is_empty(),
        "unexpected diagnostics: {:?}",
        session.diagnostics()
    );
}
