// One harness avoids linking the compiler into a separate binary per test file.
mod common;

mod file_variant;
mod incremental;
mod recovery;
mod session_lifecycle;
mod v1_differential;
mod v1_routing;
