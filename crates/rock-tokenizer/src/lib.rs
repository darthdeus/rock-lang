//! The Rock tokenizer: `TokenKind` (+ keyword table and operator
//! binding-power tables), the `Token` struct, and the `Tokenizer`
//! itself. Builds only on `rock-core` (spans, diagnostics, compile
//! stats) — deliberately independent of the AST so a new parser/AST
//! can be built on top of it.

pub mod interp;
pub mod lexical_regions;
pub mod token;
pub mod tokenizer;

pub use interp::{Flavor, Piece, Scan, scan_string};
pub use lexical_regions::{LexRegion, region_at};
pub use token::*;
pub use tokenizer::*;

mod prelude;
