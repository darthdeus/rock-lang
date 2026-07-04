//! Incremental re-typecheck layer on top of `bone`.
//!
//! `Session` tracks a set of source files and applies edits incrementally:
//! classify each edit (`BodyOnly` / `Interface` / ...), then re-lower + re-infer
//! only what changed. This is the edit-loop layer the LSP and skull hot-reload
//! build on -- distinct from `bone` itself, which type-checks a whole program.
//!
//! `span_shift` patches the absolute spans of the bodies an incremental edit
//! KEEPS, so queries stay correct without re-lowering them.

pub mod session;
pub mod span_shift;

pub use session::{Session, UpdateResult, UpdateTier};
