//! Classifying vertex, varyings and fragment signatures.

use super::*;

#[derive(Default)]
pub(crate) struct StageResources {
    uniform_params: Vec<usize>,
    /// The `@uniform` group's struct name, if the stage declares one. Each
    /// stage gets its own std140 block at its own binding index -- the stages
    /// declare DIFFERENT types and cannot be made to share one.
    pub(crate) uniform_group: Option<Ustr>,
    engine_params: Vec<usize>,
    pub(crate) engine_group: Option<Ustr>,
    textures: Option<(usize, TextureSlots)>,
    bare_samplers: Vec<(usize, Ustr, SamplerKind)>,
    pub(crate) arrays: Vec<ArrayParam>,
}

enum OtherParam {
    Value(GlslTy),
    Struct,
}

impl StageResources {
    fn classify(
        &mut self,
        ws: &Workspace,
        pd: &bone_core::hir::ParamDecl,
        i: usize,
        span: Span,
        stage: &str,
    ) -> Result<Option<OtherParam>, CompilerError> {
        if let Some(a) = classify_array_param(ws, pd, i, span, stage)? {
            self.arrays.push(a);
            return Ok(None);
        }
        if has_anno(&pd.annotations, "textures") {
            self.add_textures(ws, pd, i, span, stage)?;
            return Ok(None);
        }
        match glsl_ty(ws, &SpecSubst::default(), pd.ty, span, "parameter") {
            Ok(t) if t.is_sampler() => {
                self.bare_samplers.push((i, pd.name, sampler_kind_of(t)));
                Ok(None)
            }
            Ok(other) => Ok(Some(OtherParam::Value(other))),
            Err(_) => Ok(Some(OtherParam::Struct)),
        }
    }

    fn add_textures(
        &mut self,
        ws: &Workspace,
        pd: &bone_core::hir::ParamDecl,
        index: usize,
        span: Span,
        stage: &str,
    ) -> Result<(), CompilerError> {
        reject_conflicting_texture_role(&pd.annotations, pd.name, span, stage)?;
        if self.textures.is_some() {
            return Err(err(
                span,
                if stage == "entry" {
                    "shader entry: at most one @textures group param is allowed".to_owned()
                } else {
                    format!(
                        "shader {stage}: at most one @textures group param is allowed -- one group is the binding set"
                    )
                },
            ));
        }
        let name = nominal_name(ws, pd.ty).ok_or_else(|| {
            err(
                span,
                if stage == "entry" {
                    "shader entry: an @textures param must be a value struct".to_owned()
                } else {
                    format!("shader {stage}: an @textures param must be a struct")
                },
            )
        })?;
        self.textures = Some((
            index,
            texture_slots(ws, name).map_err(|e| err(span, e.message))?,
        ));
        Ok(())
    }

    // A group's fields are validated where they become the interface's
    // members (`uniform_group_members`), which is the std140 question.
    fn add_uniform(&mut self, ws: &Workspace, pd: &bone_core::hir::ParamDecl, i: usize) {
        self.uniform_group = nominal_name(ws, pd.ty);
        self.uniform_params.push(i);
    }

    fn add_engine(&mut self, ws: &Workspace, pd: &bone_core::hir::ParamDecl, i: usize) {
        self.engine_group = nominal_name(ws, pd.ty);
        self.engine_params.push(i);
    }

    fn check_counts(&self, span: Span, stage: &str) -> Result<(), CompilerError> {
        if self.uniform_params.len() > 1 {
            return Err(err(
                span,
                format!("shader {stage}: at most one @uniform struct param is allowed"),
            ));
        }
        if self.engine_params.len() > 1 {
            return Err(err(
                span,
                format!(
                    "shader {stage}: at most one @engine struct param is allowed -- it is one engine-owned block"
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn global_params(&self) -> impl Iterator<Item = usize> + '_ {
        self.engine_params
            .iter()
            .chain(&self.uniform_params)
            .copied()
            .chain(self.textures.iter().map(|(index, _)| *index))
    }

    pub(crate) fn sampler_params(&self) -> impl Iterator<Item = usize> + '_ {
        self.bare_samplers
            .iter()
            .map(|(index, _, _)| *index)
            .chain(self.textures.iter().map(|(index, _)| *index))
    }

    pub(crate) fn textures(&self) -> Option<&TextureSlots> {
        self.textures.as_ref().map(|(_, slots)| slots)
    }

    pub(crate) fn bare_samplers(
        &self,
        stage: IrStage,
    ) -> impl Iterator<Item = LogicalSampler> + '_ {
        self.bare_samplers
            .iter()
            .map(move |&(_, name, kind)| LogicalSampler { name, kind, stage })
    }
}

pub(crate) struct VertexShape {
    pub(crate) attr_params: Vec<usize>,
    pub(crate) inputs: Vec<LogicalVertexInput>,
    pub(crate) res: StageResources,
    pub(crate) varyings_ty: TyId,
}

pub(crate) const VARYINGS_PARAM: usize = 0;

pub(crate) struct FragShape {
    pub(crate) res: StageResources,
    pub(crate) output: FragmentOutput,
    pub(crate) frag_depth: bool,
}

fn has_anno(annos: &[bone_core::hir::Annotation], name: &str) -> bool {
    annos.iter().any(|a| a.name.name.as_str() == name)
}

fn anno_int(annos: &[bone_core::hir::Annotation], name: &str) -> Option<i64> {
    annos
        .iter()
        .find(|a| a.name.name.as_str() == name)
        .and_then(|a| a.args.first())
        .and_then(|arg| match arg.value {
            AnnoValue::Int(n) => Some(n),
            _ => None,
        })
}

fn uniform_array_elem(
    ws: &Workspace,
    ty: TyId,
    span: Span,
) -> Result<Option<WireKind>, CompilerError> {
    let Some(TypeExpr::App {
        ctor: TypeCtor::Nominal(n),
        params,
    }) = ty_expr(ws, ty)
    else {
        return Ok(None);
    };
    if n.as_str() != "UniformArray" {
        return Ok(None);
    }
    let Some(first) = params.into_iter().next() else {
        return Err(err(
            span,
            "shader: UniformArray needs an element type argument",
        ));
    };
    let ety = ty.with_idx(first);
    if let Some(k) = skull_mir::gpu_layout::wire_kind(ws, ety) {
        return Ok(Some(k));
    }
    let name = glsl_ty(
        ws,
        &SpecSubst::default(),
        ety,
        span,
        "uniform-array element",
    )?;
    Err(err(
        span,
        format!(
            "shader: UniformArray element type `{}` has no std140 form (allowed: float, int, Vec2, Vec4)",
            name.name()
        ),
    ))
}

pub(crate) struct ArrayParam {
    pub(crate) param: usize,
    pub(crate) decl: ArrayMemberDecl,
}

fn classify_array_param(
    ws: &Workspace,
    pd: &bone_core::hir::ParamDecl,
    i: usize,
    span: Span,
    stage: &str,
) -> Result<Option<ArrayParam>, CompilerError> {
    let Some(elem) = uniform_array_elem(ws, pd.ty, span)? else {
        return Ok(None);
    };
    let anno = pd.annotations.iter().find(|a| a.name.name.as_str() == "size").ok_or_else(|| {
        err(span, format!(
            "shader {stage}: uniform-array param `{}` needs @size(N) (the declared GLSL array capacity)",
            pd.name
        ))
    })?;
    let n = anno_int(&pd.annotations, "size").expect("@size arity/type is registry-validated");
    let capacity = u32::try_from(n).map_err(|_| {
        CompilerError::new_with_label(anno.span, format!(
            "shader {stage}: uniform-array param `{}` has @size({n}), which is not a usable element count",
            pd.name
        ))
    })?;
    Ok(Some(ArrayParam {
        param: i,
        decl: ArrayMemberDecl {
            name: pd.name,
            elem,
            capacity,
            span: anno.span,
        },
    }))
}

fn struct_def_of(ws: &Workspace, ty: TyId) -> Option<&StructDef> {
    let sname = nominal_name(ws, ty)?;
    let &sid = ws.struct_by_name.get(&sname)?;
    Some(&ws.modules[sid.module.index()].structs[sid.def])
}

fn struct_gl_fields(
    ws: &Workspace,
    ty: TyId,
    span: Span,
    what: &str,
) -> Result<Vec<(Ustr, GlslTy)>, CompilerError> {
    let sd = struct_def_of(ws, ty)
        .ok_or_else(|| err(span, format!("shader: {what} must be a value struct")))?;
    let mut out = Vec::new();
    for fd in &sd.fields {
        let fty = glsl_ty(
            ws,
            &SpecSubst::default(),
            fd.ty,
            span,
            &format!("{what} field `{}`", fd.name),
        )?;
        if fty.is_sampler() {
            return Err(err(
                span,
                format!(
                    "shader: {what} field `{}` cannot be a sampler -- pass samplers as fn params",
                    fd.name
                ),
            ));
        }
        if fty == GlslTy::Bvec3 {
            return Err(err(
                span,
                format!(
                    "shader: {what} field `{}` cannot be a bvec3 -- bool-vector {what}s are unsupported (bvec3 is local/temp only)",
                    fd.name
                ),
            ));
        }
        out.push((fd.name, fty));
    }
    Ok(out)
}

fn sampler_kind_of(t: GlslTy) -> SamplerKind {
    match t {
        GlslTy::ISampler2D => SamplerKind::Int,
        _ => SamplerKind::Float,
    }
}

pub(crate) fn sampler_glsl_ty(k: SamplerKind) -> GlslTy {
    match k {
        SamplerKind::Float => GlslTy::Sampler2D,
        SamplerKind::Int => GlslTy::ISampler2D,
    }
}

const PARAM_ROLES: [&str; 4] = ["attr", "instance", "uniform", "engine"];

fn reject_conflicting_texture_role(
    annos: &[bone_core::hir::Annotation],
    name: Ustr,
    span: Span,
    stage: &str,
) -> Result<(), CompilerError> {
    for other in PARAM_ROLES.iter().copied() {
        if has_anno(annos, other) {
            return Err(err(
                span,
                format!(
                    "shader {stage}: param `{name}` has conflicting role markers (@textures and @{other})"
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn attr_glsl_ty(k: AttrKind) -> GlslTy {
    match k {
        AttrKind::F32 => GlslTy::Float,
        AttrKind::I32 => GlslTy::Int,
        AttrKind::F32x2 => GlslTy::Vec2,
        AttrKind::F32x3 => GlslTy::Vec3,
        AttrKind::F32x4 => GlslTy::Vec4,
    }
}

pub(crate) fn classify_vertex(ws: &Workspace, f: &Function) -> Result<VertexShape, CompilerError> {
    let span = f.name_span;
    let (mut per_vertex, mut instanced) = (Vec::new(), Vec::new());
    let mut res = StageResources::default();
    for (i, pd) in f.param_decls.iter().enumerate() {
        match res.classify(ws, pd, i, span, "vertex")? {
            None => {}
            Some(OtherParam::Value(other)) => {
                return Err(err(
                    span,
                    format!(
                        "shader vertex: param `{}` ({}) must be an @attr/@instance/@uniform value struct or a Sampler2D",
                        pd.name,
                        other.name()
                    ),
                ));
            }
            Some(OtherParam::Struct) => match (
                has_anno(&pd.annotations, "attr"),
                has_anno(&pd.annotations, "instance"),
                has_anno(&pd.annotations, "uniform"),
                has_anno(&pd.annotations, "engine"),
            ) {
                (true, false, false, false) => per_vertex.push(i),
                (false, true, false, false) => instanced.push(i),
                (false, false, true, false) => res.add_uniform(ws, pd, i),
                (false, false, false, true) => res.add_engine(ws, pd, i),
                (false, false, false, false) => {
                    return Err(err(
                        span,
                        format!(
                            "shader vertex: value-struct param `{}` needs a role marker -- @attr (per-vertex attributes), @instance (per-instance attributes), @uniform (this shader's uniform block), or @engine (the engine-owned frame/pass block)",
                            pd.name
                        ),
                    ));
                }
                _ => {
                    return Err(err(
                        span,
                        format!(
                            "shader vertex: param `{}` has conflicting role markers",
                            pd.name
                        ),
                    ));
                }
            },
        }
    }
    res.check_counts(span, "vertex")?;
    // @attr params first, then @instance -- the ORDER locations are assigned
    // in, which is all the classification owns; the numbers are the ABI's.
    let instanced_from = per_vertex.len();
    let mut attr_params = per_vertex;
    attr_params.extend(instanced);
    let mut inputs = Vec::new();
    for (n, &i) in attr_params.iter().enumerate() {
        let gl_fields = struct_gl_fields(ws, f.param_decls[i].ty, span, "attribute")?;
        // GLSL forbids `in bool`.
        if let Some((n, _)) = gl_fields.iter().find(|&&(_, t)| t == GlslTy::Bool) {
            return Err(err(
                span,
                format!(
                    "shader vertex: attribute `{n}` cannot be bool (GLSL forbids bool vertex inputs) -- use int or float"
                ),
            ));
        }
        let sname = nominal_name(ws, f.param_decls[i].ty).ok_or_else(|| {
            err(
                span,
                "shader vertex: attribute param must be a value struct",
            )
        })?;
        let va = vertex_attrs(ws, sname).map_err(|e| err(span, e.message))?;
        inputs.push(LogicalVertexInput {
            struct_name: NominalType::of(ws, sname),
            rate: if n < instanced_from {
                VertexRate::PerVertex
            } else {
                VertexRate::PerInstance
            },
            attrs: va.attrs.iter().map(|a| (a.name, a.kind)).collect(),
        });
    }
    let varyings_ty = f.return_ty.ok_or_else(|| {
        err(
            span,
            "shader vertex: entry must return a varyings value struct",
        )
    })?;
    Ok(VertexShape {
        attr_params,
        inputs,
        res,
        varyings_ty,
    })
}

pub(crate) fn classify_varyings(
    ws: &Workspace,
    ty: TyId,
    span: Span,
) -> Result<Vec<LogicalVarying>, CompilerError> {
    let sd = struct_def_of(ws, ty)
        .ok_or_else(|| err(span, "shader: the varyings type must be a value struct"))?;
    let mut has_position = false;
    let mut varyings = Vec::new();
    for fd in &sd.fields {
        let fty = glsl_ty(
            ws,
            &SpecSubst::default(),
            fd.ty,
            span,
            &format!("varying `{}`", fd.name),
        )?;
        let kind = wire_kind_of(fty);
        if fty == GlslTy::Bvec3 {
            return Err(err(
                span,
                format!(
                    "shader: varying `{}` cannot be a bvec3 (GLSL forbids bool-vector interpolants) -- bvec3 is local/temp only",
                    fd.name
                ),
            ));
        }
        if fd.name.as_str() == "position" {
            if fty != GlslTy::Vec4 {
                return Err(err(
                    span,
                    "shader: the varyings `position` field must be Vec4 (it becomes gl_Position)",
                ));
            }
            has_position = true;
            continue;
        }
        let flat = has_anno(&fd.annotations, "flat");
        if fty == GlslTy::Int && !flat {
            return Err(err(
                span,
                format!(
                    "shader: int-typed varying `{}` needs @flat (GLSL requires flat integer interpolation)",
                    fd.name
                ),
            ));
        }
        // GLSL forbids bool in/out variables, even flat.
        if fty == GlslTy::Bool {
            return Err(err(
                span,
                format!(
                    "shader: varying `{}` cannot be bool (GLSL forbids bool varyings) -- use float or int + @flat",
                    fd.name
                ),
            ));
        }
        varyings.push(LogicalVarying {
            name: fd.name,
            kind,
            flat,
        });
    }
    if !has_position {
        return Err(err(
            span,
            "shader: the varyings struct must have a `position: Vec4` field (it becomes gl_Position)",
        ));
    }
    Ok(varyings)
}

pub(crate) fn classify_pipeline_fragment(
    ws: &Workspace,
    f: &Function,
    vert_varyings_ty: TyId,
) -> Result<FragShape, CompilerError> {
    let span = f.name_span;
    let Some(first) = f.param_decls.get(VARYINGS_PARAM) else {
        return Err(err(
            span,
            "shader fragment: the entry must take the varyings struct as its first param",
        ));
    };
    if nominal_name(ws, first.ty).is_none()
        || nominal_name(ws, first.ty) != nominal_name(ws, vert_varyings_ty)
    {
        return Err(err(
            span,
            "shader fragment: the first param must be the varyings struct (the vertex entry's return type) -- the pair's varyings types do not match",
        ));
    }
    let mut res = StageResources::default();
    for (i, pd) in f.param_decls.iter().enumerate().skip(VARYINGS_PARAM + 1) {
        match res.classify(ws, pd, i, span, "fragment")? {
            None => {}
            Some(OtherParam::Value(other)) => {
                return Err(err(
                    span,
                    format!(
                        "shader fragment: param `{}` ({}) must be @uniform or a Sampler2D",
                        pd.name,
                        other.name()
                    ),
                ));
            }
            Some(OtherParam::Struct) => match (
                has_anno(&pd.annotations, "uniform"),
                has_anno(&pd.annotations, "engine"),
            ) {
                (true, false) => res.add_uniform(ws, pd, i),
                (false, true) => res.add_engine(ws, pd, i),
                (false, false) => {
                    return Err(err(
                        span,
                        format!(
                            "shader fragment: value-struct param `{}` needs @uniform (this shader's block) or @engine (the engine-owned frame/pass block)",
                            pd.name
                        ),
                    ));
                }
                (true, true) => {
                    return Err(err(
                        span,
                        format!(
                            "shader fragment: param `{}` has conflicting role markers",
                            pd.name
                        ),
                    ));
                }
            },
        }
    }
    res.check_counts(span, "fragment")?;
    let mut frag_depth = false;
    let output = match f
        .return_ty
        .map(|rt| (rt, glsl_ty(ws, &SpecSubst::default(), rt, span, "return")))
    {
        Some((_, Ok(GlslTy::Vec4))) => FragmentOutput::Single(WireKind::F32x4),
        Some((_, Ok(GlslTy::Int))) => FragmentOutput::Single(WireKind::I32),
        Some((rt, Err(_))) => {
            let sd = struct_def_of(ws, rt).ok_or_else(|| {
                err(span, "shader fragment: the return type must be Vec4 (FragColor), int (an integer id target), or a value struct of Vec4/int fields (MRT outputs)")
            })?;
            let mut outs = Vec::new();
            for fd in &sd.fields {
                let fty = glsl_ty(
                    ws,
                    &SpecSubst::default(),
                    fd.ty,
                    span,
                    &format!("MRT output `{}`", fd.name),
                )?;
                if fd.name.as_str() == FRAG_DEPTH_FIELD {
                    if fty != GlslTy::Float {
                        return Err(err(
                            span,
                            format!(
                                "shader fragment: `{FRAG_DEPTH_FIELD}` must be float (window-space depth in [0,1], written to gl_FragDepth), found {}",
                                fty.name()
                            ),
                        ));
                    }
                    frag_depth = true;
                    continue;
                }
                if fty != GlslTy::Vec4 && fty != GlslTy::Int {
                    return Err(err(
                        span,
                        format!(
                            "shader fragment: MRT output `{}` ({}) must be Vec4 or int (each output is a `layout(location=N) out <ty>`)",
                            fd.name,
                            fty.name()
                        ),
                    ));
                }
                outs.push((fd.name, wire_kind_of(fty)));
            }
            if outs.is_empty() {
                return Err(err(
                    span,
                    "shader fragment: the MRT output struct must have at least one colour (Vec4/int) field besides `FragDepth`",
                ));
            }
            FragmentOutput::Mrt(outs)
        }
        _ => {
            return Err(err(
                span,
                "shader fragment: the return type must be Vec4 (FragColor), int (an integer id target), or a value struct of Vec4/int fields (MRT outputs)",
            ));
        }
    };
    Ok(FragShape {
        res,
        output,
        frag_depth,
    })
}

pub(crate) fn reject_position_read(
    mf: &MirFunc,
    varyings_cell: Idx<Cell>,
    span: Span,
) -> Result<(), CompilerError> {
    for (_, inst) in mf.insts.iter() {
        if let Inst::Field { base, field, .. } = inst {
            if field.as_str() == "position"
                && matches!(mf.insts[*base], Inst::Load(c) if c == varyings_cell)
            {
                return Err(err(
                    span,
                    "shader fragment: `position` is gl_Position (vertex-only) and is not emitted as a varying -- it cannot be read in the fragment stage",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn classify_entry(
    ws: &Workspace,
    function: &Function,
) -> Result<StageResources, CompilerError> {
    let span = function.name_span;
    let mut res = StageResources::default();
    let mut tex_coord = None;
    for (index, param) in function.param_decls.iter().enumerate() {
        if has_anno(&param.annotations, "textures") {
            res.add_textures(ws, param, index, span, "entry")?;
            continue;
        }
        match fullscreen_ty(ws, param.ty, span, "parameter") {
            Ok(ty) if ty.is_sampler() => {
                res.bare_samplers
                    .push((index, param.name, sampler_kind_of(ty)))
            }
            Ok(GlslTy::Vec2) => {
                if tex_coord.replace(index).is_some() {
                    return Err(err(
                        span,
                        "shader entry: exactly one Vec2 UV parameter is allowed",
                    ));
                }
            }
            Ok(other) => {
                return Err(err(
                    span,
                    format!(
                        "shader entry: param `{}` ({}) is not a fullscreen UV, sampler, or uniform struct",
                        param.name,
                        match other {
                            GlslTy::Vec3 => "Vec3",
                            GlslTy::Vec4 => "Vec4",
                            GlslTy::Bvec3 => "Bvec3",
                            GlslTy::Mat4 => "Mat4",
                            _ => other.name(),
                        }
                    ),
                ));
            }
            Err(error) => {
                let Some(group) = nominal_name(ws, param.ty) else {
                    return Err(error);
                };
                if !has_anno(&param.annotations, "uniform") {
                    return Err(err(
                        span,
                        format!(
                            "shader entry: value-struct param `{}` needs @uniform",
                            param.name
                        ),
                    ));
                }
                if res.uniform_group.is_some() {
                    return Err(err(
                        span,
                        "shader entry: at most one uniform struct param is allowed",
                    ));
                }
                res.uniform_params.push(index);
                res.uniform_group = Some(group);
            }
        }
    }
    if tex_coord.is_none() {
        return Err(err(span, "shader entry: a Vec2 UV parameter is required"));
    }
    if !matches!(
        function
            .return_ty
            .and_then(|ty| fullscreen_ty(ws, ty, span, "return").ok()),
        Some(GlslTy::Vec4)
    ) {
        return Err(err(span, "shader entry: the return type must be Vec4"));
    }
    Ok(res)
}

fn fullscreen_ty(
    ws: &Workspace,
    ty: TyId,
    span: Span,
    what: &str,
) -> Result<GlslTy, CompilerError> {
    let ty = match ty_expr(ws, ty) {
        Some(TypeExpr::Builtin(ty)) => builtin_glsl_ty(ty),
        Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        }) => nominal_glsl_ty(name.as_str()),
        Some(TypeExpr::Generic(_, name)) => {
            return Err(err(
                span,
                format!(
                    "shader artifact: {what} type `{name}` has no admitted concrete shader shape"
                ),
            ));
        }
        _ => None,
    };
    ty.ok_or_else(|| err(span,
        format!("shader artifact: {what} type is outside the admitted scalar/vector shader vocabulary")))
}

pub(crate) fn check_entry_collisions(
    interface: &ShaderInterfacePlan,
    span: Span,
) -> Result<(), CompilerError> {
    let mut seen: HashSet<Ustr> = [Ustr::from("TexCoord"), Ustr::from("FragColor")]
        .into_iter()
        .collect();
    for name in interface_global_names(interface) {
        if !seen.insert(name) {
            return Err(err(
                span,
                format!(
                    "shader entry: uniform name `{name}` would collide (fields, samplers, TexCoord and FragColor share the generated namespace)"
                ),
            ));
        }
    }
    Ok(())
}
