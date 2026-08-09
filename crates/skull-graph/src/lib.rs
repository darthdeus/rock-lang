//! The target-neutral instance graph, and each artifact's VIEW of it
//! (`docs/monomorphization-plan.md` S 9, S 11).
//!
//! **It is below every backend, and depends on none of them.** One
//! [`instance_graph::InstanceGraph`] is built per compilation over one
//! `MonoInterner`, and each target realizes it through
//! [`artifact::realize`] -- so an `Instance` means the same thing to the
//! luajit driver, the JS emitter and the GLSL passes. A graph that lived
//! inside one backend could not be that, and a graph that depended on a
//! backend to spell its own target axes would not be neutral.
//!
//! **Roots are an INPUT, not something derived here.** Which declarations a
//! build roots from is a driver decision (`skull::mir_emit::emission_roots`),
//! which is why `build` and `ArtifactRequest` take them.

pub mod artifact;
pub mod concrete_body;
pub mod expr_subtree;
pub mod instance_graph;
pub mod target_projection;
