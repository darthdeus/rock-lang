//! The target-neutral instance graph, and each artifact's VIEW of it.
//!
//! **It depends on no backend, so any backend may depend on it.** One
//! [`instance_graph::InstanceGraph`] is built per compilation over one
//! `MonoInterner`, and each target realizes it through [`artifact::realize`],
//! so an `Instance` can mean the same thing to every emitter. A graph that
//! lived inside one backend could not be that, and a graph that depended on a
//! backend to spell its own target axes would not be neutral.
//!
//! **Current dependents, stated rather than assumed**: `skull`, `skull-codegen-js`, and
//! `skull-codegen-glsl` depend on this crate. Lua, JS, and native shader
//! registration consume per-target views; route-B's plain GLSL function-library
//! entry point remains a legacy path because it has no registered shader
//! artifact. The native seam is `mir_emit::shader_registration_lines`, and the
//! web/validator drivers use the same view-driven entry/pipeline emitters.
//!
//! **Roots are an INPUT, not something derived here.** Which declarations a
//! build roots from is a driver decision (`skull::mir_emit::emission_roots`,
//! plus the generated-glue obligations `all_roots` adds), which is why `build`
//! and `ArtifactRequest` take them.

pub mod artifact;
pub mod concrete_body;
pub mod expr_subtree;
pub mod identity_storage;
pub mod instance_graph;
pub mod shader;
pub mod target_projection;
