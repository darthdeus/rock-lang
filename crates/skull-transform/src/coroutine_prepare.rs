//! The backend-neutral front half of `co fn` emission. Both drivers render
//! from a [`PreparedCoroutine`]: the transformed step, its detached defer and
//! hook bodies, the converted closures and the finished code artifact with
//! the keys the constructor publishes.

use rustc_hash::FxHashSet as HashSet;

use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::code_identity::{
    CaptureSchema, CodeArtifact, CodeCompatibility, CodeIdentity, FrameSchema, LambdaSiteId,
};
use skull_mir::lower::CoroutinePlan;
use skull_mir::mir::*;
use skull_mir::verify::Stage;
use ustr::Ustr;

use crate::CoroLowered;

/// The code-slot keys a coroutine constructor references.
pub struct CoroutineKeys {
    pub advance: Option<String>,
    pub resume: String,
    pub defers: Vec<String>,
    pub hooks: Vec<String>,
}

pub struct PreparedCoroutine {
    pub lowered: CoroLowered,
    pub step_name: String,
    /// `lowered.step.nested_fns[..named_nested]` are the co fn's named nested
    /// `fn`s, which backends finish like any ordinary nested fn; the rest are
    /// hoisted lambda bodies, already cleaned up here.
    pub named_nested: usize,
    /// Terminal-cleanup defers, cleaned up, with cleaned-up nested bodies.
    pub defers: Vec<MirFunc>,
    /// Lifecycle hooks, cleaned up, with cleaned-up nested bodies.
    pub hooks: Vec<(CoroHookKind, MirFunc)>,
    pub code_artifact: CodeArtifact,
    pub keys: CoroutineKeys,
}

/// Transform `func`, split out its detached regions, convert closures,
/// validate the step and build its code artifact.
///
/// `reserve` sees every synthetic global name this coroutine mints, so a
/// backend can reject a user definition that collides with one.
///
/// Coroutine bodies are never inlined, so a tagged iterator loop in a hoisted
/// lambda, defer or hook still calls `next` and `recover_counted_loops` could
/// not match it. Those bodies take cleanup only.
pub fn prepare_coroutine(
    ws: &Workspace,
    interner: &skull_mir::monoty::MonoInterner,
    source_func: &bone_core::hir::Function,
    plan: &CoroutinePlan,
    func: &MirFunc,
    reserve: &dyn Fn(&str),
) -> Result<PreparedCoroutine, CompilerError> {
    let channel = plan.channel.as_ref();
    let mut lowered = crate::transform_coroutine(func, channel, plan.has_return_value());
    crate::body_chain::debug_verify(&lowered.step, Stage::LOWERED, &"the coroutine transform");
    // `__resume`/`__advance` are rendered by the constructor from the plan;
    // whether an `__advance` exists is the plan's answer.
    reserve(&plan.resume());
    if let Some(advance) = plan.advance() {
        reserve(&advance);
    }
    let step_name = plan.step();
    reserve(&step_name);
    // Detached regions become ordinary MirFuncs, so their own lambda
    // parameters and locals cannot be rendered as coroutine frame fields.
    let frame_info = lowered.root_frame_info();
    let defer_entries: Vec<Idx<Block>> = lowered
        .step
        .coro_defers
        .iter()
        .map(|(_, entry)| *entry)
        .collect();
    let hook_entries: Vec<(CoroHookKind, Idx<Block>)> = lowered.hooks.clone();
    let mut defers = Vec::with_capacity(defer_entries.len());
    for (k, &entry) in defer_entries.iter().enumerate() {
        let name = plan.defer(k);
        reserve(&name);
        defers.push(crate::lower_detached_region(
            &lowered.step,
            entry,
            &name,
            &frame_info,
        ));
    }
    let mut hooks = Vec::with_capacity(hook_entries.len());
    for (kind, entry) in &hook_entries {
        let name = plan.hook(kind.slot());
        reserve(&name);
        let mut aux = crate::lower_detached_region(&lowered.step, *entry, &name, &frame_info);
        // A hook without a body site uses its registration lambda's source site.
        if aux.code_identity.is_none() {
            let enclosing = func.code_identity.as_ref().and_then(CodeIdentity::owner);
            let site = hook_registration_site(func, *kind, *entry);
            let (Some(enclosing), Some(site)) = (enclosing, site) else {
                return Err(CompilerError::new_with_label(
                    source_func.name_span,
                    format!(
                        "coroutine hook `{}` has no source identity at its registration lambda",
                        kind.slot()
                    ),
                ));
            };
            aux.code_identity = Some(CodeIdentity::Nested {
                enclosing,
                site,
                name: aux.name,
            });
            aux.code_compatibility = Some(CodeCompatibility {
                call_abi: format!("detached:{}", aux.name),
                capture: CaptureSchema::empty(),
                frame: Some(FrameSchema {
                    slots: sorted_slots(&lowered),
                }),
                lambda_topology: Default::default(),
            });
        }
        hooks.push((*kind, aux));
    }
    crate::erase_detached_lambdas(
        &mut lowered.step,
        defer_entries
            .iter()
            .copied()
            .chain(hook_entries.iter().map(|(_, entry)| *entry)),
    );
    let named_nested = lowered.step.nested_fns.len();
    convert_named_nested_closures(&mut lowered.step.nested_fns[..named_nested], reserve);
    crate::convert_closures_framed(
        &mut lowered.step,
        &step_name,
        Some(&frame_info),
        Some((&lowered.step_tree, &lowered.step_own)),
    );
    let mut detached_identities = HashSet::default();
    for aux in defers.iter().chain(hooks.iter().map(|(_, aux)| aux)) {
        collect_nested_code_identities(aux, &mut detached_identities);
    }
    lowered.step.nested_fns.retain(|nested| {
        nested
            .code_identity
            .as_ref()
            .is_none_or(|id| !detached_identities.contains(id))
    });
    for nf in &lowered.step.nested_fns[named_nested..] {
        reserve(&nf.name);
    }
    crate::validate_no_residual_bound_calls(ws, source_func, &lowered.step)?;
    for nf in &mut lowered.step.nested_fns[named_nested..] {
        crate::cleanup_with_nobox(ws, nf)?;
    }
    for aux in defers
        .iter_mut()
        .chain(hooks.iter_mut().map(|(_, aux)| aux))
    {
        crate::cleanup_with_nobox(ws, aux)?;
        for nf in &mut aux.nested_fns {
            crate::cleanup_with_nobox(ws, nf)?;
        }
    }
    crate::body_chain::debug_verify(&lowered.step, Stage::EMIT_READY, &"coroutine preparation");
    for aux in defers.iter().chain(hooks.iter().map(|(_, aux)| aux)) {
        crate::body_chain::debug_verify(aux, Stage::CONVERTED, &"coroutine preparation");
    }
    let volatile_vars = &ws.comptime_inputs.volatile_vars;
    let mut code_artifact =
        CodeArtifact::from_mir_tree_with_volatile_vars(&lowered.step, volatile_vars);
    code_artifact.set_body_names(&lowered.step, &step_name);
    if let Some(identity) = &lowered.step.code_identity {
        if let Some(entry) = code_artifact.entries.get_mut(identity) {
            entry.body_name = Some(Ustr::from(plan.base()));
            let mut slots = sorted_slots(&lowered);
            slots.dedup();
            entry.compatibility.frame = Some(FrameSchema { slots });
        }
    }
    for aux in defers.iter().chain(hooks.iter().map(|(_, aux)| aux)) {
        let mut artifact = CodeArtifact::from_mir_tree_with_volatile_vars(aux, volatile_vars);
        artifact.set_body_names(aux, aux.name.as_str());
        code_artifact.merge(artifact);
    }
    // Step, defer and hook regions share this coroutine's owner but computed
    // their lambda topologies from disjoint populations; present one value
    // per owner before the generated entries inherit the root's.
    code_artifact.unify_owner_topologies();
    let keys = add_coroutine_code_values(
        ws,
        interner,
        plan,
        &lowered,
        &mut code_artifact,
        &defers,
        &hooks,
    );
    Ok(PreparedCoroutine {
        lowered,
        step_name,
        named_nested,
        defers,
        hooks,
        code_artifact,
        keys,
    })
}

/// Convert the closures of each named nested `fn`, depth-first. The hoisted
/// bodies of one fn land in its own `nested_fns`, after its named children.
pub fn convert_named_nested_closures(nested: &mut [MirFunc], reserve: &dyn Fn(&str)) {
    for nf in nested {
        let named = nf.nested_fns.len();
        let name = nf.name;
        crate::convert_closures(nf, name.as_str());
        for child in &nf.nested_fns[named..] {
            reserve(&child.name);
        }
        convert_named_nested_closures(&mut nf.nested_fns[..named], reserve);
    }
}

fn sorted_slots(lowered: &CoroLowered) -> Vec<u32> {
    let mut slots: Vec<u32> = lowered.frame_plan.slots().values().copied().collect();
    slots.sort_unstable();
    slots
}

fn collect_nested_code_identities(mf: &MirFunc, out: &mut HashSet<CodeIdentity>) {
    for nested in &mf.nested_fns {
        if let Some(identity) = &nested.code_identity {
            out.insert(identity.clone());
        }
        collect_nested_code_identities(nested, out);
    }
}

fn hook_registration_site(
    source: &MirFunc,
    kind: CoroHookKind,
    entry: Idx<Block>,
) -> Option<LambdaSiteId> {
    source.insts.iter().find_map(|(_, inst)| {
        let Inst::CoroHook {
            kind: registered,
            lambda,
        } = inst
        else {
            return None;
        };
        if *registered != kind {
            return None;
        }
        let Inst::Lambda {
            entry: lambda_entry,
            ..
        } = &source.insts[*lambda]
        else {
            return None;
        };
        if *lambda_entry != entry {
            return None;
        }
        source
            .inst_spans
            .get(*lambda)
            .copied()
            .and_then(LambdaSiteId::from_span)
    })
}

fn add_coroutine_code_values(
    ws: &Workspace,
    interner: &skull_mir::monoty::MonoInterner,
    plan: &CoroutinePlan,
    lowered: &CoroLowered,
    artifact: &mut CodeArtifact,
    defers: &[MirFunc],
    hooks: &[(CoroHookKind, MirFunc)],
) -> CoroutineKeys {
    let root = lowered.step.code_identity.as_ref().expect("co fn identity");
    let owner = root.owner().expect("co fn owner");
    let root_compatibility = artifact
        .entries
        .get(root)
        .expect("co fn artifact entry")
        .compatibility
        .clone();
    let generated_compatibility = |role: &str| CodeCompatibility {
        call_abi: format!("coroutine:{role};{}", root_compatibility.call_abi),
        capture: CaptureSchema::empty(),
        frame: root_compatibility.frame.clone(),
        lambda_topology: root_compatibility.lambda_topology,
    };
    let mut generated = |role: &str, body: &str| {
        let identity = CodeIdentity::Generated {
            enclosing: owner.clone(),
            role: Ustr::from(role),
        };
        artifact.add_generated_value(identity.clone(), generated_compatibility(role), body);
        identity.logical_key(ws, interner)
    };
    let resume = generated("resume", &plan.resume());
    let advance = plan.advance().map(|body| generated("advance", &body));
    let mut value_key = |aux: &MirFunc, what: &str| {
        let identity = aux
            .code_identity
            .clone()
            .unwrap_or_else(|| panic!("{what} identity"));
        artifact.mark_value(identity.clone());
        identity.logical_key(ws, interner)
    };
    let defers = defers.iter().map(|aux| value_key(aux, "defer")).collect();
    let hooks = hooks
        .iter()
        .map(|(_, aux)| value_key(aux, "hook"))
        .collect();
    CoroutineKeys {
        advance,
        resume,
        defers,
        hooks,
    }
}
