//! Arena-allocated surface AST and parser, built on `rock-tokenizer`
//! (lexer) and `rock-core` (spans, diagnostics).

pub mod ast;
pub mod desugar;
pub(crate) mod graft;
pub mod macro_expand;
pub mod parser;
pub mod stdlib;

pub use ast::*;
pub(crate) use parser::Parser;

pub use ordered_float::OrderedFloat;
pub use rock_core::errors::CompilerErrorKind;
pub use rock_core::source_code::{GlobalPolicy, IdentityStorage, PackageTag};
pub use rock_core::{CompilerError, SourceFiles, Span, source_file_from_path_and_contents};
pub use rock_tokenizer::TokenKind;
use rock_tokenizer::Tokenizer;
use ustr::Ustr;

/// Tokenize `source` with rock's tokenizer and parse it into an arena `Ast`.
/// `file` is the source-file identity used for spans/diagnostics.
///
/// The parse is TOTAL: error recovery (for partial parses) records the dropped
/// spans in `Ast.recovery_errors` and the best-effort tree is returned `Ok`
/// regardless (rust-analyzer's model -- the tree always comes back). Only an
/// unrecoverable tokenizer failure is a hard `Err`. Strictness is a DOWNSTREAM
/// policy, not the parser's job: the CLI compile/test paths gate on
/// `recovery_errors` (see `rock-cli` `check_modules` / `parse_user_modules`),
/// while the interactive `Session`/LSP keep the partial tree so every feature
/// (completion, hover, rename, inlay) works on broken code and the recovery
/// errors surface as diagnostics.
pub fn parse_module(source: &str, file: Ustr) -> Result<Ast, CompilerError> {
    rock_core::source_code::validate_modules_directive(source, file)?;
    let tokens = Tokenizer::new(source, file).tokenize()?;
    let mut ast = Parser::new(tokens, source, 0).parse_module()?;
    ast.file = file;
    Ok(ast)
}

/// [`parse_module`] for a caller that has NO recovery story: a recovered syntax
/// error is an `Err` here rather than a dropped item in an otherwise-`Ok` tree.
///
/// Test fixtures and corpus tools want this one. `parse_module(..).expect(..)`
/// reads like it proves the source parsed, and does not -- a malformed item is
/// silently absent from the AST, so the test runs against a partial workspace
/// and passes for a reason that has nothing to do with its name. That is how a
/// `""" .. """` inline body missing its `=` sat in the JS emitter fixtures:
/// accepted, dropped, and the rejection under test came from a different path.
pub fn parse_module_clean(source: &str, file: Ustr) -> Result<Ast, CompilerError> {
    let ast = parse_module(source, file)?;
    match ast.recovery_errors.first() {
        Some(e) => Err(e.clone()),
        None => Ok(ast),
    }
}

/// Parse exactly ONE top-level item from `source` (the text of a single item),
/// with spans anchored at `(offset, line, column)` -- the absolute position of
/// `source[0]` within the outer file. Used by the incremental driver to re-parse
/// just an edited item instead of the whole file: extract the item's text by its
/// stored span, parse it here, and the resulting AST's spans line up with the
/// rest of the file. `line`/`column` are 0-based (rock-core `Span` convention).
pub fn parse_one_item(
    source: &str,
    file: Ustr,
    offset: usize,
    line: usize,
    column: usize,
) -> Result<Ast, CompilerError> {
    let tokens = Tokenizer::new_with_offset(source, file, offset, line, column).tokenize()?;
    let mut ast = Parser::new(tokens, source, offset).parse_single_item()?;
    ast.file = file;
    // Total, like `parse_module`. The incremental fast path inspects
    // `recovery_errors` itself: a body-level recovery (e.g. a stray `)` inside the
    // edited item) makes it bail to a full reparse rather than installing a
    // half-parsed item -- a caller policy, not a parse failure.
    Ok(ast)
}

/// Expand and synthesize one module using the workspace's macro registry.
/// Existing parse diagnostics remain available; newly reported expansion errors fail cooking.
pub fn cook_one(
    ast: &mut Ast,
    registry: &macro_expand::MacroRegistry,
) -> Result<(), CompilerError> {
    let errors_before = ast.recovery_errors.len();
    macro_expand::expand_one(ast, registry);
    if let Some(error) = ast.recovery_errors.get(errors_before) {
        return Err(error.clone());
    }
    desugar::desugar(ast);
    Ok(())
}

pub use macro_expand::{
    MacroRegistry, check_macro_name_uniqueness, collect_registry, is_hygiene_name, macro_reach,
};

#[cfg(test)]
mod tests;
