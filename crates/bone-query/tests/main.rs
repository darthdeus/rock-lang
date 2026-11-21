// One harness: a single linked binary instead of one per file.

#[path = "support/completion.rs"]
mod completion;
#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/label_asserts.rs"]
mod label_asserts;
#[path = "support/response_invariants.rs"]
mod response_invariants;

#[path = "completion_contexts.rs"]
mod completion_contexts;
#[path = "completion_dispatch.rs"]
mod completion_dispatch;
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
#[path = "hover.rs"]
mod hover;
#[path = "incremental_stale_diagnostic.rs"]
mod incremental_stale_diagnostic;
#[path = "inline_local.rs"]
mod inline_local;
#[path = "inline_local_macro_spans.rs"]
mod inline_local_macro_spans;
#[path = "interface_relower_staleness.rs"]
mod interface_relower_staleness;
#[path = "interp_hole_spans.rs"]
mod interp_hole_spans;
#[path = "lsp_span_coverage.rs"]
mod lsp_span_coverage;
#[path = "lsp_utf16_columns.rs"]
mod lsp_utf16_columns;
#[path = "macro_goto_definition.rs"]
mod macro_goto_definition;
#[path = "modules_v1.rs"]
mod modules_v1;
#[path = "program_inputs.rs"]
mod program_inputs;
#[path = "query.rs"]
mod query;
#[path = "query_mutations.rs"]
mod query_mutations;
#[path = "reachability.rs"]
mod reachability;
#[path = "rename_read_only.rs"]
mod rename_read_only;
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
