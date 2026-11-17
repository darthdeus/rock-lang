// One harness avoids linking the compiler into a separate binary per test file.
use bone_lints::{Finding, LintOptions, lint_workspace};
use bone_semantic::prelude::*;

mod support;

fn lint(files: &[(&str, &str)], options: &LintOptions, is_bin: bool) -> Vec<Finding> {
    let (ws, sources) = support::setup(files);
    lint_workspace(&ws, &support::warm(&ws), is_bin, &sources, options)
}

fn lint_asts(
    asts: &mut [skull_parser::Ast],
    files: &[(&str, &str)],
    options: &LintOptions,
    is_bin: bool,
) -> Vec<Finding> {
    let ws = support::workspace(asts.to_vec());
    let sources: FxHashMap<Ustr, &str> = files
        .iter()
        .map(|(name, src)| (Ustr::from(name), *src))
        .collect();
    lint_workspace(&ws, &support::warm(&ws), is_bin, &sources, options)
}

mod action_layer_boundaries;
mod action_layer_raw_input;
mod action_policy;
mod backend_sibling_mismatch;
mod catalog;
mod incremental_cache;
mod method_syntax;
mod modules_v1_names;
mod row_escape;
mod task_null_abandon;
mod transmute_of_nullable;
mod unused_fn;
mod unused_pure_expr;
mod unused_var;
