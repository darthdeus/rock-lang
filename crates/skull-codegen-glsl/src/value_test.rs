//! Value-test shaders and shader-root queries.

use super::*;

/// The scalar return kind of a value-test fn -- selects the readback texture
/// format on the GL side (int/bool -> R32I, float -> R32F) and the fragment
/// output declaration.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ValueTestRet {
    Int,
    Float,
    Bool,
}

impl ValueTestRet {
    /// The GL runner's tag for this kind (matches the JSON job protocol).
    pub fn tag(self) -> &'static str {
        match self {
            ValueTestRet::Int => "int",
            ValueTestRet::Float => "float",
            ValueTestRet::Bool => "bool",
        }
    }
}

/// A complete GLSL shader pair driving one `@test(returns = ...)` value test. The fragment shader wraps the
/// emitted root fn and writes its scalar result to `layout(location = 0)`; the
/// vertex shader draws a fullscreen triangle (no vertex attributes). Both carry
/// the profile's `#version` line so the GL runner compiles them as one program.
#[derive(Debug)]
pub struct ValueTestShaders {
    pub vertex: String,
    pub fragment: String,
    pub ret: ValueTestRet,
}

/// The desktop GLSL version line.
pub(crate) fn profile_header(profile: GlslProfile) -> &'static str {
    match profile {
        GlslProfile::Core330 => "#version 330 core",
    }
}

/// Emit the vertex+fragment GLSL pair for a scalar-returning `@test(returns=)`
/// fn under `profile`. The root must take no parameters (a `@test` fn) and
/// return `int`/`float`/`bool`; anything else -- a non-tier-0 return, a param,
/// or a body outside the tier-0 profile -- rejects with a spanned
/// `CompilerError` (the G1 two-tier SKIP reason). No fallback: an unmappable
/// intrinsic or non-scalar shape is a rejection, never a guess.
pub fn emit_value_test(
    ws: &Workspace,
    mid: ModuleId,
    root: &str,
    profile: GlslProfile,
) -> Result<ValueTestShaders, CompilerError> {
    let fref = resolve_root(ws, mid, root)?;
    let f = &ws.module(fref.module).functions[fref.func];
    if !f.param_decls.is_empty() {
        return Err(err(
            f.name_span,
            "value test: the root fn must take no parameters (a @test fn)",
        ));
    }
    let ret = match f
        .return_ty
        .map(|rt| glsl_ty(ws, &SpecSubst::default(), rt, f.name_span, "return"))
    {
        Some(Ok(GlslTy::Int)) => ValueTestRet::Int,
        Some(Ok(GlslTy::Float)) => ValueTestRet::Float,
        Some(Ok(GlslTy::Bool)) => ValueTestRet::Bool,
        Some(Ok(other)) => {
            return Err(err(
                f.name_span,
                format!(
                    "value test: return type `{}` is not a scalar (only int/float/bool return \
                     values are read back today)",
                    other.name()
                ),
            ));
        }
        _ => {
            return Err(err(
                f.name_span,
                "value test: the root fn must return int/float/bool",
            ));
        }
    };

    // The fn library runs the strict tier-0 profile pass; a body outside the
    // subset rejects here (the SKIP reason -- the A1 fn-emission work list).
    let defs = emit_shader_library(ws, mid, std::slice::from_ref(&root.to_string()))?;
    let call = mangle_fn(ws, f);

    let version = profile_header(profile);
    let (out_ty, out_expr) = match ret {
        ValueTestRet::Int => ("int", format!("{call}()")),
        ValueTestRet::Float => ("float", format!("{call}()")),
        ValueTestRet::Bool => ("int", format!("({call}() ? 1 : 0)")),
    };
    let fragment = format!(
        "{version}\n\n{defs}\nlayout(location = 0) out {out_ty} _rock_value_out;\n\
         void main() {{\n  _rock_value_out = {out_expr};\n}}\n"
    );
    // A fullscreen triangle from gl_VertexID -- no vertex buffers/attributes, so
    // the runner binds only an empty VAO. gl_VertexID is core since 3.0 / ES 3.0.
    let vertex = format!(
        "{version}\nvoid main() {{\n  \
         vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n  \
         gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n}}\n"
    );
    Ok(ValueTestShaders {
        vertex,
        fragment,
        ret,
    })
}

/// True iff `root` resolves to a `@shader(vertex)` fn -- the CLI routes to the
/// two-root vertex+fragment pipeline form when any root is a vertex entry.
pub fn is_vertex_shader_root(ws: &Workspace, mid: ModuleId, root: &str) -> bool {
    resolve_root(ws, mid, root).ok().is_some_and(|fref| {
        shader_stage(&ws.module(fref.module).functions[fref.func]) == Some("vertex")
    })
}

/// True iff `root` resolves to a `@shader(fragment)` fn -- the CLI uses this to
/// admit `--profile` for the single-root fullscreen form while still rejecting
/// it for the route-B fn library (which emits no `#version` line to switch).
pub fn is_fragment_shader_root(ws: &Workspace, mid: ModuleId, root: &str) -> bool {
    resolve_root(ws, mid, root).ok().is_some_and(|fref| {
        shader_stage(&ws.module(fref.module).functions[fref.func]) == Some("fragment")
    })
}

/// The `@shader(<stage>)` annotation's stage argument, if the fn carries one.
pub(crate) fn shader_stage(f: &Function) -> Option<&'static str> {
    let ann = f
        .annotations
        .iter()
        .find(|a| a.name.name.as_str() == "shader")?;
    match ann.args.first().map(|a| &a.value) {
        Some(AnnoValue::Ident(id)) if id.name.as_str() == "fragment" => Some("fragment"),
        Some(AnnoValue::Ident(id)) if id.name.as_str() == "vertex" => Some("vertex"),
        _ => Some("<unknown>"),
    }
}
