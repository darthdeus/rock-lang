//! The bone typechecker's data layer: HIR, ids, `Workspace` and inference
//! results, with no dependency on lowering or the inference algorithm.

#[macro_use]
pub mod prelude;
pub mod annotations;
pub mod comptime_files;
pub mod effects;
pub mod escape;
pub mod hir;
pub mod hir_walk;
pub mod identity_storage;
pub mod modules;
pub mod suspension;
pub mod text_edit;
pub mod type_equiv;
pub mod type_sources;
pub mod workspace;
