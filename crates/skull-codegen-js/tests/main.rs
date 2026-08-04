// Single harness for this crate's integration tests: one linked binary
// instead of one per file. nextest still runs each test in its own
// process, so isolation is unchanged.

mod common;

#[path = "backend_externs.rs"]
mod backend_externs;
#[path = "closure.rs"]
mod closure;
#[path = "deepcopy.rs"]
mod deepcopy;
#[path = "differential.rs"]
mod differential;
#[path = "emit.rs"]
mod emit;
#[path = "enum_boxes.rs"]
mod enum_boxes;
#[path = "format_specs.rs"]
mod format_specs;
#[path = "inline_pipeline.rs"]
mod inline_pipeline;
#[path = "inspect.rs"]
mod inspect;
#[path = "lambda.rs"]
mod lambda;
#[path = "web_uniform_block_layout_check.rs"]
mod web_uniform_block_layout_check;
#[path = "web_canvas_active_extent.rs"]
mod web_canvas_active_extent;
#[path = "log_dedup.rs"]
mod log_dedup;
#[path = "loops.rs"]
mod loops;
#[path = "publisher_abi.rs"]
mod publisher_abi;
#[path = "map_iter.rs"]
mod map_iter;
#[path = "match_fold.rs"]
mod match_fold;
#[path = "misc_intrinsics.rs"]
mod misc_intrinsics;
#[path = "module_globals.rs"]
mod module_globals;
#[path = "multi_rejection.rs"]
mod multi_rejection;
#[path = "multivalue.rs"]
mod multivalue;
#[path = "nb_differential.rs"]
mod nb_differential;
#[path = "nb_leaf_layout_premise.rs"]
mod nb_leaf_layout_premise;
#[path = "nb_value_struct.rs"]
mod nb_value_struct;
#[path = "no_inline_function.rs"]
mod no_inline_function;
#[path = "nomangle_roots.rs"]
mod nomangle_roots;
#[path = "number_format.rs"]
mod number_format;
#[path = "overlay.rs"]
mod overlay;
#[path = "parse_float.rs"]
mod parse_float;
#[path = "prelude_decl_arity.rs"]
mod prelude_decl_arity;
#[path = "program.rs"]
mod program;
#[path = "put.rs"]
mod put;
#[path = "ret_bank_no_hand_readers.rs"]
mod ret_bank_no_hand_readers;
#[path = "ret_bank_sizing.rs"]
mod ret_bank_sizing;
#[path = "rng.rs"]
mod rng;
#[path = "shader_register.rs"]
mod shader_register;
#[path = "shared_tail.rs"]
mod shared_tail;
#[path = "snippets.rs"]
mod snippets;
#[path = "sort.rs"]
mod sort;
#[path = "spec_column_callee.rs"]
mod spec_column_callee;
#[path = "stdlib_sibling_location.rs"]
mod stdlib_sibling_location;
#[path = "string_builder.rs"]
mod string_builder;
#[path = "string_primitives.rs"]
mod string_primitives;
#[path = "strings.rs"]
mod strings;
#[path = "table_unset.rs"]
mod table_unset;
#[path = "trampoline_fallback.rs"]
mod trampoline_fallback;
#[path = "try_call.rs"]
mod try_call;
#[path = "value_gc_eq.rs"]
mod value_gc_eq;
#[path = "value_structs.rs"]
mod value_structs;
#[path = "vec_columns.rs"]
mod vec_columns;
