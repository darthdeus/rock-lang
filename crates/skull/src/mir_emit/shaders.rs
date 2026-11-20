use super::*;
use crate::instance_graph::Edge;

/// A Lua long-bracket string literal holding `s`, at the smallest `=` level
/// whose closing bracket does not occur in `s`. Lua's long-string rule
/// drops a leading newline.
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

/// Projected from instances onto declarations. Projection only merges
/// components, so a callee reported separate from its caller really is, and
/// the binding pass never orders a capture after the closure that needs it.
pub(super) fn call_sccs(
    graph: &crate::instance_graph::InstanceGraph,
) -> bone_semantic::prelude::FxHashMap<skull_mir::mir::FuncRef, u32> {
    skull_codegen_lua::call_sccs(graph.edges().map(|(from, edge)| (from.func, edge.to.func)))
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
    let rejections: Vec<&crate::instance_graph::Rejection> = graph.rejections().collect();
    let mut identity_walk = None;
    for rejection in &rejections {
        if rejection.kind == crate::instance_graph::RejectionKind::IdentityInOrdinaryStorage {
            let walk = identity_walk
                .get_or_insert_with(|| IdentityRejectionWalk::new(ws, graph, &rejections));
            if let Some(err) = walk.error(ws, rejection) {
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

fn identity_storage_exempt(ws: &Workspace, instance: &Instance) -> bool {
    bone_semantic::identity_storage::identity_storage_exempt(ws, ws.module(instance.func.module))
}

/// An identity-storage rejection belongs to the package whose call site
/// instantiated it. A caller in an exempt package whose own type arguments
/// carry the identity only forwards it, so its callers are asked in turn.
struct IdentityRejectionWalk<'g> {
    forwarders: FxHashSet<&'g Instance>,
    callers: FxHashMap<&'g Instance, Vec<(&'g Instance, &'g Edge)>>,
    cleared: FxHashSet<&'g Instance>,
}

impl<'g> IdentityRejectionWalk<'g> {
    fn new(
        ws: &Workspace,
        graph: &'g crate::instance_graph::InstanceGraph,
        rejections: &[&'g crate::instance_graph::Rejection],
    ) -> Self {
        use bone_core::prelude::*;
        let owners: Vec<(
            &crate::instance_graph::Owner,
            &crate::instance_graph::Contribution,
        )> = graph
            .contributions
            .iter()
            .map(|(owner, c)| (owner, &**c))
            .collect();
        let forwarders: FxHashSet<&Instance> = owners
            .par_iter()
            .filter_map(|(owner, _)| owner.instance())
            .filter(|caller| {
                identity_storage_exempt(ws, caller)
                    && caller.args.iter().any(|arg| {
                        skull_graph::identity_storage::stores_identity(
                            ws,
                            &graph.interner().get(*arg),
                        )
                    })
            })
            .collect();
        let rejected: FxHashSet<&Instance> = rejections
            .iter()
            .filter(|rejection| {
                rejection.kind == crate::instance_graph::RejectionKind::IdentityInOrdinaryStorage
            })
            .map(|rejection| &rejection.at)
            .collect();
        let plain = rejected.iter().any(|instance| instance.args.is_empty());
        // A full build keeps no caller index, so every owner is read twice.
        let into: Vec<Vec<(&Instance, &Edge)>> = owners
            .par_iter()
            .map(|(owner, c)| {
                c.edges
                    .iter()
                    .filter(|edge| {
                        (plain || !edge.to.args.is_empty())
                            && (rejected.contains(&edge.to) || forwarders.contains(&edge.to))
                    })
                    .map(|edge| (owner.instance().expect("edge owner is an instance"), edge))
                    .collect()
            })
            .collect();
        let mut callers: FxHashMap<_, Vec<_>> = FxHashMap::default();
        for (from, edge) in into.into_iter().flatten() {
            callers.entry(&edge.to).or_default().push((from, edge));
        }
        IdentityRejectionWalk {
            forwarders,
            callers,
            cleared: FxHashSet::default(),
        }
    }

    fn error(
        &mut self,
        ws: &Workspace,
        rejection: &'g crate::instance_graph::Rejection,
    ) -> Option<CompilerError> {
        let mut work = vec![&rejection.at];
        while let Some(instance) = work.pop() {
            if !self.cleared.insert(instance) {
                continue;
            }
            for (from, edge) in self.callers.get(instance).into_iter().flatten() {
                if self.forwarders.contains(from) {
                    work.push(from);
                } else if !identity_storage_exempt(ws, from) {
                    let callee =
                        &ws.module(rejection.at.func.module).functions[rejection.at.func.func];
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
        // An instance no edge reaches is rooted by an always-emitted body, so its own
        // package answers for it.
        (!self.callers.contains_key(&rejection.at) && !identity_storage_exempt(ws, &rejection.at))
            .then(|| CompilerError::new_with_label(rejection.span, rejection.message.clone()))
    }
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
    let programs: Vec<(
        &skull_mir::lower::ShaderPair,
        &crate::shader_artifact::ShaderProgramView,
        Option<&skull_codegen_glsl::ShaderPlan>,
    )> = collection
        .pairs
        .iter()
        .zip(&view.program_views)
        .zip(&view.program_plans)
        .map(|((pair, program_view), plan)| (pair, program_view, plan.as_deref()))
        .collect();
    let emissions: Vec<Result<skull_codegen_glsl::ShaderEmission, CompilerError>> = {
        use bone_core::prelude::*;
        programs
            .par_iter()
            // A program the view did not plan is emitted from the view, which reports
            // the same refusal at the same span.
            .map(|(_, program_view, plan)| match plan {
                Some(plan) => {
                    skull_codegen_glsl::render_shader_plan(ws, scx.interner(), plan, profile)
                }
                None => skull_codegen_glsl::emit_shader_with_view(ws, scx, program_view, profile),
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
        // Slot order is the engine's whole view of texture binding: it points each
        // sampler at its unit once at link time, then binds by slot.
        let slot_names: Vec<&str> = emission
            .abi
            .texture_units
            .iter()
            .map(|n| n.as_str())
            .collect();
        // The `@uniform` blocks' std140 layouts, as parallel flat lists (one
        // entry per block, members concatenated with a per-block count) so a
        // hand-written driver never nests.
        // A program with no block still passes the empty lists.
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
    Ok(ShaderRegistrations {
        lua: out,
        sources: sources.into(),
        dependencies,
        shader_contributions: collection
            .contributions
            .iter()
            .filter(|(_, pairs)| !pairs.is_empty())
            .map(|(&key, pairs)| (key, pairs.clone()))
            .collect(),
        roots,
    })
}

impl ShaderRegistrations {
    pub(super) fn collection(&self) -> skull_mir::lower::ShaderCollection {
        let mut pairs = BTreeMap::new();
        for pair in self.shader_contributions.values().flatten() {
            pairs
                .entry(pair.key.as_str())
                .or_insert_with(|| pair.clone());
        }
        skull_mir::lower::ShaderCollection {
            pairs: pairs.into_values().collect(),
            callsites: self.shader_contributions.keys().copied().collect(),
            contributions: self.shader_contributions.clone(),
        }
    }

    pub(super) fn keys_match<'a>(&self, keys: impl Iterator<Item = &'a str>) -> bool {
        self.sources
            .iter()
            .map(|source| source.key.as_str())
            .eq(keys)
    }
}
