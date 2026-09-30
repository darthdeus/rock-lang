//! bone-core: the foundation layer of the bone typechecker.
//!
//! Pure data and storage with no dependency on lowering or the inference
//! algorithm: the HIR data types, the id types (`ModuleId`/`StructId`/...), the
//! `comp_error!`/`unify_fail!` macros, small utilities, and the parallelism
//! shim. `lower` and `inference` both build on this; `Workspace` and the
//! inference RESULT structs will move here too (so `lower` stops depending on
//! `inference` -- see `docs/bone-crate-split-plan.md`).

#[macro_use]
pub mod prelude;
pub mod annotations;
pub mod hir;
pub mod escape;
pub mod identity_storage;
pub mod suspension;
pub mod workspace;
pub mod modules;
