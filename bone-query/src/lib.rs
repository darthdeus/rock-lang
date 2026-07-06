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

pub mod query;
pub mod reachability;

mod backend;
mod cursor_context;
pub use backend::*;

mod ctags;
