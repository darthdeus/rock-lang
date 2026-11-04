// One harness avoids linking the compiler into a separate binary per test file.
mod support;

mod action_layer_boundaries;
mod action_layer_raw_input;
mod action_policy;
mod backend_sibling_mismatch;
mod catalog;
mod census;
mod incremental_cache;
mod method_syntax;
mod modules_v1_names;
mod row_escape;
mod task_null_abandon;
mod transmute_of_nullable;
mod unused_fn;
mod unused_pure_expr;
mod unused_var;
