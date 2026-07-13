//! The completion pipeline (docs/completion-v2-design.md M5): classify the
//! cursor once, sources emit fact-shaped candidates, one rank key, one
//! select + materialization. `backend.rs` keeps only the workspace/variant
//! plumbing (`BoneWorkspace::completion`, `ExpectedArg`, Session variants).

pub(crate) mod classify;
pub(crate) mod match_policy;
pub(crate) mod rank;
pub(crate) mod select;
pub(crate) mod sources;
