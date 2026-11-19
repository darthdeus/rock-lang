//! Incremental re-typecheck layer on top of `bone`.

pub mod session;
pub mod span_shift;

pub use session::{Session, UpdateResult, UpdateTier};
