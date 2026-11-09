//! Shader program discovery, realization and registration.

use super::*;

/// A Lua long-bracket string literal (`[==[ ... ]==]`) holding `s`, at the
/// smallest `=` level whose closing bracket does not occur in `s` (never does
/// for generated GLSL, but the guard keeps a `]]` in shader source from
/// terminating early). The leading newline is elided by Lua's long-string rule.
fn lua_long_bracket(s: &str) -> String {
    let mut level = 0usize;
    loop {
        let close = format!("]{}]", "=".repeat(level));
        if !s.contains(&close) {
            let eqs = "=".repeat(level);
            return format!("[{eqs}[\n{s}]{eqs}]");
        }
        level += 1;
    }
}

pub(super) fn shader_programs(
    collection: &skull_mir::lower::ShaderCollection,
) -> Vec<crate::shader_artifact::ShaderProgram> {
    collection
        .pairs
        .iter()
        .map(|pair| crate::shader_artifact::ShaderProgram {
            vert: pair.vert,
            frag: pair.frag,
        })
        .collect()
}

/// Every function's call-graph strongly connected component, read off the
/// whole-program instance graph and projected onto declarations. Every edge
/// kind counts, `EdgeKind::FnValue` included: a function taken as a value is
/// reached by a call the direct-call edges cannot see. Projecting MERGES nodes, which can only merge
/// components, so a callee this reports as separate from its caller really is:
/// the binding pass never orders a capture after the closure that needs it.
pub(super) fn call_sccs(
    graph: &crate::instance_graph::InstanceGraph,
) -> bone_semantic::prelude::FxHashMap<skull_mir::mir::FuncRef, u32> {
    skull_codegen_lua::call_sccs(
        graph
            .contributions
            .values()
            .flat_map(|contribution| contribution.edges.iter())
            .map(|edge| (edge.from.func, edge.to.func)),
    )
}

pub(super) fn realize_shader_view(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    programs: &[crate::shader_artifact::ShaderProgram],
    scx: &skull_mir::lower::ScalarCtx,
    plans: std::sync::Arc<skull_mir::concrete::ConcretePlans>,
) -> Result<crate::shader_artifact::ShaderArtifactView, CompilerError> {
    crate::shader_artifact::realize_shader_with_context_and_plans(
        ws,
        graph,
        &crate::shader_artifact::ShaderArtifactRequest {
            spec: crate::target_projection::TargetSpec {
                shader: Some(crate::target_projection::ShaderTargetSpec::Glsl(
                    skull_codegen_glsl::GlslProfile::Core330,
                )),
                ..crate::target_projection::LUAJIT_DESKTOP
            },
            programs,
        },
        scx,
        Some(plans),
    )
    .ok_or_else(|| CompilerError::new_no_location("GLSL shader view is unavailable".to_string()))
}

pub(super) fn graph_rejection_error(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
) -> Option<CompilerError> {
    let mut callers: Option<
        FxHashMap<&crate::instance_graph::Instance, Vec<&crate::instance_graph::Edge>>,
    > = None;
    for rejection in graph.rejections() {
        if rejection.kind == crate::instance_graph::RejectionKind::IdentityInOrdinaryStorage {
            let callers = callers.get_or_insert_with(|| {
                let mut map: FxHashMap<_, Vec<_>> = FxHashMap::default();
                for edge in graph.edges() {
                    map.entry(&edge.to).or_default().push(edge);
                }
                map
            });
            if let Some(err) = identity_instance_rejection_error(ws, graph, callers, rejection) {
                return Some(err);
            }
            continue;
        }
        if is_stdlib_module_file(ws.module(rejection.at.func.module).file.as_str()) {
            continue;
        }
        return Some(CompilerError::new_with_label(
            rejection.span,
            rejection.message.clone(),
        ));
    }
    None
}

/// An instance's identity-storage rejection belongs to the package whose call
/// site instantiated it. The walk goes back
/// from the instance through its callers; a caller in an exempt package whose
/// own type arguments carry an identity only forwarded it, so its callers are
/// asked in turn. Any other caller introduced the identity, and the rejection is
/// fatal when that caller's package is not exempt.
fn identity_instance_rejection_error(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    callers: &FxHashMap<&crate::instance_graph::Instance, Vec<&crate::instance_graph::Edge>>,
    rejection: &crate::instance_graph::Rejection,
) -> Option<CompilerError> {
    let exempt = |instance: &crate::instance_graph::Instance| {
        bone_semantic::identity_storage::identity_storage_exempt(
            ws,
            ws.module(instance.func.module),
        )
    };
    let mut seen = FxHashSet::default();
    let mut work = vec![&rejection.at];
    let mut reached_by_edge = false;
    while let Some(instance) = work.pop() {
        if !seen.insert(instance) {
            continue;
        }
        for edge in callers.get(instance).into_iter().flatten() {
            reached_by_edge = true;
            let forwarded = exempt(&edge.from)
                && edge.from.args.iter().any(|arg| {
                    skull_graph::identity_storage::stores_identity(ws, &graph.interner().get(*arg))
                });
            if forwarded {
                work.push(&edge.from);
            } else if !exempt(&edge.from) {
                let callee = &ws.module(rejection.at.func.module).functions[rejection.at.func.func];
                return Some(
                    CompilerError::new_with_label(rejection.span, rejection.message.clone())
                        .with_label(
                            edge.span,
                            format!(
                                "`{}` is instantiated from here with an identity struct",
                                callee.name
                            ),
                        ),
                );
            }
        }
    }
    // An instance no edge reaches is rooted by an always-emitted body; the
    // instance's own package answers for it.
    (!reached_by_edge && !exempt(&rejection.at))
        .then(|| CompilerError::new_with_label(rejection.span, rejection.message.clone()))
}

pub(super) fn shader_registration_lines(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    collection: &skull_mir::lower::ShaderCollection,
    view: &crate::shader_artifact::ShaderArtifactView,
) -> Result<ShaderRegistrations, CompilerError> {
    let profile = skull_codegen_glsl::GlslProfile::Core330;
    if view.profile != profile || view.program_views.len() != collection.pairs.len() {
        return Err(CompilerError::new_no_location(
            "shader graph/view registration denominator differs from shader collection".to_string(),
        ));
    }
    // Each program's GLSL is emitted from its own view against the shared
    // scalar context; the programs fan out and register in collection order.
    let programs: Vec<(
        &skull_mir::lower::ShaderPair,
        &crate::shader_artifact::ShaderProgramView,
    )> = collection.pairs.iter().zip(&view.program_views).collect();
    let emissions: Vec<Result<skull_codegen_glsl::ShaderEmission, CompilerError>> = {
        use bone_core::prelude::*;
        programs
            .par_iter()
            .map(|(pair, program_view)| {
                // vert=None is the A3c single-arg fullscreen form -> the A1
                // single-entry emitter (PIXEL body + fullscreen VERTEX passthrough).
                match pair.vert {
                    Some(_) => skull_codegen_glsl::emit_shader_pipeline_with_view(
                        ws,
                        scx,
                        program_view,
                        profile,
                    ),
                    None => skull_codegen_glsl::emit_shader_entry_with_view(
                        ws,
                        scx,
                        program_view,
                        profile,
                    ),
                }
            })
            .collect()
    };
    let mut out = String::new();
    let mut sources = Vec::new();
    let mut dependencies = FxHashSet::default();
    for (pair, emission) in collection.pairs.iter().zip(emissions) {
        let emission = emission?;
        dependencies.extend(emission.dependencies);
        sources.push(ShaderSource {
            key: pair.key.clone(),
            language: ShaderLanguage::Glsl,
            source: emission.source.clone(),
        });
        // Slot order is the engine's whole view of texture binding: it points
        // each sampler at its unit once at link time, then binds by slot.
        let slot_names: Vec<&str> = emission
            .abi
            .texture_units
            .iter()
            .map(|n| n.as_str())
            .collect();
        // The `@uniform` blocks' std140 layouts, in binding order: A1 has at
        // most one, an A2 pipeline has one per stage. Carried as PARALLEL FLAT
        // lists -- one entry per block, plus the members concatenated with a
        // per-block count -- so a hand-written driver never nests. A program
        // with no group sends empty lists, which are still arguments; a short
        // call is what rock-cli/tests/shader_register_arity_gate.rs catches.
        let w = skull_codegen_glsl::BlockWire::of(
            emission.abi.uniform_blocks.iter(),
            emission.abi.uniform_array_blocks.iter(),
        );
        let fullscreen = pair.vert.is_none();
        let semantic_wire = emission.interface.semantic_wire(fullscreen);
        let _ = writeln!(
            out,
            "__rock_shader_register({:?}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            pair.key,
            lua_long_bracket(&emission.source),
            skull_codegen_lua::lua_string_vec(&slot_names),
            skull_codegen_lua::lua_string_vec(&w.names),
            skull_codegen_lua::lua_int_vec(&w.bindings),
            skull_codegen_lua::lua_int_vec(&w.sizes),
            skull_codegen_lua::lua_int_vec(&w.fingerprints),
            skull_codegen_lua::lua_int_vec(&w.counts),
            skull_codegen_lua::lua_string_vec(&w.member_names),
            skull_codegen_lua::lua_int_vec(&w.member_offsets),
            skull_codegen_lua::lua_int_vec(&w.member_kinds),
            skull_codegen_lua::lua_int_vec(&w.member_capacities),
            skull_codegen_lua::lua_int_vec(&w.member_strides),
            fullscreen,
            lua_long_bracket(&semantic_wire),
        );
    }
    let roots: Vec<FuncRef> = collection
        .pairs
        .iter()
        .flat_map(|pair| pair.vert.into_iter().chain(std::iter::once(pair.frag)))
        .collect();
    let keys = collection
        .pairs
        .iter()
        .map(|pair| pair.key.clone())
        .collect();
    let mut shader_pair_users = BTreeMap::new();
    for pairs in collection.contributions.values() {
        for pair in pairs {
            *shader_pair_users.entry(pair.key.clone()).or_insert(0) += 1;
        }
    }
    let shader_pairs = collection
        .pairs
        .iter()
        .map(|pair| (pair.key.clone(), pair.clone()))
        .collect();
    let shader_root_set = roots.iter().copied().collect();
    Ok(ShaderRegistrations {
        lua: out,
        sources: sources.into(),
        dependencies,
        callsites: collection.callsites.iter().copied().collect(),
        shader_contributions: collection.contributions.clone(),
        shader_pair_users,
        shader_pairs,
        shader_root_set,
        roots: roots.into_iter().collect(),
        keys,
    })
}
