//! Foundation crate for the rock compiler: source positions (`Span`,
//! `LineCol`), diagnostics (`CompilerError`), and module ids.
//! Deliberately holds nothing AST-shaped — the parser (`skull-parser`)
//! and `rock-tokenizer` both build on top of this.

pub mod clock;
pub mod errors;
pub mod nesting;
pub mod package_aliases;
pub mod source_code;
pub mod source_files;
pub mod source_id;
pub mod utils;

pub use errors::*;
pub use source_code::*;
pub use source_files::*;
pub use source_id::*;
pub use utils::*;

mod prelude;
