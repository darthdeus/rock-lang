//! Single-function entry points: plain, trampolined and closure-carrying bodies.

use super::*;

/// Emit `mf` as `function <js_name>(...) { ... }`. `js_name` is the caller's
/// chosen JS identity (the differential harness names each fallback fn).
pub fn emit_fn(ws: &Workspace, mf: &MirFunc, js_name: &str) -> Result<String, CompilerError> {
    let names = CalleeNames::default();
    emit_fn_with_callees(
        ws,
        mf,
        js_name,
        &names,
        &HashSet::new(),
        &GlobalCtx::default(),
    )
}

/// [`emit_fn`] with a set of known co-emitted plain-Rock callees: a call whose
/// `FuncRef` is in `callees` renders as `<js name>(args)` (the caller promises
/// to emit that fn under that name in the same program). Anything neither a
/// mapped intrinsic nor a known callee still rejects.
pub(crate) fn emit_fn_with_callees(
    ws: &Workspace,
    mf: &MirFunc,
    js_name: &str,
    callees: &CalleeNames,
    hoisted: &HashSet<ustr::Ustr>,
    globals: &GlobalCtx,
) -> Result<String, CompilerError> {
    if mf.is_coroutine {
        return Err(err("JS subset: coroutine fn not supported"));
    }
    let (mut frame, unwind) = FnFrame::new(ws, mf, callees, hoisted, globals, mf.entry, false);
    let Some(shape) = placement::Shape::analyze(&frame.emitter, mf.entry) else {
        return emit_fn_trampoline(
            ws, mf, js_name, callees, hoisted, globals, mf.entry, &mf.params, true,
        );
    };
    let header = frame.header(js_name, &mf.params, true);
    placement::emit_body(&mut frame.emitter, &shape, mf.entry)?;
    Ok(format!(
        "{header}{}}}\n",
        defer_unwind_wrap(unwind, &frame.emitter.out)
    ))
}

/// Emit a region of `mf` as a block trampoline `for(;;) switch(__bb)` -- the
/// coroutine-step shape (dispatch-into-loops is irreducible by construction, so
/// the structured relooper cannot apply). `entry` is the region root (the step
/// entry, or a detached hook/defer region's block); `params` the emitted fn's
/// param cells (the step's `[s, resume_arg]`, an aux body's `[s]`). Case bodies
/// are braced so `let v<N>` temps stay case-local; values are same-block by the
/// value model + the transform's cross-block spill, so no cross-case temps exist.
pub fn emit_fn_trampoline(
    ws: &Workspace,
    mf: &MirFunc,
    js_name: &str,
    callees: &CalleeNames,
    hoisted: &HashSet<ustr::Ustr>,
    globals: &GlobalCtx,
    entry: Idx<Block>,
    params: &[Idx<Cell>],
    pack_multi_return: bool,
) -> Result<String, CompilerError> {
    let (frame, unwind) = FnFrame::new(ws, mf, callees, hoisted, globals, entry, pack_multi_return);
    let header = frame.header(js_name, params, false);
    let FnFrame {
        emitter: mut em,
        region,
    } = frame;
    em.push(&format!("var __bb = {};", mf.emit_block_id(entry)));
    em.push("for (;;) {");
    em.indent += 1;
    em.push("switch (__bb) {");
    em.indent += 1;
    let mut cases = region;
    cases.sort_by_key(|b| mf.emit_block_id(*b));
    for b in cases {
        em.push(&format!("case {}: {{", mf.emit_block_id(b)));
        em.indent += 1;
        em.emit_block_effects(b)?;
        em.emit_trampoline_term(b)?;
        em.indent -= 1;
        em.push("}");
    }
    em.indent -= 1;
    em.push("}");
    em.indent -= 1;
    em.push("}");
    Ok(format!(
        "{header}{}}}\n",
        defer_unwind_wrap(unwind, &em.out)
    ))
}

/// An [`Emitter`] over the blocks reachable from `entry`, with that region.
struct FnFrame<'a> {
    emitter: Emitter<'a>,
    region: Vec<Idx<Block>>,
}

impl<'a> FnFrame<'a> {
    /// The frame, and whether the region registers `defer` cleanups that need
    /// [`defer_unwind_wrap`].
    fn new(
        ws: &'a Workspace,
        mf: &'a MirFunc,
        callees: &'a CalleeNames,
        hoisted: &'a HashSet<ustr::Ustr>,
        globals: &'a GlobalCtx,
        entry: Idx<Block>,
        pack_multi_return: bool,
    ) -> (Self, bool) {
        let region = reachable_from(mf, entry);
        let (use_count, inlinable) = compute_inlinable(mf, &region);
        let hoisted_temps = cross_block_temps(mf, &region, &use_count, &inlinable);
        let unwind = mf.registers_defer_cleanup(&region);
        let emitter = Emitter {
            ws,
            mf,
            callees,
            hoisted,
            globals,
            use_count,
            inlinable,
            pack_multi_return,
            hoisted_temps,
            out: String::new(),
            indent: if unwind { 2 } else { 1 },
        };
        (Self { emitter, region }, unwind)
    }

    /// `function <js_name>(<params>) {` plus one hoisted `let` for the stored
    /// cells and the materialized values that cross block scopes: follower
    /// labels and trampoline cases add scopes outside the block that defines
    /// some of them. `coerce_uint` normalizes `uint` params on entry.
    fn header(&self, js_name: &str, params: &[Idx<Cell>], coerce_uint: bool) -> String {
        let (ws, mf) = (self.emitter.ws, self.emitter.mf);
        let names: Vec<String> = params.iter().map(|&c| cell_name(mf, c)).collect();
        let mut header = format!("function {js_name}({}) {{\n", names.join(", "));
        if coerce_uint {
            for &c in params {
                if cell_is_uint(ws, mf.cells[c].ty) {
                    let name = cell_name(mf, c);
                    header.push_str(&format!("  {name} = {name} >>> 0;\n"));
                }
            }
        }
        let param_set: HashSet<Idx<Cell>> = params.iter().copied().collect();
        let mut declared: Vec<u32> = Vec::new();
        let mut seen = HashSet::new();
        for &b in &self.region {
            for &iid in &mf.blocks[b].insts {
                mf.insts[iid].cell_writes_each(&mut |cell| {
                    if !param_set.contains(&cell) && seen.insert(cell) {
                        declared.push(mf.emit_cell_id(cell));
                    }
                });
            }
        }
        let mut hoisted_names: Vec<u32> = self
            .emitter
            .hoisted_temps
            .iter()
            .map(|&id| mf.emit_inst_id(Idx::from_raw(RawIdx::from_u32(id))))
            .collect();
        hoisted_names.sort_unstable();
        if !declared.is_empty() || !hoisted_names.is_empty() {
            declared.sort_unstable();
            let names: Vec<String> = declared
                .iter()
                .map(|id| format!("c{id}"))
                .chain(hoisted_names.iter().map(|id| format!("v{id}")))
                .collect();
            header.push_str(&format!("  let {};\n", names.join(", ")));
        }
        header
    }
}

/// A body that registers an ordinary-fn `defer` (`skull_mir::mir::DEFER_PUSH`)
/// catches whatever escapes it: `__rock_defer_unwind` runs the cleanups still
/// registered above the depth the body was entered at, LIFO, and rethrows. The
/// Lua backend gets the same protection by re-entering the body under
/// `__rock_defer_call`.
fn defer_unwind_wrap(unwind: bool, body: &str) -> String {
    if !unwind {
        return body.to_string();
    }
    format!(
        "  var __rock_defer_base = __rock_defer_top;\n  try {{\n{body}  \
         }} catch (__rock_e) {{ __rock_defer_unwind(__rock_defer_base, __rock_e); }}\n"
    )
}

/// The runtime entry points the shared lowering calls as globals; the JS
/// runtime defines them (`JS_DEFER_RUNTIME`, the coroutine runtime).
pub(super) fn is_runtime_global(name: &str) -> bool {
    name == skull_mir::mir::DEFER_PUSH
        || name == skull_mir::mir::DEFER_POP
        || name == skull_mir::mir::CORO_KILL
}

/// Deterministic JS name for a co-emitted callee -- the SHARED mangle keyed on
/// `(name, declared signature)`, identical to the Lua and GLSL backends. Exempt
/// (`extern`/`@nomangle`/`@test`/`main`/`test_*`) fns keep their bare name: the
/// discipline is "called by original name from inline bodies / compiler
/// codegen", and on JS an inline(js) body or a driver-injected line is verbatim
/// text that can only use the bare name (same rule as the lua backend).
pub(crate) fn callee_js_name(ws: &Workspace, fr: FuncRef) -> String {
    let f = &ws.module(fr.module).functions[fr.func];
    skull_mir::mangle::mangle_fn(ws, f)
}

/// Emit `roots` (each under its given JS name) plus the transitive plain-Rock
/// callee closure of each root, callees under [`callee_js_name`]s. Mapped
/// intrinsics stay call-site templates. Any callee that is neither (inline(lua)
/// without a mapping, extern) and any op outside the emitter subset rejects
/// with the emitter's `CompilerError` -- the caller turns that into SKIP(reason).
/// Function declarations hoist in JS, so definition order is irrelevant.
pub fn emit_with_closure(
    ws: &Workspace,
    roots: &[(FuncRef, String)],
) -> Result<String, CompilerError> {
    let scx = js_scx(ws);
    let mut refs: Vec<FuncRef> = roots.iter().map(|(fr, _)| *fr).collect();
    let mut ref_set: HashSet<FuncRef> = refs.iter().copied().collect();
    for shader_root in shader_root_refs(ws)? {
        if ref_set.insert(shader_root) {
            refs.push(shader_root);
        }
    }
    push_synthesized_entry_roots(ws, &mut refs, &mut ref_set);
    push_json_factory_roots(ws, &mut refs, &mut ref_set);
    let mut root_plan = JsRootPlan {
        named: roots.to_vec(),
        refs,
    };
    let session = loop {
        let session = new_js_session(ws, &scx, &root_plan.refs);
        if !extend_root_plan_from_inline_view(&mut root_plan, &session.view) {
            break session;
        }
    };
    let mut st = WorklistState::new(
        ws,
        &scx,
        &root_plan.named,
        false,
        &session.graph,
        &session.view,
        session.concrete.plans.as_ref(),
        &session.concrete.always_plans,
    )?;
    drain_worklists(ws, &scx, &mut st)?;
    st.canonicalize_emission_order();
    for (_, mf) in &st.lowered {
        register_code_slots(ws, scx.interner(), mf, &mut st.gctx);
    }
    for (_, mf) in &st.spec_lowered {
        register_code_slots(ws, scx.interner(), mf, &mut st.gctx);
    }
    for g in &st.ginits {
        register_code_slots(ws, scx.interner(), &g.mf, &mut st.gctx);
    }
    let mut rejections = std::mem::take(&mut st.rejections);
    let bodies = st.top_bodies();
    rejections.extend(check_scalar_abi(
        ws,
        &scx,
        &st.lowered,
        &st.spec_lowered,
        &bodies,
        st.concrete,
        &st.artifact_decls,
    ));
    let hoisted = collect_hoisted(&bodies);
    let struct_registry = skull_mir::lower::StructRegistryPlan::for_bodies(
        ws,
        bone_core::hir::FnBackend::Js,
        &bodies,
    )
    .map_err(|error| err(error.message))?;

    let mut out = String::new();
    let mut code_artifact = CodeArtifact::default();
    out.push_str(&emit_enum_singletons(&st.gctx, &st.enum_defs));
    out.push_str(&emit_inspect_enum_payloads(ws)?);
    out.push_str(&emit_vt_eq(ws, struct_registry.as_ref())?);
    out.push_str(&emit_tostring_ty(ws, struct_registry.as_ref())?);
    out.push_str(&emit_inspect_ty(ws, struct_registry.as_ref())?);
    out.push_str(&emit_inspect_cdata_enums(ws)?);
    out.push_str(&emit_col_mat(ws, struct_registry.as_ref())?);
    out.push_str(&ret_slot_decls(
        max_multi_wire(&bodies)?.max(max_published_lanes(&session.view, &st.inline_js)?),
    ));
    // Global initializers first (`function __ginit_<name>() {...}`), then the
    // load-time assignment lines in dependency order, then the fn defs. (The
    // program path reorders these; per-test order is pinned by the JS suite.)
    for g in &st.ginits {
        match emit_ginit_with_nested(ws, g, &st.names, &hoisted, &st.gctx) {
            Ok(js) => {
                out.push_str(&js);
                let mut artifact = js_code_artifact(ws, &g.mf);
                artifact.set_body_names(&g.mf, &g.init_name);
                code_artifact.merge(artifact);
            }
            Err(e) => rejections.push((g.init_name.clone(), e)),
        }
    }
    let init_lines = order_global_inits(&st.ginits, &st.lowered, &st.names, &st.gctx.state)?;
    // The init lines run LATE, after the code slots are staged (see below): a
    // global initialized from a lambda stores that lambda's CodeRef, so resolving
    // it needs the staged slot.
    let mut init_js = String::new();
    for init in &init_lines {
        init_js.push_str(&init.js);
    }
    for (fr, mf) in &st.lowered {
        let name = st
            .names
            .name_for_decl(ws, *fr)
            .expect("emitted body has a name");
        let plan = st
            .concrete
            .get(&Instance::base(*fr))
            .ok_or_else(|| err(format!("JS: no concrete plan for `{name}`")))?;
        match emit_lowered_fn(
            ws,
            scx.interner(),
            mf,
            name,
            plan,
            &st.names,
            &hoisted,
            &st.gctx,
        ) {
            Ok((js, artifact)) => {
                out.push_str(&js);
                code_artifact.merge(artifact);
            }
            Err(e) => rejections.push((name.clone(), e)),
        }
    }
    out.push_str(&emit_spec_bodies(
        ws,
        scx.interner(),
        &st,
        &hoisted,
        &mut rejections,
    ));
    combine_rejections(rejections)?;
    let code_slot_stage = code_slot_registration_js(ws, scx.interner(), &code_artifact)?;
    out.push_str(&code_slot_stage);
    for body in &st.inline_js {
        out.push_str(&emit_inline_js_fn(
            ws,
            &scx,
            st.concrete,
            &body.instance,
            body.decl,
            st.names
                .get(&body.instance)
                .expect("emitted body has a name"),
        )?);
    }
    out.push_str(&init_js);
    out.push_str(code_slot_publish_js(&code_artifact, false, false));
    // A closure that reads and writes no mutable global or static runs without
    // the globals runtime (the per-construct harnesses rely on it).
    if !st.gctx.uses_storage.get() {
        return Ok(out);
    }
    let global_layout = skull_mir::global_storage::global_layout_id(ws, &scx).map_err(err)?;
    Ok(format!(
        "{}{out}",
        globals_open_js(ws, &scx, &st.gctx.state, &global_layout)?
    ))
}

/// One non-fn body (decorator definition, global initializer) plus its hoisted
/// lambda bodies, as one string -- so a rejection anywhere in it drops the whole
/// definition rather than half of it.
pub(super) fn emit_body_with_nested(
    ws: &Workspace,
    mf: &MirFunc,
    js_name: &str,
    names: &CalleeNames,
    hoisted: &HashSet<ustr::Ustr>,
    gctx: &GlobalCtx,
) -> Result<String, CompilerError> {
    let mut out = emit_fn_with_callees(ws, mf, js_name, names, hoisted, gctx)?;
    for nf in &mf.nested_fns {
        out.push_str(&emit_body_with_nested(
            ws,
            nf,
            nf.name.as_str(),
            names,
            hoisted,
            gctx,
        )?);
    }
    Ok(out)
}

pub(super) fn emit_ginit_with_nested(
    ws: &Workspace,
    g: &GlobalInit,
    names: &CalleeNames,
    hoisted: &HashSet<ustr::Ustr>,
    gctx: &GlobalCtx,
) -> Result<String, CompilerError> {
    emit_body_with_nested(ws, &g.mf, &g.init_name, names, hoisted, gctx)
}
