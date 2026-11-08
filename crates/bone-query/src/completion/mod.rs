//! The completion pipeline: classify the
//! cursor once, sources emit fact-shaped candidates, one rank key, one
//! select + materialization. The request itself (lexical gate, the ordered
//! contextual sources, the general gather/rank/synthesize/finish steps)
//! lives in `backend/completion.rs`, next to the project Session it
//! analyzes variants of; `backend.rs` keeps `ExpectedArg` and the shared
//! workspace plumbing.

pub(crate) mod classify;
pub(crate) mod match_policy;
pub(crate) mod rank;
pub(crate) mod select;
pub(crate) mod sources;
