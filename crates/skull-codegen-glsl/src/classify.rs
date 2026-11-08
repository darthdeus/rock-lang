//! Classifying vertex, varyings and fragment signatures.

use super::*;

/// The vertex stage: which shader stage a reachability walk is emitting (drives
/// per-stage builtin gating).
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum Stage {
    Vertex,
    Fragment,
}

/// A classified `@shader(vertex)` entry.
pub(crate) struct VertexShape {
    /// Attribute struct param indices, in location-assignment order (per-vertex
    /// `@attr` first, then `@instance` -- the numbering continues).
    pub(crate) attr_params: Vec<usize>,
    /// The attribute structs in location-assignment order, in the NEUTRAL
    /// vocabulary. THE classification -- a GLSL-typed copy with baked locations
    /// stood beside it and was free to disagree with the ABI plan about both.
    /// Locations come from `ShaderAbiPlan::attribute_locations`; the spelling
    /// from `attr_glsl_ty` at the point of emission.
    pub(crate) inputs: Vec<LogicalVertexInput>,
    /// `@uniform` struct param indices (at most one).
    pub(crate) uniform_params: Vec<usize>,
    /// The `@uniform` group's struct name, if the stage declares one. Each
    /// stage gets its own std140 block at its own binding index -- the stages
    /// declare DIFFERENT types and cannot be made to share one.
    pub(crate) uniform_group: Option<Ustr>,
    /// `@engine` struct param indices (at most one).
    pub(crate) engine_params: Vec<usize>,
    /// The `@engine` group's struct name, if the stage declares one. A separate
    /// ROLE from `@uniform`, not a second one of them: the engine produces it,
    /// its lifetime is the pass rather than the draw, and it binds at index 0 so
    /// both stages of a program name the same block.
    pub(crate) engine_group: Option<Ustr>,
    /// `@textures` group param indices -- see the field of the same name on
    /// `FragShape`. Vertex texture fetch is legal in GL 3.3.
    pub(crate) texture_params: Vec<usize>,
    /// The `@textures` group's struct name, if the stage declares one.
    pub(crate) texture_group: Option<Ustr>,
    /// Sampler params (rare in vertex, but legal).
    pub(crate) samplers: Vec<(usize, Ustr, SamplerKind)>,
    /// `UniformArray<T>` params, declaration order -- the array block's members.
    pub(crate) arrays: Vec<ArrayParam>,
    /// The varyings value-struct return type.
    pub(crate) varyings_ty: TyId,
}

/// A classified varyings value struct.
pub(crate) struct VaryingsShape {
    /// Varyings EXCLUDING `position`, in the NEUTRAL vocabulary. This IS the
    /// interface's list -- not a GLSL-typed copy of it -- so there is nothing
    /// for the two to disagree about.
    pub(crate) varyings: Vec<LogicalVarying>,
}

/// A classified pipeline `@shader(fragment)` entry (varyings-consuming form).
pub(crate) struct FragShape {
    /// The varyings-struct param index (always 0 -- the first param).
    pub(crate) varyings_param: usize,
    pub(crate) uniform_params: Vec<usize>,
    /// The `@uniform` group's struct name, if the stage declares one. Its own
    /// std140 block, at its own binding index -- see `VertexShape`.
    pub(crate) uniform_group: Option<Ustr>,
    /// `@engine` struct param indices (at most one) -- see `VertexShape`.
    pub(crate) engine_params: Vec<usize>,
    /// The `@engine` group's struct name, if the stage declares one.
    pub(crate) engine_group: Option<Ustr>,
    /// `@textures` group param indices. Their fields are already expanded into
    /// `samplers`; the params themselves render as bare globals like a uniform
    /// struct, so they belong in `global_params`.
    pub(crate) texture_params: Vec<usize>,
    /// The `@textures` group's struct name, if the stage declares one.
    pub(crate) texture_group: Option<Ustr>,
    pub(crate) samplers: Vec<(usize, Ustr, SamplerKind)>,
    /// `UniformArray<T>` params, declaration order -- the array block's members.
    pub(crate) arrays: Vec<ArrayParam>,
    /// How the stage's colour output is shaped, in the NEUTRAL vocabulary:
    /// `Mrt` when the entry returns a value struct (one
    /// `layout(location=N) out <ty> <field>` per field, declaration order),
    /// `Single` for the implicit `FragColor` (`I32` for an R32I-only target).
    /// This IS the interface's answer, not a GLSL-typed copy of it.
    pub(crate) output: FragmentOutput,
    /// The output struct carries a `FragDepth: float` field (gl_FragDepth),
    /// which is excluded from `output`.
    pub(crate) frag_depth: bool,
}

fn has_anno(annos: &[bone_core::hir::Annotation], name: &str) -> bool {
    annos.iter().any(|a| a.name.name.as_str() == name)
}

/// The int argument of an `@name(N)` annotation, if present (arity/type are
/// registry-validated upstream).
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

/// The element WIRE KIND of a `UniformArray<T>`-typed param, or `None` when the
/// type is not the uniform-array phantom.
///
/// A wire kind and not a [`GlslTy`], because the array block's DECLARATION is
/// rendered from the layout plan: classifying the element here as a GLSL type
/// too would make the emitter a second authority on what an array holds, free to
/// disagree with the one that computed the offsets. Which kinds are ADMITTED is
/// likewise `uniform_array_block`'s answer, not this function's -- it rejects
/// `Vec3`/`Mat4`/`bool`/`uint` with a span, so re-stating the set here would be the same
/// drift one level down.
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
    // The element has no std140 form. Naming it needs its GLSL spelling, and
    // when THAT fails too the right diagnostic is the one `glsl_ty` produced --
    // it knows why the type is unusable, and this function does not. Replacing
    // it with a fabricated `?` would discard the compiler's own information at
    // exactly the point the author needs it.
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

/// One classified `UniformArray<T>` param: where it sits in the entry's
/// signature (so it can be dropped from the emitted one) and its DECLARATION for
/// the layout planner.
pub(crate) struct ArrayParam {
    pub(crate) param: usize,
    pub(crate) decl: ArrayMemberDecl,
}

/// Classify param `i` as an array member, or `None` when it is not one.
///
/// The `@size` ANNOTATION's span travels on the decl and is what every layout
/// rejection is reported at. `ParamDecl` carries no span of its own (that is
/// bone's, and off limits), and the annotation is both present on every array
/// param by construction and the exact thing an author edits to fix a capacity
/// or budget rejection.
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
    // `N` is source-controlled, so it reaches the layout as a `u32` only through
    // a rejection: a negative or absurd literal must not wrap into a small
    // capacity that then passes the very budget check it should fail.
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

/// The `layout(std140) uniform <Blk> { T name[N]; .. };` declaration for an
/// array block.
///
/// Every line comes from the PLAN -- element kind through [`wire_glsl_ty`], the
/// same function the scalar declaration uses, and capacity from the member the
/// offsets were computed for. The emitter deliberately holds no `GlslTy` for an
/// array element: a second classification of the same param is exactly how a
/// declaration and a layout drift into disagreeing, and "one layout authority"
/// would stop being true the moment it existed.
pub(crate) fn uniform_array_block_decl(plan: &UniformArrayBlockPlan) -> String {
    let mut s = format!("layout(std140) uniform {} {{\n", plan.block_name);
    for m in &plan.members {
        s.push_str(&format!(
            "    {} {}[{}];\n",
            wire_glsl_ty(m.elem),
            m.name,
            m.capacity
        ));
    }
    s.push_str("};\n");
    s
}

/// The [`StructDef`] behind a nominal type, if any.
fn struct_def_of(ws: &Workspace, ty: TyId) -> Option<&StructDef> {
    let sname = nominal_name(ws, ty)?;
    let &sid = ws.struct_by_name.get(&sname)?;
    Some(&ws.modules[sid.module.index()].structs[sid.def])
}

/// A block member's GLSL type spelling. The one function a WGSL or Metal
/// emitter rewrites for the uniform lane, the twin of [`attr_glsl_ty`].
fn wire_glsl_ty(k: WireKind) -> &'static str {
    match k {
        WireKind::F32 => "float",
        WireKind::F32x2 => "vec2",
        WireKind::F32x3 => "vec3",
        WireKind::F32x4 => "vec4",
        WireKind::I32 => "int",
        WireKind::U32 => "uint",
        WireKind::Bool32 => "bool",
        WireKind::Mat4 => "mat4",
    }
}

/// The `layout(std140) uniform <Blk> { .. };` declaration for a group.
///
/// Declared with NO instance name, which is what keeps this change confined to
/// declaration lines: an unnamed-instance block's members are bare GLSL globals,
/// so every body reading them is byte-identical to the loose form.
///
/// Members come from the PLAN, not from a second walk of the struct -- the same
/// single-oracle rule `VertexShape.attrs` follows for attributes. The driver
/// resolves members by name, so the names here and the offsets the packer writes
/// must come from one place or they can drift silently.
pub(crate) fn uniform_block_decl(block: &GlslUniformBlock) -> String {
    let mut s = format!("layout(std140) uniform {} {{\n", block.name);
    for m in &block.plan.members {
        s.push_str(&format!("    {} {};\n", wire_glsl_ty(m.kind), m.name));
    }
    s.push_str("};\n");
    s
}

/// Attribute/uniform fields of a struct param, GLSL-typed (a sampler-typed field
/// rejects -- samplers are params, not struct fields).
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

/// Samplers that are NOT part of a `@textures` group -- bare params, still
/// bound by name and a hand-assigned unit on the CPU side.
pub(crate) fn has_bare_sampler(
    samplers: &[(usize, Ustr, SamplerKind)],
    texture_params: &[usize],
) -> bool {
    samplers.iter().any(|(i, _, _)| !texture_params.contains(i))
}

/// How this backend spells a `SamplerKind`.
/// The neutral kind of a tier-0 sampler type. The inverse of
/// [`sampler_glsl_ty`], and the point where a declared sampler param leaves
/// GLSL's vocabulary for the interface's.
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

/// A `@textures` group param expands to one sampler declaration per field, at
/// the unit the artifact assigned from field order. Every entry carries the
/// GROUP's param index: the param itself is dropped from the GLSL signature and
/// its field reads render as bare globals, exactly like a `@uniform` struct.
fn texture_group_samplers(
    ws: &Workspace,
    ty: TyId,
    param_index: usize,
    span: Span,
    stage: &str,
) -> Result<Vec<(usize, Ustr, SamplerKind)>, CompilerError> {
    let sname = nominal_name(ws, ty).ok_or_else(|| {
        err(
            span,
            format!("shader {stage}: an @textures param must be a struct"),
        )
    })?;
    let ts = texture_slots(ws, sname).map_err(|e| err(span, e.message))?;
    Ok(ts
        .slots
        .iter()
        .map(|s| (param_index, s.name, s.kind))
        .collect())
}

/// Every param role, so a check that means "all the others" cannot go stale by
/// listing them. `reject_conflicting_texture_role` enumerated the three that
/// existed when it was written, and adding `@engine` silently widened what it
/// let through.
const PARAM_ROLES: [&str; 4] = ["attr", "instance", "uniform", "engine"];

/// `@textures` is exclusive with the other param roles -- a group is a binding
/// set of its own, not attribute or uniform-block data.
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

/// How this backend spells an `AttrKind`. The artifact stays target-neutral
/// (kind + arity); GLSL names live here, and a WGSL or Metal emitter writes its
/// own version of this one function.
pub(crate) fn attr_glsl_ty(k: AttrKind) -> GlslTy {
    match k {
        AttrKind::F32 => GlslTy::Float,
        AttrKind::I32 => GlslTy::Int,
        AttrKind::F32x2 => GlslTy::Vec2,
        AttrKind::F32x3 => GlslTy::Vec3,
        AttrKind::F32x4 => GlslTy::Vec4,
    }
}

/// Classify a `@shader(vertex)` entry: value-struct params carry a role marker
/// (`@attr` / `@instance` / `@uniform`); samplers are type-distinguished; the
/// return type is the varyings struct.
pub(crate) fn classify_vertex(ws: &Workspace, f: &Function) -> Result<VertexShape, CompilerError> {
    let span = f.name_span;
    let (mut per_vertex, mut instanced, mut uniform_params, mut samplers, mut arrays) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut texture_params = Vec::new();
    let mut texture_group: Option<Ustr> = None;
    let mut uniform_group: Option<Ustr> = None;
    let mut engine_group: Option<Ustr> = None;
    let mut engine_params: Vec<usize> = Vec::new();
    for (i, pd) in f.param_decls.iter().enumerate() {
        if let Some(a) = classify_array_param(ws, pd, i, span, "vertex")? {
            arrays.push(a);
            continue;
        }
        if has_anno(&pd.annotations, "textures") {
            reject_conflicting_texture_role(&pd.annotations, pd.name, span, "vertex")?;
            if texture_group.is_some() {
                return Err(err(
                    span,
                    "shader vertex: at most one @textures group param is allowed -- one group is the binding set",
                ));
            }
            texture_group = nominal_name(ws, pd.ty);
            samplers.extend(texture_group_samplers(ws, pd.ty, i, span, "vertex")?);
            texture_params.push(i);
            continue;
        }
        match glsl_ty(ws, &SpecSubst::default(), pd.ty, span, "parameter") {
            Ok(t) if t.is_sampler() => samplers.push((i, pd.name, sampler_kind_of(t))),
            Ok(other) => {
                return Err(err(
                    span,
                    format!(
                        "shader vertex: param `{}` ({}) must be an @attr/@instance/@uniform value struct or a Sampler2D",
                        pd.name,
                        other.name()
                    ),
                ));
            }
            Err(_) => match (
                has_anno(&pd.annotations, "attr"),
                has_anno(&pd.annotations, "instance"),
                has_anno(&pd.annotations, "uniform"),
                has_anno(&pd.annotations, "engine"),
            ) {
                (true, false, false, false) => per_vertex.push(i),
                (false, true, false, false) => instanced.push(i),
                (false, false, true, false) => {
                    uniform_group = nominal_name(ws, pd.ty);
                    uniform_params.push(i);
                }
                (false, false, false, true) => {
                    engine_group = nominal_name(ws, pd.ty);
                    engine_params.push(i);
                }
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
    if uniform_params.len() > 1 {
        return Err(err(
            span,
            "shader vertex: at most one @uniform struct param is allowed",
        ));
    }
    if engine_params.len() > 1 {
        return Err(err(
            span,
            "shader vertex: at most one @engine struct param is allowed -- it is one engine-owned block",
        ));
    }
    let mut attr_params = Vec::new();
    let mut inputs: Vec<LogicalVertexInput> = Vec::new();
    // @attr params first, then @instance -- the ORDER locations are assigned
    // in, which is all the classification owns; the numbers are the ABI's.
    let instanced_from = per_vertex.len();
    for (n, &i) in per_vertex.iter().chain(instanced.iter()).enumerate() {
        attr_params.push(i);
        // GLSL forbids `in bool` -- bool is legal only as a uniform. A GLSL
        // legality gate, not a source of attribute data: the artifact would
        // reject bool too, but only as "not a vertex-attribute type".
        let gl_fields = struct_gl_fields(ws, f.param_decls[i].ty, span, "attribute")?;
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
        uniform_group,
        engine_params,
        engine_group,
        attr_params,
        inputs,
        uniform_params,
        texture_params,
        texture_group,
        samplers,
        arrays,
        varyings_ty,
    })
}

/// Classify the varyings struct: a required `position: Vec4` field (-> gl_Position,
/// suppressed as a varying), plus the other fields as varyings. An int-typed
/// varying without `@flat` rejects (GLSL mandates flat integer interpolation).
pub(crate) fn classify_varyings(
    ws: &Workspace,
    ty: TyId,
    span: Span,
) -> Result<VaryingsShape, CompilerError> {
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
        // GLSL forbids bool in/out variables entirely (even flat).
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
    Ok(VaryingsShape { varyings })
}

/// Classify a pipeline `@shader(fragment)` entry: the first param is the varyings
/// struct (its type must equal the vertex entry's return type); the rest are
/// `@uniform` structs and samplers.
pub(crate) fn classify_pipeline_fragment(
    ws: &Workspace,
    f: &Function,
    vert_varyings_ty: TyId,
) -> Result<FragShape, CompilerError> {
    let span = f.name_span;
    let Some(first) = f.param_decls.first() else {
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
    let (mut uniform_params, mut samplers, mut arrays) = (Vec::new(), Vec::new(), Vec::new());
    let mut texture_params = Vec::new();
    let mut texture_group: Option<Ustr> = None;
    let mut uniform_group: Option<Ustr> = None;
    let mut engine_group: Option<Ustr> = None;
    let mut engine_params: Vec<usize> = Vec::new();
    for (i, pd) in f.param_decls.iter().enumerate().skip(1) {
        if let Some(a) = classify_array_param(ws, pd, i, span, "fragment")? {
            arrays.push(a);
            continue;
        }
        if has_anno(&pd.annotations, "textures") {
            reject_conflicting_texture_role(&pd.annotations, pd.name, span, "fragment")?;
            if texture_group.is_some() {
                return Err(err(
                    span,
                    "shader fragment: at most one @textures group param is allowed -- one group is the binding set",
                ));
            }
            texture_group = nominal_name(ws, pd.ty);
            samplers.extend(texture_group_samplers(ws, pd.ty, i, span, "fragment")?);
            texture_params.push(i);
            continue;
        }
        match glsl_ty(ws, &SpecSubst::default(), pd.ty, span, "parameter") {
            Ok(t) if t.is_sampler() => samplers.push((i, pd.name, sampler_kind_of(t))),
            Ok(other) => {
                return Err(err(
                    span,
                    format!(
                        "shader fragment: param `{}` ({}) must be @uniform or a Sampler2D",
                        pd.name,
                        other.name()
                    ),
                ));
            }
            Err(_) => {
                if has_anno(&pd.annotations, "engine") {
                    if has_anno(&pd.annotations, "uniform") {
                        return Err(err(
                            span,
                            format!(
                                "shader fragment: param `{}` has conflicting role markers",
                                pd.name
                            ),
                        ));
                    }
                    // The group's fields are validated where they become the
                    // interface's members (`uniform_group_members`), which is
                    // the std140 question. A `glsl_ty` pass here was a weaker
                    // restatement -- it admits Sampler2D and Bvec3, which no
                    // uniform block can pack.
                    engine_group = nominal_name(ws, pd.ty);
                    engine_params.push(i);
                } else {
                    if !has_anno(&pd.annotations, "uniform") {
                        return Err(err(
                            span,
                            format!(
                                "shader fragment: value-struct param `{}` needs @uniform (this shader's block) or @engine (the engine-owned frame/pass block)",
                                pd.name
                            ),
                        ));
                    }
                    uniform_group = nominal_name(ws, pd.ty);
                    uniform_params.push(i);
                }
            }
        }
    }
    if uniform_params.len() > 1 {
        return Err(err(
            span,
            "shader fragment: at most one @uniform struct param is allowed",
        ));
    }
    if engine_params.len() > 1 {
        return Err(err(
            span,
            "shader fragment: at most one @engine struct param is allowed -- it is one engine-owned block",
        ));
    }
    // Return: a single Vec4 (implicit FragColor), a single int (an R32I-only
    // target, e.g. the silhouette mask), OR a value struct of Vec4/int fields
    // (MRT -- one `layout(location=N) out <ty> <field>` per field, in order;
    // int fields feed R32I attachments).
    // A `FragDepth: float` field is gl_FragDepth, not an MRT output: it takes
    // no location, so the colour fields keep theirs.
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
        uniform_group,
        engine_params,
        engine_group,
        varyings_param: 0,
        uniform_params,
        texture_params,
        texture_group,
        samplers,
        arrays,
        output,
        frag_depth,
    })
}

/// Reject a fragment-stage read of the varyings `position` field: it is
/// gl_Position (vertex-only), not a fragment input.
pub(crate) fn reject_position_read(
    ws: &Workspace,
    mf: &MirFunc,
    varyings_cell: Idx<Cell>,
    span: Span,
) -> Result<(), CompilerError> {
    let _ = ws;
    for (_, inst) in mf.insts.iter() {
        if let Inst::Field { base, field } = inst {
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
