//! IDE/LSP query layer over bone's typed HIR.

pub mod modules_scope;
pub mod query;
pub mod reachability;

mod backend;
mod completion;
mod cursor_context;
mod rank_key;
mod types;
pub use backend::*;
pub use rank_key::explain_sort_text;

mod ctags;

mod file_uri;
pub use file_uri::{FileUriError, FileUriExt};
