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
pub use rock_core::source_code::{Dialect, GlobalPolicy, IdentityStorage, PackageTag};
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
    parse_module_with_dialect(source, file, Dialect::V1)
}

/// [`parse_module`] with a compatibility dialect argument. The parser uses
/// the modules-v1 grammar for every source.
pub fn parse_module_with_dialect(
    source: &str,
    file: Ustr,
    _dialect: Dialect,
) -> Result<Ast, CompilerError> {
    if let Some(span) = removed_legacy_directive_span(source, file) {
        return Err(CompilerError::new_with_label(
            span,
            "the `//! modules: legacy` directive has been removed; delete it".to_string(),
        ));
    }
    let tokens = Tokenizer::new(source, file).tokenize()?;
    let mut ast = Parser::new(tokens, source, 0).parse_module()?;
    ast.file = file;
    Ok(ast)
}

fn removed_legacy_directive_span(source: &str, file: Ustr) -> Option<Span> {
    let mut offset = 0;
    for (line_number, line) in source.split_inclusive('\n').take(10).enumerate() {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        let trimmed = content.trim_start();
        if let Some(rest) = trimmed.strip_prefix("//!") {
            let rest = rest.trim();
            if rest
                .strip_prefix("modules:")
                .is_some_and(|value| value.trim() == "legacy")
            {
                let leading_bytes = content.len() - trimmed.len();
                let leading_chars = content[..leading_bytes].chars().count();
                return Some(Span::new(
                    file,
                    (line_number, line_number),
                    (leading_chars, content.chars().count()),
                    (offset + leading_bytes, offset + content.len()),
                ));
            }
        }
        offset += line.len();
    }
    None
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
    parse_module_clean_with_dialect(source, file, Dialect::V1)
}

/// [`parse_module_clean`] with a compatibility dialect argument. The parser
/// uses the modules-v1 grammar for every source.
pub fn parse_module_clean_with_dialect(
    source: &str,
    file: Ustr,
    dialect: Dialect,
) -> Result<Ast, CompilerError> {
    let ast = parse_module_with_dialect(source, file, dialect)?;
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
/// `dialect` is retained for compatibility; the parser always uses modules-v1.
pub fn parse_one_item(
    source: &str,
    file: Ustr,
    offset: usize,
    line: usize,
    column: usize,
    _dialect: Dialect,
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
