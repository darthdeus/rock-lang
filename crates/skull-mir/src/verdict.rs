//! `semantic_pass` returning `Ok` does not mean no type errors: they are
//! recorded per body, so every verdict-shaped consumer must ask here.
use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;

/// In source order, since bone stores errors in hash maps. Includes lowering and
/// interface-position errors: lowering is total, so ignoring them would accept
/// `fn f(x: Nonexistent)`.
pub fn body_errors(ws: &Workspace) -> Vec<CompilerError> {
    let mut errs: Vec<CompilerError> = ws
        .inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten().cloned())
        .chain(
            ws.modules
                .iter()
                .flat_map(|m| m.lowering_errors.values().flatten().cloned()),
        )
        .chain(
            ws.modules
                .iter()
                .flat_map(|m| m.interface_errors.iter().cloned()),
        )
        .collect();
    errs.sort_by(|a, b| {
        (a.location.file.as_str(), a.location.offset)
            .cmp(&(b.location.file.as_str(), b.location.offset))
    });
    errs
}
