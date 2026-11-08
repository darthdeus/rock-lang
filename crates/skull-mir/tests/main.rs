// One harness avoids linking the compiler into a separate binary per test file.
mod abi_plan;
mod common;
mod demand_map;
mod gpu_layout;
mod instance_args_subst_roundtrip;
mod instance_identity;
mod lower_concrete;
mod monoty;
mod projection_demand;
mod reach_driver;
mod result_destinations;
mod shader_ir;
mod shape;
mod uniform_array_block;
mod uniform_block;
mod wire_kind_codes;
mod zero_arg_mangle;
