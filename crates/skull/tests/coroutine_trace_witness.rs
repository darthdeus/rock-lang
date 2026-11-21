use crate::common::{definition_names, emit_module, function_body, mangled_name, run_luajit};

const FIXTURE: &str = include_str!("fixtures/nested_coroutine_trace.rock");
