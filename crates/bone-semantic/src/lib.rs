mod consume;
pub mod fn_identity;
pub use bone_core::identity_storage;
pub mod relocation;
mod value_recursion;
pub use bone_core::hir;
pub use bone_core::{comp_error, or_fail, unify_fail};
pub use bone_inference as inference;
pub use bone_lower as lower;
#[macro_use]
pub mod prelude;
pub mod query;
pub mod semantic_passes;
pub mod stdlib_base;
