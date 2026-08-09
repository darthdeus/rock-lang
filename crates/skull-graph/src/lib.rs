//! The target-neutral instance graph, and each artifact's VIEW of it
//! (`docs/monomorphization-plan.md` S 9, S 11).
//!
//! **It depends on no backend, so any backend may depend on it.** One
//! [`instance_graph::InstanceGraph`] is built per compilation over one
//! `MonoInterner`, and each target realizes it through [`artifact::realize`],
//! so an `Instance` can mean the same thing to every emitter. A graph that
//! lived inside one backend could not be that, and a graph that depended on a
//! backend to spell its own target axes would not be neutral.
//!
//! **Current dependents, stated rather than assumed** (E1 Phase 3/4 in
//! `docs/monomorphization-plan.md`): `skull` and `skull-codegen-js` depend on
//! this crate. Lua and JS production emission consume per-target views; GLSL
//! production still has its private reachability path. The planned GLSL seam is
//! through the `skull` driver -- `mir_emit::shader_registration_lines` is what
//! calls `plan_shader_entry`/`plan_shader_pipeline` -- so
//! `skull-codegen-glsl` may never need a direct dependency at all.
//!
//! **Roots are an INPUT, not something derived here.** Which declarations a
//! build roots from is a driver decision (`skull::mir_emit::emission_roots`,
//! plus the generated-glue obligations `all_roots` adds), which is why `build`
//! and `ArtifactRequest` take them.

pub mod artifact;
pub mod concrete_body;
pub mod expr_subtree;
pub mod instance_graph;
pub mod target_projection;
