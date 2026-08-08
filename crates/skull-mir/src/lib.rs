//! skull-mir — the cell-model CFG IR (and, in later phases, the HIR->CFG
//! lowering) for skull, the Claude-managed backend.
//!
//! A faithful realization of `docs/bone-mir-design.md`: variables are mutable
//! cells, expression results are single-use values, `Idx<Inst>` IS the value,
//! control flow lives only in block terminators, and there are NO block params
//! (merges go through cells). Transforms live in `skull-transform`; codegen in
//! `skull-codegen-*`. Roadmap + premises under test:
//! `docs/skull-bone-mir-faithful-plan.md`.

pub mod ablation;
pub mod bound;
pub mod cfg;
pub mod decorator;
pub mod gpu_layout;
pub mod display;
pub mod intrinsics;
pub mod lower;
pub mod mangle;
pub mod mir;
pub mod monoty;
pub mod reach;
pub mod shader_ir;
pub mod instance;
pub mod site;
pub mod spec_census;
pub mod struct_defaults;
pub mod verdict;
pub mod vertex_plan;
