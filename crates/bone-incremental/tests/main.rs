// One harness avoids linking the compiler into a separate binary per test file.
mod common;

mod builtin_lifetime;
mod error_cascade;
mod error_recovery;
mod file_variant;
mod incremental;
mod inference_recovery;
mod rebuild_users_atomicity;
mod v1_differential;
mod v1_routing;
