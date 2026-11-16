// One harness avoids linking the compiler into a separate binary per test file.
mod common;

mod builtin_lifetime;
mod file_variant;
mod incremental;
mod rebuild_users_atomicity;
mod recovery;
mod v1_differential;
mod v1_routing;
