// One harness avoids linking the compiler into a separate binary per test file.
#[macro_use]
mod support;

mod builtin_shadow;
mod conformance;
mod const_global;
mod golden;
mod hostile_target;
mod one_plan;
mod reject;
mod reserved_locals;
mod shader_pipeline;
mod uniform_array_block_decl;
mod uniform_shadow;
