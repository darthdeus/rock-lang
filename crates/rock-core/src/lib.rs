//! Foundation crate for the rock compiler: source positions (`Span`,
//! `LineCol`), diagnostics (`CompilerError`), the type representation
//! (`TypeRef`), symbols, scopes, node-id newtypes, and compile stats.
//! Deliberately holds nothing AST-shaped — the parser (`skull-parser`)
//! and `rock-tokenizer` both build on top of this.

pub mod code_repr;
pub mod compile_stats;
pub mod errors;
pub mod module_id;
pub mod node_id;
pub mod scope;
pub mod shuffle;
pub mod source_code;
pub mod source_files;
pub mod source_id;
pub mod symbols;
pub mod types;
pub mod utils;

pub use code_repr::*;
pub use compile_stats::*;
pub use errors::*;
pub use module_id::*;
pub use node_id::*;
pub use scope::*;
pub use shuffle::*;
pub use source_code::*;
pub use source_files::*;
pub use source_id::*;
pub use symbols::*;
pub use types::*;
pub use utils::*;

mod prelude;
