// One harness avoids linking the compiler into a separate binary per test file.
use bone_lints::{Finding, LintOptions, lint_workspace};

mod support;

fn lint(files: &[(&str, &str)], options: &LintOptions, is_bin: bool) -> Vec<Finding> {
    let (ws, sources) = support::setup(files);
    lint_workspace(&ws, &support::warm(&ws), is_bin, &sources, options)
}

mod action_layer_boundaries;
mod action_layer_raw_input;
mod action_policy;
mod catalog;
mod deprecated;
mod incremental_cache;
mod task_null_abandon;
mod transmute_of_nullable;
mod unused_fn;
mod unused_pure_expr;
mod unused_var;
