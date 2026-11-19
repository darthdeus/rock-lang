//! Arena-allocated surface AST and parser, built on `rock-tokenizer`
//! (lexer) and `rock-core` (spans, diagnostics).

pub mod ast;
pub mod desugar;
pub(crate) mod graft;
pub mod macro_expand;
pub mod parser;
pub mod stdlib;
pub(crate) mod walk;

pub use ast::*;
pub(crate) use parser::Parser;

pub use ordered_float::OrderedFloat;
pub use rock_core::errors::CompilerErrorKind;
pub use rock_core::source_code::{GlobalPolicy, IdentityStorage, PackageTag};
pub use rock_core::{CompilerError, SourceFiles, Span, source_file_from_path_and_contents};
pub use rock_tokenizer::TokenKind;
use rock_tokenizer::{Token, Tokenizer};
use ustr::Ustr;

pub fn parse_module(source: &str, file: Ustr) -> Result<Ast, CompilerError> {
    parse_module_with_tokens(source, file, |_| ()).map(|(ast, ())| ast)
}

/// Observe the module's lexer output before the parser consumes it.
pub fn parse_module_with_tokens<T>(
    source: &str,
    file: Ustr,
    inspect: impl FnOnce(&[Token]) -> T,
) -> Result<(Ast, T), CompilerError> {
    let tokens = Tokenizer::new(source, file).tokenize()?;
    let facts = inspect(&tokens);
    let mut ast = Parser::new(tokens, source, 0).parse_module()?;
    ast.file = file;
    Ok((ast, facts))
}

pub fn parse_module_clean(source: &str, file: Ustr) -> Result<Ast, CompilerError> {
    let ast = parse_module(source, file)?;
    match ast.recovery_errors.first() {
        Some(e) => Err(e.clone()),
        None => Ok(ast),
    }
}

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
