// Single harness for this crate's integration tests: one linked binary
// instead of one per file. nextest still runs each test in its own
// process, so isolation is unchanged.

#[path = "completion_blank_line.rs"]
mod completion_blank_line;
#[path = "completion_contexts.rs"]
mod completion_contexts;
#[path = "completion_dispatch.rs"]
mod completion_dispatch;
#[path = "completion_in_comment.rs"]
mod completion_in_comment;
#[path = "completion_in_string.rs"]
mod completion_in_string;
#[path = "completion_invariants.rs"]
mod completion_invariants;
#[path = "completion_macros.rs"]
mod completion_macros;
#[path = "completion_magic_comment.rs"]
mod completion_magic_comment;
#[path = "completion_numeric_prefix.rs"]
mod completion_numeric_prefix;
#[path = "completion_ranking.rs"]
mod completion_ranking;
#[path = "completion_sweep.rs"]
mod completion_sweep;
#[path = "cross_file_diagnostics.rs"]
mod cross_file_diagnostics;
#[path = "ctags.rs"]
mod ctags;
#[path = "cw7_backend_failure_reporting.rs"]
mod cw7_backend_failure_reporting;
#[path = "enum_variant_nav.rs"]
mod enum_variant_nav;
#[path = "hole_info.rs"]
mod hole_info;
#[path = "hover.rs"]
mod hover;
#[path = "incremental_stale_diagnostic.rs"]
mod incremental_stale_diagnostic;
#[path = "inline_local.rs"]
mod inline_local;
#[path = "interface_relower_staleness.rs"]
mod interface_relower_staleness;
#[path = "interp_hole_spans.rs"]
mod interp_hole_spans;
#[path = "line_index.rs"]
mod line_index;
#[path = "lint_columns_after_body_edit.rs"]
mod lint_columns_after_body_edit;
#[path = "macro_call_site_sentinel.rs"]
mod macro_call_site_sentinel;
#[path = "macro_goto_definition.rs"]
mod macro_goto_definition;
#[path = "modules_v1.rs"]
mod modules_v1;
#[path = "noncanonical_path.rs"]
mod noncanonical_path;
#[path = "phantom_block_sentinel.rs"]
mod phantom_block_sentinel;
#[path = "query.rs"]
mod query;
#[path = "reachability.rs"]
mod reachability;
#[path = "semantic_token_mutations.rs"]
mod semantic_token_mutations;
#[path = "semantic_tokens.rs"]
mod semantic_tokens;
#[path = "stale_spans_after_body_edit.rs"]
mod stale_spans_after_body_edit;
#[path = "stdlib_lsp.rs"]
mod stdlib_lsp;
#[path = "struct_fill_from_scope.rs"]
mod struct_fill_from_scope;
#[path = "type_sources.rs"]
mod type_sources;
#[path = "workspace.rs"]
mod workspace;
