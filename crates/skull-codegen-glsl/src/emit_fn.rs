//! Emitting one function definition, and resolving its callees.

use super::*;

impl FnEmit<'_> {
    /// The instantiation the call at `iid` reaches its callee at.
    ///
    /// A LOOKUP, not a second derivation: [`callee_inst_at`] resolved and
    /// validated every call site before this body could reach emission, and
    /// `emit_fn_def_with` recorded the answers. A miss is compiler state --
    /// the map is built from the same `mf.insts` walk this renders from.
    pub(crate) fn callee_inst(&self, iid: Idx<Inst>) -> &ShaderInstance {
        self.callee_insts
            .get(&iid)
            .expect("every Call in this body was resolved by `callee_inst_at` before emission")
    }

    pub(crate) fn concrete_call_signature(
        &self,
        iid: Idx<Inst>,
    ) -> Result<&ConcreteSignature, CompilerError> {
        let plan = self.concrete_plan.ok_or_else(|| {
            err(
                self.span(),
                "shader profile: concrete call queried without a body plan",
            )
        })?;
        let site = self.f.call_sites.get(&iid).ok_or_else(|| {
            err(
                self.span(),
                "shader profile: concrete call has no site identity",
            )
        })?;
        plan.callees
            .get(&site.site)
            .map(|callee| std::sync::Arc::as_ref(&callee.signature))
            .ok_or_else(|| {
                err(
                    self.span(),
                    "shader profile: concrete call has no signature in the body plan",
                )
            })
    }

    pub(crate) fn call_param_types(
        &self,
        iid: Idx<Inst>,
        cf: &Function,
    ) -> Result<Vec<Option<GlslTy>>, CompilerError> {
        if self.concrete_plan.is_some() {
            let sig = self.concrete_call_signature(iid)?;
            return sig
                .params
                .iter()
                .map(|&id| Ok(mono_glsl_ty(&self.interner.get(id))))
                .collect();
        }
        Ok(cf
            .param_decls
            .iter()
            .map(|p| {
                glsl_ty(
                    self.ws,
                    &self.callee_inst(iid).subst(self.ws, self.interner),
                    p.ty,
                    cf.name_span,
                    "arg",
                )
                .ok()
            })
            .collect())
    }
}

/// **The one resolver of a call site's callee instance, and it FAILS CLOSED.**
///
/// `MirFunc::spec_bindings` holds the instantiation lowering recorded at each
/// call. Reading an absent entry as "no instantiation" is only correct for a
/// callee that HAS no generics; for a generic one it silently substitutes the
/// declaration's shape, which on GLSL means emitting or typing a body no
/// instance has. So an absent or PARTIAL key for an emitted generic callee is
/// a rejection with the caller's span, never a fallback.
///
/// A callee that renders inline is exempt and is the reason the rule is not
/// simply "generic implies key": `min<T>`, `max<T>`, `clamp` and `v(..)` are
/// generic declarations that GLSL spells as native builtins at the call site,
/// so they are never emitted as definitions and their key is never consulted.
pub(crate) fn callee_inst_at(
    ws: &Workspace,
    mf: &MirFunc,
    iid: Idx<Inst>,
    func: FuncRef,
    span: Span,
    concrete: bool,
) -> Result<ShaderInstance, CompilerError> {
    let cf = &ws.module(func.module).functions[func.func];
    // Absent is not "the declaration": a callee with no generics HAS no
    // instantiation to record, and one that is generic and recorded nothing is
    // the rejection below.
    let recorded = if concrete {
        mf.callee_instances.get(&iid).cloned()
    } else {
        mf.spec_bindings.get(&iid).cloned()
    };
    if renders_inline(cf) || cf.generics.is_empty() {
        return Ok(ShaderInstance::new(
            recorded.unwrap_or_else(|| Instance::base(func)),
        ));
    }
    let key = recorded
        .as_ref()
        .map(|i| i.args.clone())
        .unwrap_or_default();
    // Covers both shapes: a call that recorded nothing, and one whose key the
    // producer declined to build because a type parameter did not reify. Those
    // used to be two arms, the second scanning for a `None` slot; a key is
    // total now, so that state is unrepresentable rather than merely unreached,
    // and its message is folded in here.
    if key.is_empty() {
        return Err(err(
            span,
            format!(
                "shader profile: the call to generic `{}` has no recorded instantiation -- either \
                 none was recorded, or a type parameter was left uninstantiated -- so GLSL has no \
                 concrete shape to emit it at. A GPU has no boxed representation for an unreified \
                 generic, so this cannot fall back to the declaration",
                cf.name
            ),
        ));
    }
    // The key is POSITIONAL against the declared generics, so a different
    // length names another function's instantiation. `spec_subst_from_key` zips
    // the two and would silently drop the tail or substitute a prefix.
    if key.len() != cf.generics.len() {
        return Err(err(
            span,
            format!(
                "shader profile: the call to generic `{}` recorded {} type argument(s) for {} \
                 declared generic(s) -- the instantiation is positional, so a key of another \
                 length is not this callee's",
                cf.name,
                key.len(),
                cf.generics.len()
            ),
        ));
    }
    Ok(ShaderInstance::new(
        recorded.expect("a non-empty key implies a recorded instance"),
    ))
}

/// A `void main()` entry that destructures its terminal `return <struct
/// literal>` into named global assignments: vertex varyings (+ gl_Position) or
/// MRT fragment outputs (`layout(location=N) out vec4`).
#[derive(Clone)]
pub(crate) struct MainOut {
    /// Field name -> GLSL type (the assignment target's type).
    pub(crate) fields: HashMap<Ustr, GlslTy>,
    /// The one field that is a GL builtin rather than a named output:
    /// vertex `position` -> `gl_Position`, fragment `FragDepth` ->
    /// `gl_FragDepth`. `(field, builtin, type)`.
    pub(crate) builtin: Option<(&'static str, &'static str, GlslTy)>,
}

/// The fragment output field that is written to `gl_FragDepth`.
pub(crate) const FRAG_DEPTH_FIELD: &str = "FragDepth";

/// Emit `<ret> <name>(<params>) { <body> }` for one lowered INSTANCE.
#[cfg(test)]
pub(crate) fn emit_fn_def(
    ws: &Workspace,
    interner: &MonoInterner,
    inst: &ShaderInstance,
    mf: &MirFunc,
) -> Result<String, CompilerError> {
    emit_fn_def_with(ws, interner, inst, mf, None, None)
}

/// [`emit_fn_def`], optionally in ENTRY mode: the uniform-struct and sampler
/// params are dropped from the GLSL signature (their reads render as bare
/// uniform globals) and the fn keeps its bare Rock name.
pub(crate) fn emit_fn_def_with(
    ws: &Workspace,
    interner: &MonoInterner,
    inst: &ShaderInstance,
    mf: &MirFunc,
    entry: Option<&EntryShape>,
    concrete_plan: Option<&ConcreteBodyPlan>,
) -> Result<String, CompilerError> {
    let fref = inst.fref();
    let subst = concrete_plan.is_none().then(|| inst.subst(ws, interner));
    let f = &ws.module(fref.module).functions[fref.func];
    let emits_main = entry.is_some_and(|s| s.main_out.is_some());
    let name = if emits_main {
        "main".to_string()
    } else if entry.is_some() {
        symbol_of_key(f.name).to_string()
    } else {
        glsl_name(ws, interner, inst)
    };
    // A `void main()` entry destructures its returned struct into globals
    // (gl_Position + varyings, or MRT outs) -- no signature return type.
    let ret_ty = if emits_main {
        None
    } else {
        if let Some(plan) = concrete_plan {
            plan.signature
                .result
                .map(|id| concrete_glsl_ty(interner, id, f.name_span, "return"))
                .transpose()?
        } else {
            match f.return_ty {
                Some(rt) => Some(glsl_ty(
                    ws,
                    subst
                        .as_ref()
                        .expect("source GLSL emission has a substitution"),
                    rt,
                    f.name_span,
                    "return",
                )?),
                None => None,
            }
        }
    };
    let ret = ret_ty.map_or("void", |t| t.name());

    // Resolve and VALIDATE every call site before anything renders, so the
    // emitter reads one answer rather than re-deriving it per use.
    let mut callee_insts: HashMap<Idx<Inst>, ShaderInstance> = HashMap::default();
    for (iid, inst) in mf.insts.iter() {
        if let Inst::Call { func, .. } = inst {
            callee_insts.insert(
                iid,
                callee_inst_at(ws, mf, iid, *func, f.name_span, concrete_plan.is_some())?,
            );
        }
    }

    // GLSL emits every loop as `while (true) { <test>; if (!c) break; .. }`, so
    // a test-less `loop {}` stays unregistered and rejects in the walker.
    let st = structure::analyze(mf, LoopShapes::TestedOnly);
    let (counted, general) = structure::split_loops(mf, &st, CountedBound::Any);

    let mut e = FnEmit {
        ws,
        interner,
        f: mf,
        ret_ty,
        cell_name: HashMap::default(),
        cell_ty: HashMap::default(),
        counted,
        general,
        structure: st,
        active: HashSet::default(),
        out: String::new(),
        indent: 1,
        visited: HashSet::default(),
        global_cells: entry
            .map(|s| s.global_params.iter().map(|&i| mf.params[i]).collect())
            .unwrap_or_default(),
        uniform_fields: entry
            .map(|s| s.uniform_fields.iter().copied().collect())
            .unwrap_or_default(),
        global_ref_cells: entry
            .map(|s| {
                s.sampler_params
                    .iter()
                    .chain(s.arrays.iter())
                    .map(|&i| mf.params[i])
                    .collect()
            })
            .unwrap_or_default(),
        reserved_names: entry
            .map(|s| s.reserved.iter().map(|u| u.to_string()).collect())
            .unwrap_or_default(),
        ignored_type_cells: entry
            .map(|s| {
                s.global_params
                    .iter()
                    .copied()
                    .chain(s.arrays.iter().copied())
                    .map(|i| mf.params[i])
                    .collect()
            })
            .unwrap_or_default(),
        folded_cond: HashMap::default(),
        folded_block: HashMap::default(),
        folded_bound: HashMap::default(),
        main_out: entry.and_then(|s| s.main_out.clone()),
        fn_span: f.name_span,
        concrete_plan,
        subst: subst.clone(),
        callee_insts,
        value_name: HashMap::default(),
    };
    // Global-base struct params + samplers + uniform arrays are dropped from the
    // GLSL signature.
    let dropped: HashSet<usize> = entry
        .map(|s| {
            s.global_params
                .iter()
                .copied()
                .chain(s.sampler_params.iter().copied())
                .chain(s.arrays.iter().copied())
                .collect()
        })
        .unwrap_or_default();
    // A global-base struct param is not a tier-0 GLSL leaf (glsl_ty would reject
    // it); its reads render as bare globals. Samplers DO map (`sampler2D`), so
    // they still get a cell type. Uniform-array params have no tier-0 type
    // either -- their only legal use is as `at()`'s first arg, which renders the
    // cell name (= the declared uniform array).
    let struct_params: HashSet<usize> = entry
        .map(|s| {
            s.global_params
                .iter()
                .copied()
                .chain(s.arrays.iter().copied())
                .collect()
        })
        .unwrap_or_default();
    for (i, &c) in mf.params.iter().enumerate() {
        if struct_params.contains(&i) {
            continue;
        }
        let ty = if let Some(plan) = concrete_plan {
            let id = plan.signature.params.get(i).copied().ok_or_else(|| {
                err(
                    f.name_span,
                    format!("shader profile: concrete parameter {i} is missing from the plan"),
                )
            })?;
            concrete_glsl_ty(interner, id, f.name_span, "parameter")?
        } else {
            glsl_ty(
                ws,
                subst
                    .as_ref()
                    .expect("source GLSL emission has a substitution"),
                f.param_decls[i].ty,
                f.name_span,
                "parameter",
            )?
        };
        e.cell_ty.insert(c, ty);
    }
    e.compute_cell_types()?;
    e.compute_folds();
    e.assign_names()?;

    // Parameter list (entry mode drops uniform-struct + sampler params).
    let params: Vec<String> = mf
        .params
        .iter()
        .enumerate()
        .filter(|(i, _)| !dropped.contains(i))
        .map(|(_, &c)| {
            let ty = e.cell_ty[&c].name();
            format!("{ty} {}", e.cell_name[&c])
        })
        .collect();

    // Declare every non-param cell up front (hoisted -- avoids GLSL block-scope
    // leaks and lets a counter/accumulator span nested loops).
    let param_set: HashSet<Idx<Cell>> = mf.params.iter().copied().collect();
    let mut decls = String::new();
    for (cid, _) in mf.cells.iter() {
        if param_set.contains(&cid) {
            continue;
        }
        // A folded diamond result / bound temp has no storage -- it renders inline.
        if e.folded_cond.contains_key(&cid) || e.folded_bound.contains_key(&cid) {
            continue;
        }
        if let Some(ty) = e.cell_ty.get(&cid) {
            decls.push_str(&format!("  {} {};\n", ty.name(), e.cell_name[&cid]));
        }
    }

    e.emit_seq(mf.entry, None, LoopCtx::default())?;
    let body = std::mem::take(&mut e.out);

    Ok(format!(
        "{ret} {name}({}) {{\n{decls}{body}}}\n",
        params.join(", ")
    ))
}

/// A cell's preferred GLSL spelling: its source name with the compiler's own
/// `__rock_` prefix stripped and `__` runs collapsed (a Rock local cannot
/// shadow what it cannot spell), else a cell-indexed temporary.
pub(crate) fn cell_base_name(cell: &Cell, cid: Idx<Cell>) -> String {
    cell.name
        .map(|n| {
            let mut s = n.as_str().to_string();
            if let Some(stripped) = s.strip_prefix("__rock_") {
                s = stripped.to_string();
            }
            while s.contains("__") {
                s = s.replace("__", "_");
            }
            s
        })
        .unwrap_or_else(|| format!("t{}", cid.into_raw().into_u32()))
}
