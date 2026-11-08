//! Coroutine bodies and the JS coroutine constructor.

use super::*;

/// The code identities of `mf` and its nested bodies. Volatile build values
/// are metadata, so a changed build clock does not read as a body edit.
pub(super) fn js_code_artifact(ws: &Workspace, mf: &MirFunc) -> CodeArtifact {
    CodeArtifact::from_mir_tree_with_volatile_vars(mf, &ws.comptime_inputs.volatile_vars)
}

/// Emit a `co fn` for JS: the shared preparation
/// ([`skull_transform::prepare_coroutine`]), `materialize_frame` on the step,
/// then the trampoline step, hoisted-lambda siblings, hook/defer aux
/// trampolines over the shared arena, and the JS ctor (the exact Lua ctor
/// contract: the object IS the frame; `__resume`/`__advance` drive the step;
/// `JS_CORO_RUNTIME` supplies `__rock_coroutine_kill`/
/// `__rock_coroutine_run_defers`/`error`).
pub(super) fn emit_coroutine_js(
    ws: &Workspace,
    interner: &skull_mir::monoty::MonoInterner,
    plan: &skull_mir::lower::CoroutinePlan,
    concrete: &skull_mir::concrete::ConcreteBodyPlan,
    mf: &MirFunc,
    callees: &CalleeNames,
    hoisted: &HashSet<ustr::Ustr>,
    globals: &GlobalCtx,
) -> Result<(String, CodeArtifact), CompilerError> {
    let resume_arg = mf
        .resume_arg
        .ok_or_else(|| err("co fn must carry a resume-arg cell"))?;
    let source = concrete
        .function_instance()
        .ok_or_else(|| err("a coroutine emission plan must own a function instance"))?
        .func;
    let source_func = &ws.module(source.module).functions[source.func];
    let skull_transform::PreparedCoroutine {
        mut lowered,
        step_name,
        named_nested,
        defers,
        hooks,
        code_artifact,
        keys,
    } = skull_transform::prepare_coroutine(ws, interner, source_func, plan, mf, &|_| {})?;
    // This coroutine's hoisted lambda names are referenced only from its own
    // bodies, so extend the program set locally.
    let mut hoisted = hoisted.clone();
    let regions: Vec<&MirFunc> = std::iter::once(&lowered.step)
        .chain(defers.iter())
        .chain(hooks.iter().map(|(_, aux)| aux))
        .collect();
    hoisted.extend(collect_hoisted(&regions));
    // Materialize only the root region; detached regions were materialized when
    // they were copied into ordinary MirFuncs.
    let step_entry = lowered.step.entry;
    skull_transform::materialize_frame(
        &mut lowered.step,
        step_entry,
        lowered.s_cell,
        lowered.frame_plan.slots(),
    );
    let mut globals = globals.clone();
    register_code_slots(ws, interner, &lowered.step, &mut globals);
    for aux in defers.iter().chain(hooks.iter().map(|(_, aux)| aux)) {
        register_code_slots(ws, interner, aux, &mut globals);
    }
    let mut out = emit_fn_trampoline(
        ws,
        &lowered.step,
        &step_name,
        callees,
        &hoisted,
        &globals,
        lowered.step.entry,
        &[lowered.s_cell, resume_arg],
        false,
    )?;
    // Named nested fns finish like an ordinary body's; hoisted lambda siblings
    // are frame-free ordinary fns (their frame accesses are already
    // `e.c<S>.c<slot>` MIR) -- the normal structured path.
    let mut nested = std::mem::take(&mut lowered.step.nested_fns);
    for nf in &mut nested[..named_nested] {
        skull_transform::cleanup_with_nobox(ws, nf)?;
        skull_transform::recover_counted_loops(ws, nf);
    }
    let bodies = nested
        .iter()
        .chain(defers.iter().chain(hooks.iter().map(|(_, aux)| aux)));
    for body in bodies {
        out.push_str(&emit_body_with_nested(
            ws,
            body,
            body.name.as_str(),
            callees,
            &hoisted,
            &globals,
        )?);
    }
    lowered.step.nested_fns = nested;
    out.push_str(&emit_js_constructor(
        plan,
        &step_name,
        &lowered,
        keys.advance.as_deref(),
        &keys.resume,
        &keys.defers,
        &keys.hooks,
    )?);
    Ok((out, code_artifact))
}

/// The JS Coroutine constructor + per-co-fn `__resume` (and `__advance` on the
/// frame-slot channel), mirroring the Lua ctor contract field for field. The
/// `try/catch` is the `pcall`; `undefined -> null` on the step's value so a
/// bare terminal return reads as nil.
fn emit_js_constructor(
    plan: &skull_mir::lower::CoroutinePlan,
    step_name: &str,
    lowered: &skull_transform::CoroLowered,
    advance_key: Option<&str>,
    resume_key: &str,
    defer_keys: &[String],
    hook_keys: &[String],
) -> Result<String, CompilerError> {
    use std::fmt::Write as _;
    let name = plan.base();
    let channel = plan.channel.as_ref();
    let param_fields: Vec<String> = lowered
        .params
        .iter()
        .map(|p| {
            lowered
                .frame_plan
                .slot(*p)
                .map(|s| format!("c{s}"))
                .ok_or_else(|| err("co-fn param must be a frame cell with a slot"))
        })
        .collect::<Result<_, _>>()?;
    let mut init = vec![
        "__ty: \"Coroutine\"".to_string(),
        "__state: 0".to_string(),
        format!("__layout: {}", lowered.frame_plan.layout_hash()),
    ];
    for pf in &param_fields {
        init.push(format!("{pf}: {pf}"));
    }
    if !defer_keys.is_empty() {
        let list: Vec<String> = defer_keys
            .iter()
            .map(|key| Ok(format!("__rock_code_slot_ref({})", js_string_lit(key)?)))
            .collect::<Result<_, CompilerError>>()?;
        init.push(format!("__defers: [{}]", list.join(", ")));
    }
    for ((kind, _), key) in lowered.hooks.iter().zip(hook_keys) {
        let field = kind.slot();
        init.push(format!(
            "{field}: __rock_code_slot_ref({})",
            js_string_lit(key)?
        ));
    }
    // The step-raise recovery shared by both resume shapes: errored terminal,
    // kill the delegated inner, run defers LIFO, fire on_killed.
    let catch_body = "s.__state = -2; s.__error = (e && e.message !== undefined) ? e.message : e; \
                      if (s.__inner) { __rock_coroutine_kill(s.__inner); s.__inner = null; } \
                      __rock_coroutine_run_defers(s); \
                      if (s.__on_killed) { __rock_call(s.__on_killed, s); }";
    if let Some(ch) = channel {
        let mut box_fields = format!(
            "{{ __variant: s.{tag}, __ty: {ty}",
            tag = skull_mir::lower::yield_slot(0),
            ty = js_string_lit(ch.enum_name.as_str())?,
        );
        // 0-based positional keys: JS `TupleIndex` reads `base[i]` (no +1 like Lua's
        // array keys), so a boxed-enum consumer of a yielded channel value projects
        // the payload correctly. Matches `MakeEnum`'s native 0-based keys.
        for i in 0..ch.tuple_width {
            let _ = write!(
                box_fields,
                ", \"{}\": s.{}",
                i,
                skull_mir::lower::yield_slot(1 + i)
            );
        }
        for (j, rf) in ch.record_fields.iter().enumerate() {
            let _ = write!(
                box_fields,
                ", {rf}: s.{}",
                skull_mir::lower::yield_slot(1 + ch.tuple_width + j)
            );
        }
        box_fields.push_str(" }");
        let advance = plan
            .advance()
            .expect("a channel is what makes `__advance` exist");
        let resume = plan.resume();
        let advance_ref = format!(
            "__rock_code_slot_ref({})",
            js_string_lit(advance_key.expect("advance CodeRef"))?
        );
        let resume_ref = format!("__rock_code_slot_ref({})", js_string_lit(resume_key)?);
        return Ok(format!(
            "function {advance}(s, arg) {{\n\
             \x20 if (s.__state < 0) return true;\n\
             \x20 if (s.__on_resume) __rock_call(s.__on_resume, s);\n\
             \x20 try {{ {step_name}(s, arg); }} catch (e) {{ {catch_body} return true; }}\n\
             \x20 return s.__state < 0;\n\
             }}\n\
             function {resume}(s, arg) {{\n\
             \x20 if ({advance}(s, arg)) return null;\n\
             \x20 return {box_fields};\n\
             }}\n\
             function {name}({paramlist}) {{\n\
             \x20 return {{ {fields}, __advance: {advance_ref}, __resume: {resume_ref} }};\n\
             }}\n",
            paramlist = param_fields.join(", "),
            fields = init.join(", "),
            advance_ref = advance_ref,
            resume_ref = resume_ref,
        ));
    }
    let resume = plan.resume();
    let resume_ref = format!("__rock_code_slot_ref({})", js_string_lit(resume_key)?);
    Ok(format!(
        "function {resume}(s, arg) {{\n\
         \x20 if (s.__state < 0) return null;\n\
         \x20 if (s.__on_resume) __rock_call(s.__on_resume, s);\n\
         \x20 var val;\n\
         \x20 try {{ val = {step_name}(s, arg); }} catch (e) {{ {catch_body} return null; }}\n\
         \x20 return val === undefined ? null : val;\n\
         }}\n\
         function {name}({paramlist}) {{\n\
         \x20 return {{ {fields}, __resume: {resume_ref} }};\n\
         }}\n",
        paramlist = param_fields.join(", "),
        fields = init.join(", "),
        resume_ref = resume_ref,
    ))
}
