//! IDE/LSP query layer over bone's typed HIR.
//!
//! - [`query`] -- protocol-agnostic IDE-navigation queries (definition,
//!   find-references) over a checked `bone_semantic::Workspace`. Returns bone-native
//!   spans, no `lsp_types`. (The position->type primitive `type_at` stays in
//!   `bone_semantic::query` -- bone's own `//^` fixtures read it too.)
//! - [`backend`] -- the `lsp_types`-facing facade (`BoneWorkspace`) that drives
//!   `bone_incremental::session::Session` and converts query results into LSP types. This is
//!   the only place in the stack that depends on `lsp_types`.
//!
//! bone remains the pure engine; this crate consumes its public pipeline.

pub mod modules_scope;
pub mod query;
pub mod reachability;

mod backend;
mod completion;
mod cursor_context;
mod rank_key;
mod types;
pub use backend::*;
pub use completion::match_policy::{MatchPolicy, set_match_policy};
pub use rank_key::explain_sort_text;

mod ctags;
