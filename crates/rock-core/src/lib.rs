//! Foundation crate for the rock compiler: source positions (`Span`,
//! `LineCol`), diagnostics (`CompilerError`), module ids, and compile stats.
//! Deliberately holds nothing AST-shaped — the parser (`skull-parser`)
//! and `rock-tokenizer` both build on top of this.

pub mod compile_stats;
pub mod errors;
pub mod module_id;
pub mod nesting;
pub mod source_code;
pub mod source_files;
pub mod source_id;
pub mod utils;

pub use compile_stats::*;
pub use errors::*;
pub use module_id::*;
pub use source_code::*;
pub use source_files::*;
pub use source_id::*;
pub use utils::*;

mod prelude;
