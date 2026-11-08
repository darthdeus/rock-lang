//! bone-core: the foundation layer of the bone typechecker.
//!
//! Pure data and storage with no dependency on lowering or the inference
//! algorithm: the HIR data types, the id types (`ModuleId`/`StructId`/...), the
//! `comp_error!`/`unify_fail!` macros, small utilities, and the parallelism
//! shim, plus `Workspace` and the inference result types. `lower` and
//! `inference` both build on this.

#[macro_use]
pub mod prelude;
pub mod annotations;
pub mod comptime_files;
pub mod escape;
pub mod hir;
pub mod identity_storage;
pub mod modules;
pub mod suspension;
pub mod type_equiv;
pub mod type_sources;
pub mod workspace;
