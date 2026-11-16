//! Fixture helpers shared by the GLSL test modules. "Checked" helpers first
//! refuse a fixture with per-body errors, as the CLI does; the others leave
//! such a fixture for the emitter to judge, which the rejection tests rely on.

use bone_core::prelude::ModuleId;
use bone_core::workspace::Workspace;
use skull_codegen_glsl::{GlslProfile, ShaderEmission, ShaderProgramSession};
use skull_graph::shader::ShaderProgram;
use skull_parser::Ast;
use skull_parser::CompilerError;
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

/// The registered-program session for `vert` (if any) and `frag`.
pub fn program_session(
    ws: &Workspace,
    mid: ModuleId,
    vert: Option<&str>,
    frag: &str,
) -> Result<ShaderProgramSession, CompilerError> {
    let vert = vert
        .map(|root| skull_codegen_glsl::resolve_root(ws, mid, root))
        .transpose()?;
    let frag = skull_codegen_glsl::resolve_root(ws, mid, frag)?;
    skull_codegen_glsl::shader_program_session(ws, ShaderProgram { vert, frag })
}

/// A program planned and rendered as the shader artifact view and emission do.
pub fn emit_program(
    ws: &Workspace,
    mid: ModuleId,
    vert: Option<&str>,
    frag: &str,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let session = program_session(ws, mid, vert, frag)?;
    let plan = skull_codegen_glsl::plan_shader_with_view(ws, &session.scx, &session.view)?;
    skull_codegen_glsl::render_shader_plan(ws, session.scx.interner(), &plan, profile)
}

/// The shader artifact view's profile check: the program planned, the plan
/// dropped.
pub fn check_program(
    ws: &Workspace,
    mid: ModuleId,
    vert: Option<&str>,
    frag: &str,
) -> Result<(), String> {
    let session = program_session(ws, mid, vert, frag).map_err(|e| e.message)?;
    skull_codegen_glsl::plan_shader_with_view(ws, &session.scx, &session.view)
        .map(|_| ())
        .map_err(|e| e.message)
}

/// Emission of a program the view kept no plan for: planned again from the
/// view, which must reach the check's verdict.
pub fn emit_unplanned(
    ws: &Workspace,
    mid: ModuleId,
    vert: Option<&str>,
    frag: &str,
) -> Result<(), String> {
    let session = program_session(ws, mid, vert, frag).map_err(|e| e.message)?;
    let profile = GlslProfile::Core330;
    match vert {
        Some(_) => skull_codegen_glsl::emit_shader_pipeline_with_view(
            ws,
            &session.scx,
            &session.view,
            profile,
        ),
        None => skull_codegen_glsl::emit_shader_entry_with_view(
            ws,
            &session.scx,
            &session.view,
            profile,
        ),
    }
    .map(|_| ())
    .map_err(|e| e.message)
}

pub fn emit_entry(src: &str, root: &str) -> Result<String, String> {
    entry_emission(src, root).map(|e| e.source)
}

pub fn emit_checked_entry(src: &str, root: &str) -> Result<String, String> {
    let (ws, mid) = workspace(src);
    body_errors(&ws)?;
    emit_program(&ws, mid, None, root, GlslProfile::Core330)
        .map(|e| e.source)
        .map_err(|e| e.message)
}

pub fn entry_emission(src: &str, root: &str) -> Result<ShaderEmission, String> {
    let (ws, mid) = workspace(src);
    emit_program(&ws, mid, None, root, GlslProfile::Core330).map_err(|e| e.message)
}

pub fn emit_pipeline(src: &str, vert: &str, frag: &str) -> Result<String, String> {
    pipeline_emission(src, vert, frag).map(|e| e.source)
}

pub fn pipeline_emission(src: &str, vert: &str, frag: &str) -> Result<ShaderEmission, String> {
    let (ws, mid) = workspace(src);
    emit_program(&ws, mid, Some(vert), frag, GlslProfile::Core330).map_err(|e| e.message)
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
