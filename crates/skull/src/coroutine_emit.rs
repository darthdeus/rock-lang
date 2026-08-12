//! Lua emission for a transformed `co fn`: the stackless `__step` (via the dumb
//! printer, with a frame context) + the stdlib Coroutine constructor. The
//! MIR->MIR transform itself lives in `skull_transform::transform_coroutine`;
//! this module is the Lua-printing consumer (unfuck #57: the transform crate is
//! printer-free).

use bone_semantic::inference::Workspace;
use comfy_la_arena::Idx;
use rustc_hash::FxHashSet as HashSet;
use skull_codegen_lua::{emit_named_framed, CodegenCtx, FrameCtx};
use skull_mir::lower::CoroutinePlan;
use skull_mir::mir::*;
use skull_transform::{CoroLowered, FrameInfo, STATE};

fn raw<T>(i: Idx<T>) -> u32 {
    i.into_raw().into()
}

/// Emit a `co fn` as its stackless `__step` (via the dumb printer, with a frame
/// context) + the stdlib Coroutine constructor.
///
/// Every symbol comes from `plan`, which is the JS emitter's input too: a
/// specialized instantiation is a distinct definition under a distinct base, and
/// a base spelled here from the DECLARATION would give every instantiation of a
/// generic `co fn` the same `__step`.
pub fn emit_coroutine(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &skull_mir::monoty::MonoInterner,
    plan: &CoroutinePlan,
    func: &MirFunc,
) -> String {
    let name = plan.base().to_string();
    // A co fn yielding a flat enum threads through the frame-slot channel (0 alloc
    // per yield); the channel's shape drives both the slot writes (transform) and
    // the generic/boxed `resume()` rebuild (constructor).
    let channel = plan.channel.as_ref();
    let mut lowered = skull_transform::transform_coroutine(func, channel);
    let frame = FrameCtx {
        s_cell: lowered.s_cell,
        fields: lowered.frame_plan.fields().iter().copied().collect(),
        slots: lowered.frame_plan.slots().clone(),
        hoist_lambdas: true,
    };
    // Hooks/defers keep the pre-P1 emission (D5 of
    // docs/coroutine-frame-lambda-plan.md): an ALL-cells aux frame with
    // `hoist_lambdas: false`. THE INVARIANT: with hoisting off no cell is ever
    // hoisted to a lambda body, so a cell that is NOT a frame field would render
    // as a bare `write_cell_local` (`c<raw>`) -- an undeclared local (these bodies
    // never bind locals: they are flat detached regions of the step). Making EVERY
    // step cell (minus `s`/`resume_arg`, which a nullary terminal/pre-step body
    // never references) a frame field is what keeps every reference resolving to
    // `s.c<slot>`. This is why ROOT-forcing the hook/defer-owned cells (D5's
    // alternative) is unnecessary AND would be wrong here: no ownership pass runs
    // for the aux frame (the no-hoist path skips ScopeTree/Ownership entirely), and
    // the ownership-based field set the STEP uses would shrink `fields` to co-fn
    // state -- declaring every lambda-owned cell as a dead local in each body
    // (LuaJIT 200-local risk). All-cells-framed keeps zero locals and is
    // byte-identical to what these bodies have always produced.
    let resume_arg = func.resume_arg.expect("co fn must carry a resume-arg cell");
    // Aux slots: SHARED frame cells (co-fn state a hook body reads) MUST reuse
    // the step's slots -- the hook reads the same field the ctor wrote
    // (`CoroLowered::aux_all_cells_slots`, shared with the JS path).
    let aux_slots = lowered.aux_all_cells_slots(resume_arg);
    let aux_fields: HashSet<Idx<Cell>> = aux_slots.keys().copied().collect();
    let aux_frame = FrameCtx {
        s_cell: lowered.s_cell,
        fields: aux_fields,
        slots: aux_slots,
        hoist_lambdas: false,
    };
    // `__resume`/`__advance` are minted by `emit_constructor` from the plan --
    // check them here (the only place with `ctx`) for their collision side-effect.
    // Whether an `__advance` exists is the plan's answer, not a second reading of
    // the channel.
    ctx.mint_synthetic(plan.resume());
    if let Some(advance) = plan.advance() {
        ctx.mint_synthetic(advance);
    }
    let step_name = ctx.mint_synthetic(plan.step());
    // S3 (docs/closure-conversion-mir-plan.md): route the STEP's lambdas through
    // the shared closure-conversion pass, with the frame so a captured frame cell
    // collapses to a capture of `s` (D2/D4(b) of docs/coroutine-frame-lambda-plan.md).
    // Each lambda becomes a `MakeEnv` record / bare `Global` + a hoisted sibling in
    // `step.nested_fns` (emitted below). Detached hook/defer lambdas (unreachable
    // from the step entry) are left as `Inst::Lambda` for their aux emission's
    // inline arm -- `aux_frame` was built above, before the pass mutated the arena.
    let frame_info = FrameInfo {
        s_cell: lowered.s_cell,
        fields: lowered.frame_plan.fields().iter().copied().collect(),
        slots: lowered.frame_plan.slots().clone(),
    };
    let pre_nested = lowered.step.nested_fns.len();
    skull_transform::convert_closures_framed(
        &mut lowered.step,
        &step_name,
        Some(&frame_info),
        Some((&lowered.step_tree, &lowered.step_own)),
    );
    // Route each pass-hoisted `<step>__lambda<k>` through the synthetic-name
    // collision guard (a user fn colliding with the minted name must reject).
    for nf in lowered.step.nested_fns[pre_nested..].iter() {
        ctx.mint_synthetic(nf.name.to_string());
    }
    let step_lua =
        emit_named_framed(ws, ctx, interner, &step_name, &lowered.step, Some(&frame), None);
    // Hoisted lambda bodies: frame-free sibling fns (the pass materialized their
    // frame accesses as `e.c<S>.c<slot>`, so they need no `FrameCtx`). cleanup
    // drops the clone's dead ROOT-region insts; a co fn's loops are recovered by
    // codegen's relooper (`recognize_counted_loops`), same as the step -- so no
    // `recover_counted_loops` transform here (unlike an ordinary fn's nested_fns).
    let mut nested_luas = String::new();
    let mut hoisted = std::mem::take(&mut lowered.step.nested_fns);
    for nf in hoisted[pre_nested..].iter_mut() {
        skull_transform::cleanup_with_nobox(ws, nf);
        nested_luas
            .push_str(&skull_codegen_lua::emit_named(ws, ctx, interner, nf.name.as_str(), nf));
    }
    lowered.step.nested_fns = hoisted;
    // Terminal-cleanup defers and lifecycle-hook lambdas
    // (`on_killed`/`on_resume`/`on_hot_reload`) share one emission shape: a
    // detached region of the step (`if flag { flag = false; <body> }` for a
    // defer, the hook lambda's body for a hook) rendered as a standalone
    // `<name>...(s)` fn over the step's OWN inst/cell/block arena + the all-cells
    // aux FrameCtx. The entry override enters the shared step at the region's
    // block with `params = [s]` -- no per-body `step.clone()` (which deep-copied
    // the whole arena just to swap entry/params). A hook's only upvalue is the
    // frame `s`, so every captured cell reaches as `c<s>.c<slot>`; the ctor stores
    // both kinds by value (`__defers` table / `__on_*` fields) so a stale
    // coroutine runs its own layout's cleanup, zero closures per creation.
    // `__rock_coroutine_run_defers` iterates the defers LIFO with per-body pcall.
    let s_params = [lowered.s_cell];
    let emit_aux = |aux_name: &str, entry: Idx<Block>| {
        emit_named_framed(
            ws,
            ctx,
            interner,
            aux_name,
            &lowered.step,
            Some(&aux_frame),
            Some((entry, &s_params)),
        )
    };
    let mut defer_luas = String::new();
    for (k, (_, entry)) in lowered.step.coro_defers.iter().enumerate() {
        let dname = ctx.mint_synthetic(plan.defer(k));
        defer_luas.push_str(&emit_aux(&dname, *entry));
    }
    let mut hook_luas = String::new();
    for (kind, lambda) in &lowered.hooks {
        // Reject, don't fall back: the ctor unconditionally emits
        // `__on_<hook> = <name>__on_<hook>`, so silently skipping here would
        // leave that global undefined and the hook nil at fire time.
        let Inst::Lambda { params, entry } = &lowered.step.insts[*lambda] else {
            panic!("co-fn hook {} of `{name}` must be an Inst::Lambda", kind.slot())
        };
        assert!(params.is_empty(), "co-fn lifecycle hooks are nullary (`{name}`)");
        let hname = ctx.mint_synthetic(plan.hook(kind.slot()));
        hook_luas.push_str(&emit_aux(&hname, *entry));
    }
    let ctor = emit_constructor(plan, &step_name, &lowered);
    let _ = lowered.num_states;
    format!("{step_lua}{nested_luas}{defer_luas}{hook_luas}{ctor}")
}

/// The constructor: build the state table (seed param fields + relocated
/// on_killed/on_resume hooks) and return IT as the handle -- the object IS the
/// frame, with `__`-prefixed method fields (`__resume`/`__result`/
/// `__last_error`/`__status`/`__kill`/`__fire_hot_reload`, plus `__advance` on
/// the frame-slot channel) driving the shared `<name>__step`. Method fields
/// can't collide with user state: frame cells render as `c<N>`/`v<N>`. The
/// state table is named `c<raw(s_cell)>` (NOT `s`) so a hook lambda
/// referencing a frame cell renders `c<s>.c<slot>` — the same name the
/// step's `FrameCtx` produces. A param's seeded field is `c<slot>` (its dense
/// frame slot), which is BOTH the ctor's Lua param name and the table field
/// name; the state-table local `c<raw(s_cell)>` keeps the raw id (a plain
/// local, `s` has no slot).
fn emit_constructor(plan: &CoroutinePlan, step_name: &str, lowered: &CoroLowered) -> String {
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
    // Terminal-cleanup defer fns, stored BY VALUE (site order; runners iterate
    // in reverse for LIFO) -- a stale frame must run ITS OWN layout's cleanup,
    // never the current code's (the step is name-resolved, cleanup is not).
    let n_defers = lowered.step.coro_defers.len();
    if n_defers > 0 {
        let list: Vec<String> = (0..n_defers).map(|k| plan.defer(k)).collect();
        init.push(format!("__defers = {{ {} }}", list.join(", ")));
    }

    // Lifecycle hooks are fn-value fields pointing at the per-co-fn top-level
    // hoisted bodies (`__on_killed = <name>__on_killed`, emitted by
    // `emit_coroutine`) -- a plain reference, no per-construction closure. The
    // `if s.__on_killed then s.__on_killed(s) end` call sites pass the frame.
    for (kind, _) in &lowered.hooks {
        let field = kind.slot();
        init.push(format!("{field} = {}", plan.hook(field)));
    }

    // The pure readers (result/status/last_error/kill/fire_hot_reload) are NOT
    // fields: they live in the stdlib as shared fns reading the frame directly
    // (`c.__return`, `c.__state`, `__rock_coroutine_kill`). Only the step-bound
    // methods remain per-construction. Each takes the object explicitly
    // (`c.__X(c, ...)`) and reads ONLY its `s` param -- the bodies are already
    // shared-fn-shaped, so hoisting them out of the ctor (Part B closure
    // elimination) is a ctor-only change, no consumer churn.

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
            ty = skull_codegen_lua::lua_string(ch.enum_name.as_str()),
        );
        for i in 0..ch.tuple_width {
            let _ = write!(box_fields, ", s.{}", skull_mir::lower::yield_slot(1 + i));
        }
        for (j, rf) in ch.record_fields.iter().enumerate() {
            let _ = write!(box_fields, ", {rf} = s.{}", skull_mir::lower::yield_slot(1 + ch.tuple_width + j));
        }
        box_fields.push('}');
        // `__advance`/`__resume` are PER-CO-FN top-level fns (emitted once, next
        // to the step), stored in the table literal as plain fn-value fields --
        // zero closures per creation. Hot-reload staleness is identical to the
        // closure form: a live object's stale v0 fn values resolve the step (and
        // `{name}__advance`) by GLOBAL name at call time, so a BodyOnly overlay's
        // new step is picked up mid-flight (guarded by
        // `live_flat_enum_coroutine_survives_overlay_and_threads_new_body`).
        let advance = plan.advance().expect("a channel is what makes `__advance` exist");
        let resume = plan.resume();
        return format!(
            "function {advance}(s, arg)\n\
             \x20 if s.{STATE} < 0 then return true end\n\
             \x20 if s.__on_resume then s.__on_resume(s) end\n\
             \x20 local ok, err = pcall({step_name}, s, arg)\n\
             \x20 if not ok then s.{STATE} = -2; s.__error = err; if s.__inner then __rock_coroutine_kill(s.__inner); s.__inner = nil end; __rock_coroutine_run_defers(s); if s.__on_killed then s.__on_killed(s) end; return true end\n\
             \x20 return s.{STATE} < 0\n\
             end\n\
             function {resume}(s, arg)\n\
             \x20 if {advance}(s, arg) then return nil end\n\
             \x20 return {box_fields}\n\
             end\n\
             function {name}({paramlist})\n\
             \x20 local {svar} = {{ {fields}, __advance = {advance}, __resume = {resume} }}\n\
             \x20 return {svar}\n\
             end\n",
            paramlist = param_fields.join(", "),
            fields = init.join(", "),
        );
    }

    let resume = plan.resume();
    format!(
        "function {resume}(s, arg)\n\
         \x20 if s.{STATE} < 0 then return nil end\n\
         \x20 if s.__on_resume then s.__on_resume(s) end\n\
         \x20 local ok, val = pcall({step_name}, s, arg)\n\
         \x20 if not ok then s.{STATE} = -2; s.__error = val; if s.__inner then __rock_coroutine_kill(s.__inner); s.__inner = nil end; __rock_coroutine_run_defers(s); if s.__on_killed then s.__on_killed(s) end; return nil end\n\
         \x20 return val\n\
         end\n\
         function {name}({paramlist})\n\
         \x20 local {svar} = {{ {fields}, __resume = {resume} }}\n\
         \x20 return {svar}\n\
         end\n",
        paramlist = param_fields.join(", "),
        fields = init.join(", "),
    )
}
