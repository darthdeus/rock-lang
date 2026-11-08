//! The parsed stdlib: the one loader the CLI pipeline and the LSP session both
//! warm from, so they cannot disagree about what a broken stdlib means.
//!
//! The stdlib has no recovery story -- a dropped item would surface later as
//! "callee not found" in user code -- so any syntax error, recovered or not,
//! fails the whole load with an error naming the file.

use std::sync::LazyLock;

use rock_core::CompilerError;
use rock_stdlib::StdlibFile;
use ustr::Ustr;

use crate::Ast;

/// The process's stdlib ([`rock_stdlib::stdlib_files`]) parsed in load order
/// under the stdlib session ids. Parsed once.
pub fn cached() -> &'static Result<Vec<Ast>, CompilerError> {
    static STDLIB: LazyLock<Result<Vec<Ast>, CompilerError>> =
        LazyLock::new(|| parse(rock_stdlib::stdlib_files()));
    &STDLIB
}

/// An owned copy of [`cached`], for a compile that consumes its modules.
pub fn modules() -> Result<Vec<Ast>, CompilerError> {
    cached().clone()
}

/// Whether `modules` begin with every module of [`cached`], each exactly as
/// parsed: same file, facts and tree, and no recorded error. A cache over the
/// stdlib's checked form may stand in for such a prefix; one a caller edited
/// or expanded is compared, not trusted by its file id.
pub fn is_prefix_of(modules: &[Ast]) -> bool {
    let Ok(stdlib) = cached() else {
        return false;
    };
    modules.len() >= stdlib.len()
        && stdlib
            .iter()
            .zip(modules)
            .all(|(parsed, given)| same_parse(parsed, given))
}

/// Structural equality of two clean parses.
fn same_parse(a: &Ast, b: &Ast) -> bool {
    // Destructured so a new field must be considered here.
    let Ast {
        file,
        dialect,
        package,
        module_header,
        global_policy,
        exprs,
        stmts,
        blocks,
        types,
        items,
        recovery_errors,
        macro_param_refs,
    } = a;
    *file == b.file
        && *dialect == b.dialect
        && *package == b.package
        && *module_header == b.module_header
        && *global_policy == b.global_policy
        && recovery_errors.is_empty()
        && b.recovery_errors.is_empty()
        && macro_param_refs.len() == b.macro_param_refs.len()
        && macro_param_refs
            .iter()
            .zip(&b.macro_param_refs)
            .all(|(x, y)| x.name == y.name && x.spans == y.spans)
        && *items == b.items
        && *exprs == b.exprs
        && *stmts == b.stmts
        && *blocks == b.blocks
        && *types == b.types
}

/// Parse `files` as a stdlib. The first file that does not parse cleanly is
/// the error.
pub fn parse(files: &[StdlibFile]) -> Result<Vec<Ast>, CompilerError> {
    files
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            crate::parse_module_clean(f.contents, id).map_err(|e| {
                let mut error = e.clone();
                error.message =
                    format!("stdlib `{}` does not parse: {}", f.display_path, e.message);
                error
            })
        })
        .collect()
}
