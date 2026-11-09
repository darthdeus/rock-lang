pub mod fn_identity;
pub use bone_core::identity_storage;
pub mod relocation;
mod value_recursion;
pub use bone_core::hir;
// The macros are `#[macro_export]`'d in bone-core; re-export at bone's root so
// `bone_semantic::comp_error!` / `bone_semantic::or_fail!` / `bone_semantic::unify_fail!` keep resolving.
pub use bone_core::{comp_error, or_fail, unify_fail};
pub use bone_inference as inference;
pub use bone_lower as lower;
#[macro_use]
pub mod prelude;
// `type_at` -- the position->type inference-inspection primitive bone's own
// `//^` fixtures (see tests/test_support.rs) and LSP hover both read. IDE
// navigation queries (definition/references) live in the bone-query crate.
pub mod query;
pub mod semantic_passes;
pub mod stdlib_base;
