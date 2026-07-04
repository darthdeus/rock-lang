//! The Rock tokenizer: `TokenKind` (+ keyword table and operator
//! binding-power tables), the `Token` struct, and the `Tokenizer`
//! itself. Builds only on `rock-core` (spans, diagnostics, compile
//! stats) — deliberately independent of the AST so a new parser/AST
//! can be built on top of it.

pub mod token;
pub mod tokenizer;

pub use token::*;
pub use tokenizer::*;

mod prelude;
