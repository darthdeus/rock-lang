//! The GLSL intrinsic table, reserved words and inline-rendered callables.

use super::*;

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
    // GLSL round() half-way rounding is implementation-defined.
    ("round", 1, "floor($1 + 0.5)"),
    ("min", 2, "min($1, $2)"),
    ("max", 2, "max($1, $2)"),
    ("clamp", 3, "clamp($1, $2, $3)"),
    ("lerp", 3, "mix($1, $2, $3)"),
    ("smoothstep", 3, "smoothstep($1, $2, $3)"),
    ("int_to_float", 1, "float($1)"),
    ("float", 1, "float($1)"),
    ("to_float", 1, "float($1)"),
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
    ("texture_id", 2, "texture($1, $2).r"),
    // textureSize returns ivec2; the wrap keeps the result a float Vec2.
    ("texture_size", 1, "vec2(textureSize($1, 0))"),
    ("frag_coord", 0, "gl_FragCoord"),
    ("fwidth", 1, "fwidth($1)"),
    ("dfdx", 1, "dFdx($1)"),
    ("dfdy", 1, "dFdy($1)"),
    ("discard", 0, "discard"),
    ("vertex_id", 0, "gl_VertexID"),
    ("at", 2, "$1[$2]"),
    ("isnan", 1, "isnan($1)"),
    ("isinf", 1, "isinf($1)"),
    // Inline: a helper fn named `bvec3` would collide with the builtin constructor.
    ("bvec3", 3, "bvec3($1, $2, $3)"),
    ("any", 1, "any($1)"),
    ("greater_than", 2, "greaterThan($1, $2)"),
    ("to_vec3", 1, "vec3($1)"),
];

pub(crate) fn intrinsic_template(f: &Function) -> Option<&'static str> {
    if !skull_mir::intrinsics::is_intrinsic(f)
        && !bone_core::modules::is_stdlib_module_file(f.span.file.as_str())
    {
        return None;
    }
    let n = declared_name(f.name.as_str());
    let arity = f.param_decls.len();
    GLSL_INTRINSICS
        .iter()
        .find(|(name, ar, _)| *name == n && *ar == arity)
        .map(|(_, _, tmpl)| *tmpl)
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

pub fn renders_inline_instance(
    ws: &Workspace,
    f: &Function,
    inst: &Instance,
    interner: &MonoInterner,
) -> bool {
    if intrinsic_template(f).is_some() {
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

pub(crate) fn vec_op_symbol(name: &str) -> Option<&'static str> {
    match name {
        "__op_add" => Some("+"),
        "__op_sub" => Some("-"),
        "__op_mul" => Some("*"),
        "__op_div" => Some("/"),
        _ => None,
    }
}
