// One harness avoids linking the compiler into a separate binary per test file.

mod captured_cell_seed;
mod checked_index_reuse;
mod in_bounds;
mod projection_effects;
mod scope_ownership;
#[path = "../src/test_support.rs"]
mod support;
