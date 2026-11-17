//! Incremental re-typecheck layer on top of `bone`. `Session` tracks a set of source files and
//! applies edits incrementally: classify each edit (`BodyOnly` / `Interface` / ...), then re-
//! lower + re-infer only what changed.

pub mod session;
pub mod span_shift;

pub use session::{Session, UpdateResult, UpdateTier};
