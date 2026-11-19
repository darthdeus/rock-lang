//! Emitting one function definition, and resolving its callees.

use super::*;

impl FnEmit<'_> {
    pub(crate) fn callee_inst(&self, iid: Idx<Inst>) -> &ShaderInstance {
        self.callee_insts
            .get(&iid)
            .expect("every Call in this body was resolved by `callee_inst_at` before emission")
    }

    pub(crate) fn concrete_call_signature(
        &self,
        iid: Idx<Inst>,
    ) -> Result<&ConcreteSignature, CompilerError> {
        let site = self.f.call_sites.get(&iid).ok_or_else(|| {
            err(
                self.span(),
                "shader profile: concrete call has no site identity",
            )
        })?;
        self.concrete_plan
            .callees
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
    ) -> Result<Vec<Option<GlslTy>>, CompilerError> {
        let sig = self.concrete_call_signature(iid)?;
        sig.params
            .iter()
            .map(|&id| Ok(mono_glsl_ty(&self.interner.get(id))))
            .collect()
    }
}

pub(crate) fn callee_inst_at(mf: &MirFunc, iid: Idx<Inst>, func: FuncRef) -> ShaderInstance {
    ShaderInstance::new(
        mf.callee_instances
            .get(&iid)
            .cloned()
            .unwrap_or_else(|| Instance::base(func)),
    )
}

/// A `void main()` entry that destructures its terminal `return <struct
/// literal>` into named global assignments: vertex varyings (+ gl_Position) or
/// MRT fragment outputs (`layout(location=N) out vec4`).
#[derive(Clone)]
pub(crate) struct MainOut {
    pub(crate) fields: HashMap<Ustr, GlslTy>,
    pub(crate) builtin: Option<(&'static str, &'static str, GlslTy)>,
}

pub(crate) const FRAG_DEPTH_FIELD: &str = "FragDepth";

pub(crate) fn emit_fn_def_with(
    ws: &Workspace,
    interner: &MonoInterner,
    inst: &ShaderInstance,
    mf: &MirFunc,
    entry: Option<&EntryShape>,
    concrete_plan: &ConcreteBodyPlan,
) -> Result<String, CompilerError> {
    let fref = inst.fref();
    let f = &ws.module(fref.module).functions[fref.func];
    let emits_main = entry.is_some_and(|s| s.main_out.is_some());
    let name = if emits_main {
        "main".to_string()
    } else if entry.is_some() {
        symbol_of_key(f.name).to_string()
    } else {
        inst.symbol(ws, interner)
    };
    let ret_ty = if emits_main {
        None
    } else {
        concrete_plan
            .signature
            .result
            .map(|id| concrete_glsl_ty(interner, id, f.name_span, "return"))
            .transpose()?
    };
    let ret = ret_ty.map_or("void", |t| t.name());

    let mut callee_insts: HashMap<Idx<Inst>, ShaderInstance> = HashMap::default();
    for (iid, inst) in mf.insts.iter() {
        if let Inst::Call { func, .. } = inst {
            callee_insts.insert(iid, callee_inst_at(mf, iid, *func));
        }
    }

    let st = structure::analyze(mf);
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
        callee_insts,
        value_name: HashMap::default(),
    };
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
    for (i, &c) in mf.params.iter().enumerate() {
        if e.ignored_type_cells.contains(&c) {
            continue;
        }
        let id = concrete_plan
            .signature
            .params
            .get(i)
            .copied()
            .ok_or_else(|| {
                err(
                    f.name_span,
                    format!("shader profile: concrete parameter {i} is missing from the plan"),
                )
            })?;
        let ty = concrete_glsl_ty(interner, id, f.name_span, "parameter")?;
        e.cell_ty.insert(c, ty);
    }
    let rpo = reverse_postorder(mf, mf.entry);
    e.compute_cell_types(&rpo)?;
    e.compute_folds(&rpo);
    e.assign_names(&rpo)?;

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

    let param_set: HashSet<Idx<Cell>> = mf.params.iter().copied().collect();
    let mut decls = String::new();
    for (cid, _) in mf.cells.iter() {
        if param_set.contains(&cid) {
            continue;
        }
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
