//! Is a checked workspace actually clean? `semantic_pass` returning `Ok` means
//! "lowered and checked", NOT "no type errors" -- the errors are recorded per
//! body. Every verdict-shaped consumer (the CLI, test harnesses) must ask here,
//! or it counts broken bodies as passing.

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::CompilerError;

/// All body-level type errors from a checked workspace, in source order
/// (`(file, offset)` -- bone stores them per `BodyOwner` in hash maps, so map
/// iteration alone is nondeterministic).
///
/// Per-body type errors PLUS lowering errors (which live on the module, not the
/// inference -- e.g. `_` placeholder inside an explicit lambda/block) PLUS
/// interface-position type errors (an unknown / wrong-arity type in a signature
/// / global / field). Lowering is TOTAL: such a reference lowers to `Error` + a
/// recorded `interface_errors` entry rather than aborting the pass, so a verdict
/// ignoring `interface_errors` would accept `fn f(x: Nonexistent)` and let skull
/// emit Lua over the error type. A body OR interface that fails to lower is a
/// failure, not a silent pass.
pub fn body_errors(ws: &Workspace) -> Vec<CompilerError> {
    let mut errs: Vec<CompilerError> = ws
        .inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten().cloned())
        .chain(ws.modules.iter().flat_map(|m| m.lowering_errors.values().flatten().cloned()))
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter().cloned()))
        .collect();
    errs.sort_by(|a, b| {
        (a.location.file.as_str(), a.location.offset)
            .cmp(&(b.location.file.as_str(), b.location.offset))
    });
    errs
}
