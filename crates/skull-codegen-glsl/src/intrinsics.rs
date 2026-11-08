//! The GLSL intrinsic table, reserved words and inline-rendered callables.

use super::*;

/// The GLSL tier-0 intrinsic table: one row per
/// (name, arity) -> `$1..$n` expression template. Single source for both
/// [`GlslIntrinsics::lookup`] (call-site rendering) and
/// [`GlslIntrinsics::entries`] (the fail-closed key surface), so the two can
/// never drift. Keyed by base name + arity (a user fn that merely shares a name
/// still resolves here -- acceptable for the restricted profile; the stdlib owns
/// these names). Vector operators (`__op_*` reaching codegen are non-scalar, so
/// Vec) are handled at the call site, not here.
const GLSL_INTRINSICS: &[(&str, usize, &str)] = &[
    ("sin", 1, "sin($1)"),
    ("cos", 1, "cos($1)"),
    ("tan", 1, "tan($1)"),
    ("sqrt", 1, "sqrt($1)"),
    ("exp", 1, "exp($1)"),
    ("log2", 1, "log2($1)"),
    ("abs", 1, "abs($1)"),
    ("floor", 1, "floor($1)"),
    ("ceil", 1, "ceil($1)"),
    ("fract", 1, "fract($1)"),
    ("sign", 1, "sign($1)"),
    ("pow", 2, "pow($1, $2)"),
    ("atan2", 2, "atan($1, $2)"),
    // GLSL round() half-way rounding is implementation-defined; pin floor(x+0.5).
    ("round", 1, "floor($1 + 0.5)"),
    ("min", 2, "min($1, $2)"),
    ("max", 2, "max($1, $2)"),
    ("clamp", 3, "clamp($1, $2, $3)"),
    ("lerp", 3, "mix($1, $2, $3)"),
    ("smoothstep", 3, "smoothstep($1, $2, $3)"),
    // int<->float identity casts.
    ("int_to_float", 1, "float($1)"),
    ("float", 1, "float($1)"),
    ("to_float", 1, "float($1)"),
    // Vec method helpers (the field-math bodies never emit). Keyed by the base
    // method name, which mangling strips the receiver-type prefix from
    // (`Vec2.dot` -> `dot`); GLSL `dot`/`length`/`distance` are overloaded across
    // vecN, so one entry covers Vec2/Vec3/Vec4.
    ("dot", 2, "dot($1, $2)"),
    ("length", 1, "length($1)"),
    ("distance", 2, "distance($1, $2)"),
    // NO normalize row: Rock's Vec2.normalize is zero-safe (returns zero()),
    // GLSL's native normalize is undefined at 0 -- the Rock body emits as a
    // helper fn instead (pinned by vec2_normalize_body).
    ("v", 4, "vec4($1, $2, $3, $4)"),
    ("v", 3, "vec3($1, $2, $3)"),
    ("v", 2, "vec2($1, $2)"),
    ("v", 1, "vec2($1)"),
    ("v", 0, "vec2(0.0)"),
    ("zero", 0, "vec2(0.0)"),
    ("texture", 2, "texture($1, $2)"),
    // Integer-sampler id read (R32I tier): texture() on isampler2D returns
    // ivec4; the id is the only channel anyone reads.
    ("texture_id", 2, "texture($1, $2).r"),
    // textureSize returns ivec2; wrap in vec2(...) so the result is a float Vec2
    // (matches Rock's `-> Vec2`, and the gl_FragCoord/size divides it feeds).
    ("texture_size", 1, "vec2(textureSize($1, 0))"),
    // GPU builtins. frag_coord/discard take no
    // args -> bare builtins (discard renders as the statement `discard;`, see
    // emit_block_effects). Fragment-stage gating lives in the pipeline emitter.
    ("frag_coord", 0, "gl_FragCoord"),
    ("fwidth", 1, "fwidth($1)"),
    ("dfdx", 1, "dFdx($1)"),
    ("dfdy", 1, "dFdy($1)"),
    ("discard", 0, "discard"),
    ("vertex_id", 0, "gl_VertexID"),
    // Uniform-array indexing (decision 3): `at(arr, i)` on an UniformArray<T>
    // param renders as plain GLSL indexing of the declared uniform array.
    ("at", 2, "$1[$2]"),
    // Bool-vector tier. isnan/isinf are GLSL builtins.
    // `any` here is arity-1 (Bvec3 reduction) -- distinct from the iterator
    // `any` (arity-2, not a shader intrinsic). `greater_than`/`to_vec3` carry
    // portable Rock bodies for luajit/JS; these templates render the native
    // GLSL inline so the bodies never emit on the shader path.
    ("isnan", 1, "isnan($1)"),
    ("isinf", 1, "isinf($1)"),
    // The native bvec3 ctor: inline, since a helper fn named `bvec3` would
    // collide with the GLSL builtin type constructor.
    ("bvec3", 3, "bvec3($1, $2, $3)"),
    ("any", 1, "any($1)"),
    ("greater_than", 2, "greaterThan($1, $2)"),
    ("to_vec3", 1, "vec3($1)"),
];

/// The GLSL intrinsic template for a callee, or `None` if it must be emitted as
/// a GLSL fn definition. Backs [`GlslIntrinsics::lookup`].
pub(crate) fn intrinsic_template(f: &Function) -> Option<&'static str> {
    let n = declared_name(f.name.as_str());
    let arity = f.param_decls.len();
    GLSL_INTRINSICS
        .iter()
        .find(|(name, ar, _)| *name == n && *ar == arity)
        .map(|(_, _, tmpl)| *tmpl)
}

/// The GLSL backend's [`IntrinsicTable`] -- the registry surface over
/// [`GLSL_INTRINSICS`]. `lookup` is the tier-0 template; `entries` is the
/// fail-closed key set (`unbacked_entries` checks each against the stdlib's
/// `@intrinsic` declarations).
pub struct GlslIntrinsics;

impl IntrinsicTable for GlslIntrinsics {
    fn backend(&self) -> &'static str {
        "glsl"
    }
    fn lookup(&self, f: &Function) -> Option<Emission> {
        intrinsic_template(f).map(Emission::Template)
    }
    fn entries(&self) -> Vec<IntrinsicEntry> {
        GLSL_INTRINSICS
            .iter()
            .map(|(name, arity, _)| IntrinsicEntry {
                name,
                arity: *arity,
            })
            .collect()
    }
}

/// Whether a callee that survived MIR lowering is rendered inline at call sites
/// rather than emitted as a GLSL fn definition. Scalar operators do not reach
/// this predicate: lowering turns them into native MIR instructions.
pub fn renders_inline(f: &Function) -> bool {
    GlslIntrinsics.lookup(f).is_some()
        || (vec_op_symbol(declared_name(f.name.as_str())).is_some() && f.param_decls.len() == 2)
}

// Union of lexical and future-use keywords in GLSL ES 3.00 section 3.8 and
// GLSL 3.30 section 3.6. Use one set so body names stay portable across profiles.
// https://registry.khronos.org/OpenGL/specs/es/3.0/GLSL_ES_Specification_3.00.pdf
// https://registry.khronos.org/OpenGL/specs/gl/GLSLangSpec.3.30.pdf
pub(crate) const GLSL_KEYWORDS: &str = "
    attribute const uniform varying layout centroid flat smooth noperspective
    break continue do for while switch case default if else in out inout
    float int void bool true false invariant discard return
    mat2 mat3 mat4 mat2x2 mat2x3 mat2x4 mat3x2 mat3x3 mat3x4 mat4x2 mat4x3 mat4x4
    vec2 vec3 vec4 ivec2 ivec3 ivec4 bvec2 bvec3 bvec4 uint uvec2 uvec3 uvec4
    lowp mediump highp precision
    sampler1D sampler2D sampler3D samplerCube
    sampler1DShadow sampler2DShadow samplerCubeShadow
    sampler1DArray sampler2DArray sampler1DArrayShadow sampler2DArrayShadow
    isampler1D isampler2D isampler3D isamplerCube isampler1DArray isampler2DArray
    usampler1D usampler2D usampler3D usamplerCube usampler1DArray usampler2DArray
    sampler2DRect sampler2DRectShadow isampler2DRect usampler2DRect
    samplerBuffer isamplerBuffer usamplerBuffer
    sampler2DMS isampler2DMS usampler2DMS
    sampler2DMSArray isampler2DMSArray usampler2DMSArray struct
    coherent volatile restrict readonly writeonly resource atomic_uint
    patch sample subroutine common partition active asm
    class union enum typedef template this packed goto
    inline noinline public static extern external interface
    long short double half fixed unsigned superp input output
    hvec2 hvec3 hvec4 dvec2 dvec3 dvec4 fvec2 fvec3 fvec4 sampler3DRect filter
    image1D image2D image3D imageCube iimage1D iimage2D iimage3D iimageCube
    uimage1D uimage2D uimage3D uimageCube image1DArray image2DArray
    iimage1DArray iimage2DArray uimage1DArray uimage2DArray
    image1DShadow image2DShadow image1DArrayShadow image2DArrayShadow
    imageBuffer iimageBuffer uimageBuffer sizeof cast namespace using row_major
";

/// The GLSL-side callable spellings the emitter renders, derived from
/// [`GLSL_INTRINSICS`] so the reserved set can never drift from the table: a
/// template's `ident(` callees plus bare-identifier templates. The spelling is
/// what GLSL sees, not the Rock name -- `atan` from Rock `atan2`, `mix` from
/// `lerp`, `textureSize` nested inside `texture_size`'s wrapper -- while
/// Rock-side spellings with no emitted callee (`lerp`, `v`, `at`, `round`)
/// stay unreserved, so existing shaders using them as locals do not churn.
pub(crate) fn glsl_builtin_callables() -> Vec<&'static str> {
    let mut out = Vec::new();
    for (_, _, tmpl) in GLSL_INTRINSICS {
        let bytes = tmpl.as_bytes();
        for (i, _) in tmpl.match_indices('(') {
            let mut start = i;
            while start > 0
                && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_')
            {
                start -= 1;
            }
            if start < i {
                let callee = &tmpl[start..i];
                if !out.contains(&callee) {
                    out.push(callee);
                }
            }
        }
        if !tmpl.is_empty()
            && tmpl.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !tmpl.as_bytes()[0].is_ascii_digit()
            && !out.contains(tmpl)
        {
            out.push(tmpl);
        }
    }
    out
}

/// The graph sees HIR call obligations before scalar operators are lowered away,
/// so it needs the concrete argument types to distinguish a vector infix from a
/// scalar MIR instruction.
pub fn renders_inline_instance(
    ws: &Workspace,
    f: &Function,
    inst: &Instance,
    interner: &MonoInterner,
) -> bool {
    if GlslIntrinsics.lookup(f).is_some() {
        return true;
    }
    if vec_op_symbol(declared_name(f.name.as_str())).is_none() || f.param_decls.len() != 2 {
        return false;
    }
    let subst = inst.subst(ws, interner);
    f.param_decls.iter().any(|param| {
        matches!(
            glsl_ty(ws, &subst, param.ty, f.name_span, "operator").ok(),
            Some(GlslTy::Vec2 | GlslTy::Vec3 | GlslTy::Vec4 | GlslTy::Mat4)
        ) || matches!(
            ws.render(None, param.ty).as_str(),
            "Vec2" | "Vec3" | "Vec4" | "Vec2f" | "Vec3f" | "Vec4f" | "Mat4"
        )
    })
}

/// A HIR arithmetic call that lowering removes into an `Inst::*` operation. It
/// is a graph leaf, but it is not a GLSL helper/builtin population.
pub fn is_lowered_native_operator(
    ws: &Workspace,
    f: &Function,
    inst: &Instance,
    interner: &MonoInterner,
) -> bool {
    if !matches!(
        declared_name(f.name.as_str()),
        "__op_add" | "__op_sub" | "__op_mul" | "__op_div" | "__op_idiv" | "__op_mod"
    ) || f.param_decls.len() != 2
        || renders_inline_instance(ws, f, inst, interner)
    {
        return false;
    }
    let subst = inst.subst(ws, interner);
    f.param_decls.iter().all(|param| {
        matches!(
            glsl_ty(ws, &subst, param.ty, f.name_span, "operator").ok(),
            Some(GlslTy::Float | GlslTy::Int | GlslTy::Uint)
        ) || matches!(ws.render(None, param.ty).as_str(), "float" | "int" | "uint")
    })
}

pub struct GlslShaderPolicy;

impl skull_codegen_shader::ShaderPlannerPolicy for GlslShaderPolicy {
    fn inline_call(&self, function: &Function) -> bool {
        renders_inline(function)
    }

    fn backend_name(&self) -> &'static str {
        "GLSL"
    }

    fn validate_concrete_signature(
        &self,
        interner: &MonoInterner,
        signature: &ConcreteSignature,
        span: Span,
    ) -> Result<(), CompilerError> {
        check_tier0_signature_concrete(interner, signature, span)
    }

    fn inline_instance(
        &self,
        ws: &Workspace,
        function: &Function,
        instance: &Instance,
        interner: &MonoInterner,
    ) -> bool {
        renders_inline_instance(ws, function, instance, interner)
    }

    fn lowered_native_operator(
        &self,
        ws: &Workspace,
        function: &Function,
        instance: &Instance,
        interner: &MonoInterner,
    ) -> bool {
        is_lowered_native_operator(ws, function, instance, interner)
    }
}
/// The GLSL infix operator symbol for a non-scalar `__op_*` (Vec operator) call.
pub(crate) fn vec_op_symbol(name: &str) -> Option<&'static str> {
    match name {
        "__op_add" => Some("+"),
        "__op_sub" => Some("-"),
        "__op_mul" => Some("*"),
        "__op_div" => Some("/"),
        _ => None,
    }
}

// --- per-function emission ---------------------------------------------------
