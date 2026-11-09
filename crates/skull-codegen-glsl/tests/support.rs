//! Fixture helpers shared by the GLSL test modules. "Checked" helpers first
//! refuse a fixture with per-body errors, as the CLI does; the others leave
//! such a fixture for the emitter to judge, which the rejection tests rely on.

use bone_core::prelude::ModuleId;
use bone_core::workspace::Workspace;
use skull_codegen_glsl::{GlslProfile, ShaderEmission};
use skull_parser::Ast;
use std::sync::LazyLock;
use ustr::Ustr;

pub static STDLIB: LazyLock<Vec<Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn workspace_with(stdlib: &[Ast], src: &str) -> (Workspace, ModuleId) {
    let mut modules = stdlib.to_vec();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    (ws, mid)
}

pub fn workspace(src: &str) -> (Workspace, ModuleId) {
    workspace_with(&STDLIB, src)
}

/// `semantic_pass` returns Ok with per-body errors recorded in `ws.errors`; the
/// CLI reports those and refuses to emit.
fn body_errors(ws: &Workspace) -> Result<(), String> {
    let errs = skull_mir::verdict::body_errors(ws);
    if errs.is_empty() {
        return Ok(());
    }
    Err(errs
        .iter()
        .map(|e| e.message.clone())
        .collect::<Vec<_>>()
        .join("; "))
}

/// [`workspace`], panicking on a fixture the real pipeline would reject.
pub fn checked_workspace(src: &str) -> (Workspace, ModuleId) {
    let (ws, mid) = workspace(src);
    if let Err(errs) = body_errors(&ws) {
        panic!("fixture has body errors: {errs}");
    }
    (ws, mid)
}

fn library(ws: &Workspace, mid: ModuleId, roots: &[&str]) -> Result<String, String> {
    let root_refs = roots
        .iter()
        .map(|root| skull_codegen_glsl::resolve_root(ws, mid, root))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.message)?;
    let session =
        skull_codegen_glsl::shader_library_session(ws, root_refs).map_err(|e| e.message)?;
    skull_codegen_glsl::emit_shader_library_with_view(ws, &session.scx, &session.view)
        .map_err(|e| e.message)
}

pub fn emit_library_unchecked(src: &str, roots: &[&str]) -> Result<String, String> {
    let (ws, mid) = workspace(src);
    library(&ws, mid, roots)
}

pub fn emit_library_with(stdlib: &[Ast], src: &str, roots: &[&str]) -> Result<String, String> {
    let (ws, mid) = workspace_with(stdlib, src);
    body_errors(&ws)?;
    library(&ws, mid, roots)
}

pub fn emit_library(src: &str, roots: &[&str]) -> Result<String, String> {
    emit_library_with(&STDLIB, src, roots)
}

pub fn emit_entry(src: &str, root: &str) -> Result<String, String> {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_entry(&ws, mid, root, GlslProfile::Core330)
        .map_err(|e| e.message)
}

pub fn emit_checked_entry(src: &str, root: &str) -> Result<String, String> {
    let (ws, mid) = workspace(src);
    body_errors(&ws)?;
    skull_codegen_glsl::emit_shader_entry(&ws, mid, root, GlslProfile::Core330)
        .map_err(|e| e.message)
}

pub fn entry_emission(src: &str, root: &str) -> Result<ShaderEmission, String> {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_entry_with_dependencies(&ws, mid, root, GlslProfile::Core330)
        .map_err(|e| e.message)
}

pub fn emit_pipeline(src: &str, vert: &str, frag: &str) -> Result<String, String> {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_pipeline(&ws, mid, vert, frag, GlslProfile::Core330)
        .map_err(|e| e.message)
}

pub fn pipeline_emission(src: &str, vert: &str, frag: &str) -> Result<ShaderEmission, String> {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
        &ws,
        mid,
        vert,
        frag,
        GlslProfile::Core330,
    )
    .map_err(|e| e.message)
}

/// The emitted shader contains `needle`.
#[track_caller]
pub fn assert_has(glsl: &str, needle: &str) {
    assert!(glsl.contains(needle), "missing `{needle}` in:\n{glsl}");
}

/// The emitted shader does not contain `needle`.
#[track_caller]
pub fn assert_lacks(glsl: &str, needle: &str) {
    assert!(!glsl.contains(needle), "unexpected `{needle}` in:\n{glsl}");
}

/// One `#[test]` per case, each calling `$check` with that case's arguments, so
/// a family of fixture-and-expectation tests reads as a table but still runs
/// and reports by name.
macro_rules! cases {
    ($check:ident: $($(#[$attr:meta])* $name:ident($($arg:expr),* $(,)?);)*) => {
        $($(#[$attr])* #[test] fn $name() { $check($($arg),*); })*
    };
}
