//! Lua emission for a transformed `co fn`: the stackless `__step` (via the dumb
//! printer, with a frame context) + the stdlib Coroutine constructor. The
//! MIR->MIR transform itself lives in `skull_transform::transform_coroutine`;
//! this module is the Lua-printing consumer (unfuck #57: the transform crate is
//! printer-free).

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::CompilerError;
use comfy_la_arena::Idx;
use rustc_hash::FxHashSet;
use skull_codegen_lua::{
    CodegenCtx, FrameCtx, LuaEmission, emit_lua_definition, emit_named_framed,
};
use skull_mir::code_identity::CodeArtifact;
use skull_mir::lower::CoroutinePlan;
use skull_mir::mir::*;
use skull_transform::{CoroLowered, PreparedCoroutine, STATE};

fn raw<T>(i: Idx<T>) -> u32 {
    i.into_raw().into()
}

pub(crate) fn emit_coroutine_with_artifact(
    ws: &Workspace,
    ctx: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    source_func: &bone_semantic::hir::Function,
    plan: &CoroutinePlan,
    func: &MirFunc,
) -> Result<
    (
        String,
        CodeArtifact,
        Vec<crate::allocation_report::AllocationBody>,
        FxHashSet<FuncRef>,
        Vec<StaticCell>,
    ),
    CompilerError,
> {
    let interner = scx.interner();
    let PreparedCoroutine {
        mut lowered,
        step_name,
        named_nested,
        defers,
        hooks,
        code_artifact,
        keys,
    } = skull_transform::prepare_coroutine(ws, interner, source_func, plan, func, &|name| {
        ctx.mint_synthetic(name.to_string());
    })?;
    let frame = FrameCtx {
        s_cell: lowered.s_cell,
        fields: lowered.frame_plan.fields().iter().copied().collect(),
        slots: lowered.frame_plan.slots().clone(),
    };
    let mut out = CoroutineLua {
        ws,
        ctx,
        interner,
        lua: String::new(),
        allocation_bodies: Vec::new(),
    };
    out.emit(&step_name, plan.base(), &lowered.step, Some(&frame));
    // Named nested fns take the ordinary nested-fn path; hoisted lambda bodies
    // are frame-free siblings (their frame accesses are already
    // `e.c<S>.c<slot>` MIR), so they need no `FrameCtx`.
    let mut nested_deps = FxHashSet::default();
    let mut nested_statics = lowered.step.statics.clone();
    let mut nested_times = crate::mir_emit::PhaseTimes::default();
    let mut hoisted = std::mem::take(&mut lowered.step.nested_fns);
    let (named, lambdas) = hoisted.split_at_mut(named_nested);
    crate::mir_emit::emit_nested_fns(
        ws,
        ctx,
        scx,
        named,
        &lowered.step.statics,
        &mut nested_deps,
        &mut nested_statics,
        &mut out.allocation_bodies,
        &mut out.lua,
        &mut nested_times,
    )?;
    for nf in lambdas.iter() {
        out.emit(&nf.name, &nf.name, nf, None);
    }
    lowered.step.nested_fns = hoisted;
    // Terminal-cleanup defers and lifecycle hooks are ordinary functions, with
    // their converted lambda siblings emitted beside them.
    for aux in defers.iter().chain(hooks.iter().map(|(_, aux)| aux)) {
        out.emit(&aux.name, &aux.name, aux, None);
        for nf in &aux.nested_fns {
            out.emit(&nf.name, &nf.name, nf, None);
        }
    }
    out.lua.push_str(&emit_constructor(
        plan,
        &step_name,
        &lowered,
        keys.advance.as_deref(),
        &keys.resume,
        &keys.defers,
        &keys.hooks,
    ));
    Ok((
        out.lua,
        code_artifact,
        out.allocation_bodies,
        nested_deps,
        nested_statics,
    ))
}

struct CoroutineLua<'a> {
    ws: &'a Workspace,
    ctx: &'a CodegenCtx,
    interner: &'a skull_mir::monoty::MonoInterner,
    lua: String,
    allocation_bodies: Vec<crate::allocation_report::AllocationBody>,
}

impl CoroutineLua<'_> {
    fn emit(
        &mut self,
        emitted_name: &str,
        diagnostic_name: &str,
        func: &MirFunc,
        frame: Option<&FrameCtx>,
    ) {
        let emission = emit_coroutine_body(
            self.ws,
            self.ctx,
            self.interner,
            emitted_name,
            diagnostic_name,
            func,
            frame,
        );
        self.lua.push_str(&emission.lua);
        if let Some(body) = emission.body {
            self.allocation_bodies.push(body);
        }
    }
}

struct CoroutineBodyEmission {
    lua: String,
    body: Option<crate::allocation_report::AllocationBody>,
}

fn emit_coroutine_body(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &skull_mir::monoty::MonoInterner,
    emitted_name: &str,
    diagnostic_name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
) -> CoroutineBodyEmission {
    if !crate::allocation_report::enabled() {
        return CoroutineBodyEmission {
            // A coroutine `__step` is not a declaration of its own, so it
            // has no call-graph component and binds no callees.
            lua: emit_named_framed(ws, ctx, interner, emitted_name, func, frame, None),
            body: None,
        };
    }
    let LuaEmission {
        lua,
        allocation_sites,
    } = emit_lua_definition(
        ws,
        ctx,
        interner,
        emitted_name,
        func,
        frame,
        &[],
        None,
        Some(crate::allocation_report::activation_enabled()),
    );
    let body = crate::allocation_report::collect_emitted_body(
        ws,
        interner,
        emitted_name,
        diagnostic_name,
        func,
        allocation_sites,
    );
    CoroutineBodyEmission { lua, body }
}

/// Construct the frame with step-bound resume/advance CodeRefs. Frame cells
/// use dense `c<N>` slots; the frame local keeps its raw cell id so detached
/// hooks and the step address the same fields.
fn emit_constructor(
    plan: &CoroutinePlan,
    step_name: &str,
    lowered: &CoroLowered,
    advance_key: Option<&str>,
    resume_key: &str,
    defer_keys: &[String],
    hook_keys: &[String],
) -> String {
    use std::fmt::Write as _;
    let name = plan.base();
    let channel = plan.channel.as_ref();
    let svar = format!("c{}", raw(lowered.s_cell));
    let param_fields: Vec<String> = lowered
        .params
        .iter()
        .map(|p| {
            format!(
                "c{}",
                lowered
                    .frame_plan
                    .slot(*p)
                    .expect("co-fn param must be a frame cell with a slot")
            )
        })
        .collect();
    let mut init = vec![
        "__ty = \"Coroutine\"".to_string(),
        format!("{STATE} = 0"),
        format!("__layout = {}", lowered.frame_plan.layout_hash()),
    ];
    for pf in &param_fields {
        init.push(format!("{pf} = {pf}"));
    }
    // Terminal-cleanup CodeRefs retain the old frame's logical body across a
    // compatible overlay; runners iterate them in reverse for LIFO.
    if !defer_keys.is_empty() {
        let list: Vec<String> = defer_keys
            .iter()
            .map(|key| {
                format!(
                    "__rock_code_slot_ref({})",
                    skull_codegen_lua::lua_string(key)
                )
            })
            .collect();
        init.push(format!("__defers = {{ {} }}", list.join(", ")));
    }

    // Lifecycle hook fields are logical CodeRefs, not copied backend bodies.
    for ((kind, _), key) in lowered.hooks.iter().zip(hook_keys) {
        let field = kind.slot();
        init.push(format!(
            "{field} = __rock_code_slot_ref({})",
            skull_codegen_lua::lua_string(key)
        ));
    }

    let step_call = if lowered.needs_protected_resume {
        format!("__rock_coroutine_resume({step_name}, s, arg)")
    } else {
        format!("{step_name}(s, arg)")
    };

    // Frame-slot channel: the step writes yield leaves to reused `s.__y*` slots
    // and returns nothing. The OBJECT IS THE FRAME (one table per creation):
    // `__advance(s, arg)` steps (returns done-bool) and a concrete consumer reads
    // the `__y*` leaves threaded off the object itself (0 alloc). The
    // generic/boxed `__resume` rebuilds one `{__variant}` box from the slots --
    // same external `Y?` for erased consumers (correct, allocates, rare).
    if let Some(ch) = channel {
        let mut box_fields = format!(
            "{{__variant = s.{tag}, __ty = {ty}",
            tag = skull_mir::lower::yield_slot(0),
            ty =
                skull_codegen_lua::lua_string(skull_mir::mangle::symbol_of_key(ch.name()).as_str()),
        );
        for i in 0..ch.tuple_width() {
            let _ = write!(box_fields, ", s.{}", skull_mir::lower::yield_slot(1 + i));
        }
        for (j, rf) in ch.record_fields().iter().enumerate() {
            let _ = write!(
                box_fields,
                ", {rf} = s.{}",
                skull_mir::lower::yield_slot(1 + ch.tuple_width() + j)
            );
        }
        box_fields.push('}');
        // Seed every yield slot in the literal and clear it to nil, so each
        // slot has a hash node from birth. The step then only ever writes
        // PRESENT keys: yield writes do not grow or rehash it under a trace, a
        // frame that stays inside one trace can be sunk (measured 196 -> 2 ns
        // per short-lived coroutine; 222 -> 108 ns when the handle escapes),
        // and a nil store into a full frame cannot hit the repeated-rehash
        // path (docs/luajit-performance.md). The clears keep the seeds
        // invisible: a nil-valued node does not appear to pairs or snapshots,
        // and no slot is readable before the first yield writes it. The
        // template is copied by one TDUP either way. Resumed frames use
        // the same memory; a wide frame never resumed can be larger because
        // growth happens at creation. The step's nil-write guards stay: a frame
        // rebuilt by a host copy (snapshot restore) has no seeded nodes.
        let slot_count = ch.leaf_count();
        let mut clears = String::new();
        for i in 0..slot_count {
            let slot = skull_mir::lower::yield_slot(i);
            init.push(format!("{slot} = false"));
            let _ = write!(clears, " {svar}.{slot} = nil");
        }
        let advance = plan
            .advance()
            .expect("a channel is what makes `__advance` exist");
        let resume = plan.resume();
        let advance_ref = format!(
            "__rock_code_slot_ref({})",
            skull_codegen_lua::lua_string(advance_key.expect("advance CodeRef"))
        );
        let resume_ref = format!(
            "__rock_code_slot_ref({})",
            skull_codegen_lua::lua_string(resume_key)
        );
        return format!(
            "function {advance}(s, arg)\n\
             \x20 if s.{STATE} < 0 then return true end\n\
             \x20 {step_call}\n\
             \x20 return s.{STATE} < 0\n\
             end\n\
             function {resume}(s, arg)\n\
             \x20 if {advance}(s, arg) then return nil end\n\
             \x20 return {box_fields}\n\
             end\n\
             function {name}({paramlist})\n\
             \x20 local {svar} = {{ {fields}, __advance = {advance_ref}, __resume = {resume_ref} }}\n\
             \x20{clears}\n\
             \x20 return {svar}\n\
             end\n",
            paramlist = param_fields.join(", "),
            fields = init.join(", "),
            advance_ref = advance_ref,
            resume_ref = resume_ref,
        );
    }

    let resume = plan.resume();
    let resume_ref = format!(
        "__rock_code_slot_ref({})",
        skull_codegen_lua::lua_string(resume_key)
    );
    format!(
        "function {resume}(s, arg)\n\
         \x20 if s.{STATE} < 0 then return nil end\n\
         \x20 return {step_call}\n\
         end\n\
         function {name}({paramlist})\n\
         \x20 local {svar} = {{ {fields}, __resume = {resume_ref} }}\n\
         \x20 return {svar}\n\
         end\n",
        paramlist = param_fields.join(", "),
        fields = init.join(", "),
        resume_ref = resume_ref,
    )
}
