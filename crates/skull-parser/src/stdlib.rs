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
