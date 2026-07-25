//! skull-codegen-glsl -- GLSL function-library emission (route B, `docs/shaders.md`).
//!
//! A second codegen backend over the shared `skull-mir` substrate (sibling of
//! `skull-codegen-lua`). Given a typechecked [`Workspace`] and a set of root fn
//! names, [`emit_shader_library`] emits GLSL function definitions for the roots
//! plus their reachable callees, topologically ordered (callees before callers).
//!
//! Two things make this a RESTRICTED profile, not a general backend:
//!   - Value structs stay first-class `vecN` (the `ScalarCtx` values-first-class
//!     flag), never the luajit scalar-leaf ABI.
//!   - A strict profile pass rejects everything above tier 0 (recursion, strings,
//!     `Vec`/`Map`/`Arena`, optionals, closures, `NativeBuffer`, coroutines, and
//!     any call with neither a mapping-table entry nor an emittable body). Never
//!     skip-and-continue; never emit a guess.
//!
//! The tier-0 intrinsic mapping table ([`intrinsic_template`]) is the hand-written
//! spec from `docs/shaders.md`, keyed by the callee's mangled signature (name +
//! GLSL param types) -- the future `@intrinsic` GLSL column.

mod structure;

use bone_semantic::hir::{AnnoValue, BuiltinTy, Function, StructDef, TyId, TypeCtor, TypeExpr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{FnList, ModuleId, Span};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_codegen_lua::mangle_fn;
use skull_mir::intrinsics::{Emission, IntrinsicEntry, IntrinsicTable};
use skull_mir::lower::{lower_function, ScalarCtx};
use skull_mir::mir::*;
use skull_parser::CompilerError;
use structure::{LoopRegion, Structure};
use ustr::Ustr;

/// A tier-0 GLSL type. Everything else is rejected by the profile pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GlslTy {
    Float,
    Int,
    Bool,
    Vec2,
    Vec3,
    Vec4,
    /// Bool-vector; LOCAL/TEMP only (GLSL forbids bool interpolants, and the tier
    /// has no bvec upload path -- rejected in attr/varying/uniform position).
    Bvec3,
    Mat4,
    /// Opaque; param/uniform position only (GLSL forbids sampler locals/returns).
    Sampler2D,
    /// Integer sampler (the R32I entity-id tier). Same opacity rules as
    /// Sampler2D; read via texture_id().
    ISampler2D,
}

impl GlslTy {
    fn name(self) -> &'static str {
        match self {
            GlslTy::Float => "float",
            GlslTy::Int => "int",
            GlslTy::Bool => "bool",
            GlslTy::Vec2 => "vec2",
            GlslTy::Vec3 => "vec3",
            GlslTy::Vec4 => "vec4",
            GlslTy::Bvec3 => "bvec3",
            GlslTy::Mat4 => "mat4",
            GlslTy::Sampler2D => "sampler2D",
            GlslTy::ISampler2D => "isampler2D",
        }
    }

    fn is_sampler(self) -> bool {
        matches!(self, GlslTy::Sampler2D | GlslTy::ISampler2D)
    }

    /// Uniform-declaration spelling. isampler2D carries `highp`: GLSL ES 3.00
    /// gives integer samplers NO default precision (an unqualified declaration
    /// is a compile error there); desktop GLSL accepts and ignores it.
    fn uniform_decl(self) -> &'static str {
        match self {
            GlslTy::ISampler2D => "highp isampler2D",
            other => other.name(),
        }
    }
}

fn err(span: Span, msg: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(span, msg.into())
}

/// The `TypeExpr` behind a static TyId (declared signature/field types are static).
fn ty_expr(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(m, idx) => Some(ws.module(m).ty_arena[idx]),
        TyId::Local(_) => None,
    }
}

/// Render a declared Rock type to its tier-0 GLSL type, or reject.
fn glsl_ty(ws: &Workspace, ty: TyId, span: Span, what: &str) -> Result<GlslTy, CompilerError> {
    match ty_expr(ws, ty) {
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => Ok(GlslTy::Float),
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => Ok(GlslTy::Int),
        Some(TypeExpr::Builtin(BuiltinTy::Bool)) => Ok(GlslTy::Bool),
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) => match n.as_str() {
            "Vec2" => Ok(GlslTy::Vec2),
            "Vec3" => Ok(GlslTy::Vec3),
            "Vec4" => Ok(GlslTy::Vec4),
            "Bvec3" => Ok(GlslTy::Bvec3),
            "Mat4" => Ok(GlslTy::Mat4),
            "Sampler2D" => Ok(GlslTy::Sampler2D),
            "ISampler2D" => Ok(GlslTy::ISampler2D),
            other => Err(err(span, format!("shader profile: {what} type `{other}` is not tier 0 (only float/int/bool/Vec2/Vec3/Vec4)"))),
        },
        _ => Err(err(span, format!("shader profile: {what} type is not tier 0 (only float/int/bool/Vec2/Vec3/Vec4)"))),
    }
}

/// Format an `f64` literal as GLSL, always keeping a decimal point (GLSL 330 is
/// strict: `float x = 1;` is an error, so `1.0` must never render as `1`).
fn fmt_float(f: f64) -> String {
    let s = format!("{f}");
    if s.contains('.') || s.contains('e') || s.contains('E') || s.contains("inf") || s.contains("nan") {
        s
    } else {
        format!("{s}.0")
    }
}

// --- reachability + profile pass --------------------------------------------

struct Reach<'a> {
    ws: &'a Workspace,
    scx: &'a ScalarCtx,
    /// Lowered MIR of every emittable (non-intrinsic) reachable fn, keyed by ref.
    bodies: HashMap<FuncRef, MirFunc>,
    /// DFS colouring: gray = on the current path (cycle -> recursion reject).
    gray: HashSet<FuncRef>,
    black: HashSet<FuncRef>,
    /// Post-order (callees before callers) of emittable fns.
    order: Vec<FuncRef>,
}

impl Reach<'_> {
    /// DFS from one root/callee. Intrinsics are leaves (not emitted, not walked).
    fn visit(&mut self, fref: FuncRef) -> Result<(), CompilerError> {
        if self.black.contains(&fref) {
            return Ok(());
        }
        let f = &self.ws.module(fref.module).functions[fref.func];
        // An intrinsic mapping OR a Vec operator is rendered inline at call sites
        // (a native builtin / infix op), never emitted as a GLSL definition, never
        // walked -- so it's a reachability leaf.
        if renders_inline(f) {
            self.black.insert(fref);
            return Ok(());
        }
        if f.is_extern || f.inline_body.is_some() {
            return Err(err(
                f.name_span,
                format!(
                    "shader profile: `{}` has no emittable body and no GLSL intrinsic mapping",
                    f.name
                ),
            ));
        }
        if self.gray.contains(&fref) {
            return Err(err(
                f.name_span,
                format!("shader profile: recursion through `{}` is not allowed on GLSL", f.name),
            ));
        }
        self.gray.insert(fref);

        // Signature must be tier 0.
        check_signature(self.ws, f)?;
        // Lower to MIR (values-first-class) and profile-check the body.
        let mf = lower_function(self.ws, self.scx, fref.module, fref.func).map_err(|e| {
            err(f.name_span, format!("shader profile: `{}` cannot lower for GLSL: {}", f.name, e.message))
        })?;
        let mut callees: Vec<FuncRef> = Vec::new();
        profile_body(f, &mf, &mut callees)?;

        for c in callees {
            self.visit(c)?;
        }
        self.gray.remove(&fref);
        self.black.insert(fref);
        self.order.push(fref);
        self.bodies.insert(fref, mf);
        Ok(())
    }
}

/// Reject a signature whose params/return aren't tier 0.
fn check_signature(ws: &Workspace, f: &Function) -> Result<(), CompilerError> {
    for pd in &f.param_decls {
        glsl_ty(ws, pd.ty, f.name_span, &format!("parameter `{}`", pd.name))?;
    }
    if let Some(rt) = f.return_ty {
        if glsl_ty(ws, rt, f.name_span, "return")?.is_sampler() {
            return Err(err(f.name_span, "shader profile: a sampler cannot be returned (GLSL forbids sampler locals/returns)"));
        }
    }
    Ok(())
}

/// Walk a fn's MIR, rejecting any non-tier-0 instruction and collecting the
/// [`FuncRef`]s it calls directly.
fn profile_body(
    f: &Function,
    mf: &MirFunc,
    callees: &mut Vec<FuncRef>,
) -> Result<(), CompilerError> {
    let span = f.name_span;
    for (_, inst) in mf.insts.iter() {
        match inst {
            Inst::Call { func, .. } => callees.push(*func),
            Inst::Const(Lit::Str(_)) => {
                return Err(err(span, format!("shader profile: `{}` uses a non-tier-0 construct (string literal) that has no GLSL lowering", f.name)));
            }
            // A bare `Nil` const may be a dead placeholder (`return` in expression
            // position emits an unused nil, `lower.rs`); a genuinely-used null /
            // optional is rejected when it renders (`render_val`). Rejecting every
            // nil here would refuse `if a { return .. } else { .. }`.
            Inst::Const(Lit::Nil) => {}
            // Allowed tier-0 instructions.
            Inst::Const(_)
            | Inst::Load(_)
            | Inst::IntAdd(..) | Inst::IntSub(..) | Inst::IntMul(..) | Inst::IntDiv(..) | Inst::IntMod(..)
            | Inst::FloatAdd(..) | Inst::FloatSub(..) | Inst::FloatMul(..) | Inst::FloatDiv(..) | Inst::FloatMod(..)
            | Inst::IntLt(..) | Inst::IntLeq(..) | Inst::IntGt(..) | Inst::IntGeq(..)
            | Inst::FloatLt(..) | Inst::FloatLeq(..) | Inst::FloatGt(..) | Inst::FloatGeq(..)
            | Inst::Eq(..) | Inst::NotEq(..)
            | Inst::Neg(_) | Inst::Not(_) | Inst::FloatToInt(_) | Inst::IntToFloat(_)
            | Inst::MakeValueStruct { .. }
            | Inst::Field { .. }
            | Inst::Store { .. } => {}
            // Everything else is above tier 0 (strings, containers, closures,
            // coroutines, enums, native buffers, bit ops, globals, ...).
            other => {
                return Err(err(
                    span,
                    format!("shader profile: `{}` uses a non-tier-0 construct ({}) that has no GLSL lowering", f.name, inst_kind(other)),
                ));
            }
        }
    }
    Ok(())
}

/// A short human name for a rejected instruction kind (for the error message).
fn inst_kind(inst: &Inst) -> &'static str {
    match inst {
        Inst::Global(_) => "global read",
        Inst::StringConcat(..) | Inst::ToString(_) | Inst::Format { .. } | Inst::FormatDyn { .. } => "string",
        Inst::BitAnd(..) | Inst::BitOr(..) | Inst::BitXor(..) | Inst::Shl(..) | Inst::Shr(..) | Inst::BitNot(_) => "bitwise op",
        Inst::CallMulti { .. } | Inst::CallValue { .. } | Inst::CallValueMulti { .. } | Inst::CallBoundMulti { .. } => "indirect/multi-value call",
        Inst::Lambda { .. } => "closure",
        Inst::MakeTuple(_) => "tuple",
        Inst::MakeVec(_) => "Vec",
        Inst::MakeStruct { .. } => "heap struct",
        Inst::MakeEnum { .. } | Inst::MakeEnumDyn { .. } => "enum",
        Inst::Index { .. } | Inst::Len { .. } | Inst::ArrayPush { .. } | Inst::StoreIndex { .. } => "container op",
        Inst::StoreField { .. } => "field mutation",
        Inst::StoreGlobal { .. } => "global write",
        Inst::CoroSetInner(_) | Inst::CoroHook { .. } | Inst::MapNext { .. } => "coroutine/iteration",
        _ => "unsupported instruction",
    }
}

// --- intrinsic mapping table ------------------------------------------------

/// The GLSL tier-0 intrinsic table (`docs/shaders.md`): one row per
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
    // GPU builtins (docs/shaders.md). frag_coord/discard take no
    // args -> bare builtins (discard renders as the statement `discard;`, see
    // emit_block_effects). Fragment-stage gating lives in the pipeline emitter.
    ("frag_coord", 0, "gl_FragCoord"),
    ("fwidth", 1, "fwidth($1)"),
    ("discard", 0, "discard"),
    ("vertex_id", 0, "gl_VertexID"),
    // Uniform-array indexing (decision 3): `at(arr, i)` on an UniformArray<T>
    // param renders as plain GLSL indexing of the declared uniform array.
    ("at", 2, "$1[$2]"),
    // Bool-vector tier (docs/shaders.md). isnan/isinf are GLES 3.00 builtins.
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
fn intrinsic_template(f: &Function) -> Option<&'static str> {
    let n = f.name.as_str();
    let arity = f.param_decls.iter().count();
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
            .map(|(name, arity, _)| IntrinsicEntry { name, arity: *arity })
            .collect()
    }
}

/// Whether a callee is rendered inline at call sites (an intrinsic template or a
/// Vec operator's native infix), rather than emitted as a GLSL fn definition. A
/// `__op_*` reaching codegen as a real call is always non-scalar (scalar ops lower
/// to native MIR insts), so a 2-arg `__op_*` is always a Vec operator.
fn renders_inline(f: &Function) -> bool {
    GlslIntrinsics.lookup(f).is_some()
        || (vec_op_symbol(f.name.as_str()).is_some() && f.param_decls.iter().count() == 2)
}

/// The GLSL infix operator symbol for a non-scalar `__op_*` (Vec operator) call.
fn vec_op_symbol(name: &str) -> Option<&'static str> {
    match name {
        "__op_add" => Some("+"),
        "__op_sub" => Some("-"),
        "__op_mul" => Some("*"),
        "__op_div" => Some("/"),
        _ => None,
    }
}

// --- per-function emission ---------------------------------------------------

/// A cosmetic-tier fold of a lowered `&&`/`||`/expression-`if` diamond back into
/// one GLSL expression (`docs/shaders.md`). The diamond stored its
/// result into a single-use cell across two single-store arms; instead of
/// emitting the diamond we render this expression at the cell's one use.
#[derive(Clone, Copy)]
struct FoldedDiamond {
    cond: Idx<Inst>,
    then_val: Idx<Inst>,
    else_val: Idx<Inst>,
    kind: FoldKind,
    /// The result cell's GLSL type -- ternary arms render at this type so an
    /// int-in-float-context arm keeps the coercion the diamond's store applied.
    result_ty: GlslTy,
}

#[derive(Clone, Copy, PartialEq)]
enum FoldKind {
    /// `if a then b else false` == `(a && b)`.
    And,
    /// `if a then true else b` == `(a || b)`.
    Or,
    /// `if a then b else c` == `(a ? b : c)`.
    Ternary,
}

struct FnEmit<'a> {
    ws: &'a Workspace,
    f: &'a MirFunc,
    /// GLSL return type (None = void); floats coerce int-literal returns.
    ret_ty: Option<GlslTy>,
    /// Cell -> emitted GLSL identifier.
    cell_name: HashMap<Idx<Cell>, String>,
    /// Cell -> GLSL type (params from signature, others from first store).
    cell_ty: HashMap<Idx<Cell>, GlslTy>,
    /// Loop headers matching the range-`for` shape -> their counted-loop info.
    counted: HashMap<Idx<Block>, LoopInfo>,
    /// Other natural-loop headers (general `while`) -> their region.
    general: HashMap<Idx<Block>, LoopRegion>,
    /// Dominators / post-dominators / natural loops.
    structure: Structure,
    /// Loop headers currently being emitted (so the header region walk doesn't
    /// re-wrap them as a fresh loop).
    active: HashSet<Idx<Block>>,
    out: String,
    indent: usize,
    /// Guard against unstructured control flow / infinite walks.
    visited: HashSet<Idx<Block>>,
    /// Entry mode: the cells of the "global-base" struct params -- attribute,
    /// uniform, and varyings structs, whose field reads render as bare GLSL
    /// globals (attributes/uniforms/varyings are globals, not struct members).
    global_cells: HashSet<Idx<Cell>>,
    /// Entry mode: global field name -> GLSL type (for `ty_of` on those reads).
    /// Names are collision-checked unique, so one combined map covers all bases.
    uniform_fields: HashMap<Ustr, GlslTy>,
    /// Cosmetic folds (`docs/shaders.md`, cosmetic tier). A diamond's
    /// single-use result cell -> the expression it renders as; the diamond itself
    /// is skipped, its declaration + stores suppressed.
    folded_cond: HashMap<Idx<Cell>, FoldedDiamond>,
    /// The branch block a `folded_cond` diamond starts at -> its result cell
    /// (so `emit_seq` skips the diamond and jumps to its join).
    folded_block: HashMap<Idx<Block>, Idx<Cell>>,
    /// A counted-loop bound temp folded into the for-header -> its const value.
    folded_bound: HashMap<Idx<Cell>, Idx<Inst>>,
    /// `void main()` mode: the fn is emitted as `void main()` and its terminal
    /// `return <MakeValueStruct>` destructures into a named global assignment per
    /// field (vertex varyings, or MRT fragment outputs).
    main_out: Option<MainOut>,
    /// The fn's name span -- body-level emit errors anchor here (MIR insts carry
    /// no spans, so per-expression precision is not available).
    fn_span: Span,
}

/// A `void main()` entry that destructures its terminal `return <struct
/// literal>` into named global assignments: vertex varyings (+ gl_Position) or
/// MRT fragment outputs (`layout(location=N) out vec4`).
#[derive(Clone)]
struct MainOut {
    /// Field name -> GLSL type (the assignment target's type).
    fields: HashMap<Ustr, GlslTy>,
    /// Vertex only: a `position` field becomes `gl_Position` (suppressed as a
    /// named output). Fragment MRT has no such remap -- every field is an out.
    position_is_gl: bool,
}

#[derive(Clone, Copy)]
struct LoopInfo {
    slot: Idx<Cell>,
    bound: Idx<Inst>,
    inclusive: bool,
    body_entry: Idx<Block>,
    latch: Idx<Block>,
    exit: Idx<Block>,
}

/// The enclosing loop's jump targets, mirroring skull-mir's `loop_targets`:
/// `Goto(cont)` -> `continue`, `Goto(brk)` -> `break`.
#[derive(Clone, Copy, Default)]
struct LoopCtx {
    cont: Option<Idx<Block>>,
    brk: Option<Idx<Block>>,
}

/// Emit `<ret> <name>(<params>) { <body> }` for one lowered fn.
fn emit_fn_def(
    ws: &Workspace,
    fref: FuncRef,
    mf: &MirFunc,
) -> Result<String, CompilerError> {
    emit_fn_def_with(ws, fref, mf, None)
}

/// [`emit_fn_def`], optionally in ENTRY mode: the uniform-struct and sampler
/// params are dropped from the GLSL signature (their reads render as bare
/// uniform globals) and the fn keeps its bare Rock name.
fn emit_fn_def_with(
    ws: &Workspace,
    fref: FuncRef,
    mf: &MirFunc,
    entry: Option<&EntryShape>,
) -> Result<String, CompilerError> {
    let f = &ws.module(fref.module).functions[fref.func];
    let emits_main = entry.is_some_and(|s| s.main_out.is_some());
    let name = if emits_main {
        "main".to_string()
    } else if entry.is_some() {
        f.name.as_str().to_string()
    } else {
        mangle_fn(ws, f)
    };
    // A `void main()` entry destructures its returned struct into globals
    // (gl_Position + varyings, or MRT outs) -- no signature return type.
    let ret_ty = if emits_main {
        None
    } else {
        match f.return_ty {
            Some(rt) => Some(glsl_ty(ws, rt, f.name_span, "return")?),
            None => None,
        }
    };
    let ret = ret_ty.map_or("void", |t| t.name());

    let st = structure::analyze(mf);
    let mut counted = HashMap::default();
    let mut general = HashMap::default();
    for (&h, region) in &st.loops {
        // The range-`for` fast path is valid only when the header's own `Branch`
        // IS the loop test (its else is the real exit). A `while i < N && ..`
        // header also looks like `i < N` with an `i + 1` latch, but its exit lies
        // inside the loop -- that one takes the general path.
        match match_counted_loop(mf, h) {
            Some(li) if li.exit == region.exit && region.body.contains(&li.latch) => {
                counted.insert(h, li);
            }
            _ => {
                general.insert(h, region.clone());
            }
        }
    }

    let mut e = FnEmit {
        ws,
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
        uniform_fields: entry.map(|s| s.uniform_fields.iter().copied().collect()).unwrap_or_default(),
        folded_cond: HashMap::default(),
        folded_block: HashMap::default(),
        folded_bound: HashMap::default(),
        main_out: entry.and_then(|s| s.main_out.clone()),
        fn_span: f.name_span,
    };
    e.assign_cell_names();
    // Global-base struct params + samplers + uniform arrays are dropped from the
    // GLSL signature.
    let dropped: HashSet<usize> = entry
        .map(|s| {
            s.global_params
                .iter()
                .copied()
                .chain(s.samplers.iter().map(|&(i, _, _)| i))
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
        .map(|s| s.global_params.iter().copied().chain(s.arrays.iter().copied()).collect())
        .unwrap_or_default();
    for (i, &c) in mf.params.iter().enumerate() {
        if struct_params.contains(&i) {
            continue;
        }
        let ty = glsl_ty(ws, f.param_decls[i].ty, f.name_span, "parameter")?;
        e.cell_ty.insert(c, ty);
    }
    e.compute_cell_types();
    e.compute_folds();

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

impl FnEmit<'_> {
    fn push(&mut self, line: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(line);
        self.out.push('\n');
    }

    /// Assign a unique GLSL identifier to every cell (source name, deduped).
    fn assign_cell_names(&mut self) {
        let mut used: HashSet<String> = HashSet::default();
        for (cid, cell) in self.f.cells.iter() {
            let base = cell.name.map(|n| n.as_str().to_string()).unwrap_or_else(|| format!("t{}", cid.into_raw().into_u32()));
            let mut name = base.clone();
            let mut k = 0;
            while used.contains(&name) {
                k += 1;
                name = format!("{base}_{k}");
            }
            used.insert(name.clone());
            self.cell_name.insert(cid, name);
        }
    }

    /// Whether `iid` is a `Load` of an entry-mode global-base struct param cell
    /// (attribute/uniform/varyings) -- a field read off it renders as a bare
    /// global, not a struct member.
    fn is_uniform_base(&self, iid: Idx<Inst>) -> bool {
        matches!(self.f.insts[iid], Inst::Load(c) if self.global_cells.contains(&c))
    }

    /// Type every non-param cell: prefer bone's binding type ([`Cell::ty`]) where
    /// it maps to a tier-0 GLSL type (fixes `let y: float = 0`, whose first store's
    /// rep is int); fall back to first-store inference (RPO) for compiler-minted
    /// temps (if-result, loop counter) that carry no binding type.
    fn compute_cell_types(&mut self) {
        for (cid, cell) in self.f.cells.iter() {
            if self.cell_ty.contains_key(&cid) {
                continue;
            }
            if let Some(ty) = cell.ty {
                if let Ok(g) = glsl_ty(self.ws, ty, Span::default(), "local") {
                    self.cell_ty.insert(cid, g);
                }
            }
        }
        let rpo = reverse_postorder(self.f, self.f.entry);
        for &b in &rpo {
            for &iid in &self.f.blocks[b].insts {
                if let Inst::Store { cell, value } = self.f.insts[iid] {
                    if !self.cell_ty.contains_key(&cell) {
                        let ty = self.ty_of(value);
                        self.cell_ty.insert(cell, ty);
                    }
                }
            }
        }
    }

    /// Detect the cosmetic-tier folds (`docs/shaders.md`): `&&`/`||`
    /// short-circuit diamonds collapse back to infix boolean expressions.
    fn compute_folds(&mut self) {
        let reach: HashSet<Idx<Block>> =
            reverse_postorder(self.f, self.f.entry).into_iter().collect();
        // Cell load-counts over reachable code -- a fold is only sound when the
        // diamond's result cell has exactly one use.
        let mut load_count: HashMap<Idx<Cell>, usize> = HashMap::default();
        for &b in &reach {
            for &iid in &self.f.blocks[b].insts {
                if let Inst::Load(c) = self.f.insts[iid] {
                    *load_count.entry(c).or_default() += 1;
                }
            }
        }

        // Short-circuit diamonds: a `Branch` whose two arms each single-store the
        // same single-use bool cell, one arm storing the `&&`/`||` const identity.
        // `a && b` lowers to `if a then b else false`; `a || b` to
        // `if a then true else b` (`lower_and`/`lower_or`, no And/Or inst).
        for b in reach.iter().copied() {
            let Term::Branch { cond, then_bb, else_bb } = self.f.blocks[b].term else {
                continue;
            };
            let Some(join) = self.structure.ipdom(b) else { continue };
            let Some((tcell, tval)) = self.single_store_arm(then_bb, join) else { continue };
            let Some((ecell, eval)) = self.single_store_arm(else_bb, join) else { continue };
            if tcell != ecell {
                continue;
            }
            let rcell = tcell;
            if load_count.get(&rcell).copied().unwrap_or(0) != 1 {
                continue;
            }
            let Some(&result_ty) = self.cell_ty.get(&rcell) else { continue };
            // Soundness: the fold moves the expression to the cell's use site, so
            // its inputs must be unchanged there. The arms go straight to `join`
            // and store only `rcell`, so the only place an input could be mutated
            // between the diamond and the use is `join` before the use -- require
            // the use to be in `join` with no store preceding it.
            if !self.use_hoistable_to(rcell, join) {
                continue;
            }
            let kind = if result_ty == GlslTy::Bool && self.is_const_bool(eval, false) {
                FoldKind::And
            } else if result_ty == GlslTy::Bool && self.is_const_bool(tval, true) {
                FoldKind::Or
            } else {
                FoldKind::Ternary
            };
            self.folded_cond.insert(
                rcell,
                FoldedDiamond { cond, then_val: tval, else_val: eval, kind, result_ty },
            );
            self.folded_block.insert(b, rcell);
        }

        // Bound temps (item 3): a counted loop's bound `t = <expr>; for (; i < t;
        // ..)` folds to `for (; i < <expr>; ..)`, dropping the single-use temp.
        // The bound temp is a range-for's `hi`, evaluated once in the preheader
        // (match_counted_loop only matches range-fors), so inlining is sound when
        // `<expr>` is loop-invariant -- a const, or pure int arithmetic over cells
        // that are stored exactly once (their store dominates the preheader, so
        // re-reading in the folded condition yields the same value). Folding it
        // is not just a cleanup: a GLES/GL driver miscompiles `t = radius + 1; for
        // (; y < t; ..)` for a nested runtime-bounded kernel (the outline_detect
        // classify loop found no neighbors), while the inlined `y < (radius + 1)`
        // is correct -- pinned by bitgun `outline_classify_sim`.
        let bounds: Vec<Idx<Inst>> = self.counted.values().map(|li| li.bound).collect();
        for bound in bounds {
            let Inst::Load(bcell) = self.f.insts[bound] else { continue };
            if load_count.get(&bcell).copied().unwrap_or(0) != 1 {
                continue;
            }
            let Some(v) = self.single_store_value(bcell) else { continue };
            if self.bound_value_is_invariant(v) {
                self.folded_bound.insert(bcell, v);
            }
        }
    }

    /// Whether a counted-loop bound value is loop-invariant enough to inline into
    /// the `for` header (see the fold in `precompute`). Conservative: a const, or
    /// pure int arithmetic / int<->float casts over cells that are stored exactly
    /// once in the fn (so re-reading in the folded condition can't change). The
    /// loop counter is multi-store, so it can never sneak in. Anything else (a
    /// call like `min(n, 16)`, a texture read) stays a temp -- the safe default.
    fn bound_value_is_invariant(&self, v: Idx<Inst>) -> bool {
        match self.f.insts[v] {
            Inst::Const(_) => true,
            Inst::Load(c) => !self.global_cells.contains(&c) && self.single_store_value(c).is_some(),
            Inst::IntAdd(a, b) | Inst::IntSub(a, b) | Inst::IntMul(a, b) => {
                self.bound_value_is_invariant(a) && self.bound_value_is_invariant(b)
            }
            Inst::IntToFloat(x) | Inst::FloatToInt(x) => self.bound_value_is_invariant(x),
            _ => false,
        }
    }

    /// The single value stored to `cell`, or `None` if it is stored zero or more
    /// than once anywhere in the fn.
    fn single_store_value(&self, cell: Idx<Cell>) -> Option<Idx<Inst>> {
        let mut found = None;
        for (_, blk) in self.f.blocks.iter() {
            for &iid in &blk.insts {
                if let Inst::Store { cell: c, value } = self.f.insts[iid] {
                    if c == cell {
                        if found.is_some() {
                            return None;
                        }
                        found = Some(value);
                    }
                }
            }
        }
        found
    }

    /// Whether `rcell`'s single use is in `join` with no `Store` preceding it
    /// there -- the condition that makes hoisting the folded expression to the
    /// use site sound (no input can be mutated between the diamond and the use).
    fn use_hoistable_to(&self, rcell: Idx<Cell>, join: Idx<Block>) -> bool {
        for &iid in &self.f.blocks[join].insts {
            match self.f.insts[iid] {
                Inst::Load(c) if c == rcell => return true,
                Inst::Store { .. } => return false,
                _ => {}
            }
        }
        false
    }

    /// If `arm` is a single-store block (exactly one `Store`, any number of pure
    /// value insts) ending `Goto(join)`, return its `(cell, stored value)`.
    fn single_store_arm(&self, arm: Idx<Block>, join: Idx<Block>) -> Option<(Idx<Cell>, Idx<Inst>)> {
        if self.f.blocks[arm].term != Term::Goto(join) {
            return None;
        }
        let mut store = None;
        for &iid in &self.f.blocks[arm].insts {
            if let Inst::Store { cell, value } = self.f.insts[iid] {
                if store.is_some() {
                    return None;
                }
                store = Some((cell, value));
            }
        }
        store
    }

    fn is_const_bool(&self, iid: Idx<Inst>, want: bool) -> bool {
        matches!(self.f.insts[iid], Inst::Const(Lit::Bool(b)) if b == want)
    }

    /// Render a folded diamond as one GLSL expression.
    fn render_folded(&self, fd: &FoldedDiamond) -> Result<String, CompilerError> {
        let cond = self.render_val(fd.cond)?;
        Ok(match fd.kind {
            FoldKind::And => format!("({} && {})", cond, self.render_val(fd.then_val)?),
            FoldKind::Or => format!("({} || {})", cond, self.render_val(fd.else_val)?),
            FoldKind::Ternary => format!(
                "({} ? {} : {})",
                cond,
                self.render_val_t(fd.then_val, fd.result_ty)?,
                self.render_val_t(fd.else_val, fd.result_ty)?
            ),
        })
    }

    /// The GLSL type of a value instruction.
    fn ty_of(&self, iid: Idx<Inst>) -> GlslTy {
        match &self.f.insts[iid] {
            Inst::Const(Lit::Float(_)) => GlslTy::Float,
            Inst::Const(Lit::Int(_)) => GlslTy::Int,
            Inst::Const(Lit::Bool(_)) => GlslTy::Bool,
            Inst::Const(Lit::Nil) | Inst::Const(Lit::Str(_)) => GlslTy::Float, // rejected earlier
            Inst::FloatAdd(..) | Inst::FloatSub(..) | Inst::FloatMul(..) | Inst::FloatDiv(..) | Inst::FloatMod(..) => GlslTy::Float,
            Inst::IntAdd(..) | Inst::IntSub(..) | Inst::IntMul(..) | Inst::IntDiv(..) | Inst::IntMod(..) => GlslTy::Int,
            Inst::IntLt(..) | Inst::IntLeq(..) | Inst::IntGt(..) | Inst::IntGeq(..)
            | Inst::FloatLt(..) | Inst::FloatLeq(..) | Inst::FloatGt(..) | Inst::FloatGeq(..)
            | Inst::Eq(..) | Inst::NotEq(..) | Inst::Not(_) => GlslTy::Bool,
            Inst::Neg(x) => self.ty_of(*x),
            Inst::FloatToInt(_) => GlslTy::Int,
            Inst::IntToFloat(_) => GlslTy::Float,
            Inst::Load(c) => self.cell_ty.get(c).copied().unwrap_or(GlslTy::Float),
            Inst::Field { base, field } => {
                if self.is_uniform_base(*base) {
                    self.uniform_fields.get(field).copied().unwrap_or(GlslTy::Float)
                } else if self.ty_of(*base) == GlslTy::Bvec3 {
                    GlslTy::Bool // bvec3 component read
                } else {
                    GlslTy::Float // Vec component
                }
            }
            Inst::MakeValueStruct { name, .. } => match name.as_str() {
                "Vec2" => GlslTy::Vec2,
                "Vec3" => GlslTy::Vec3,
                "Vec4" => GlslTy::Vec4,
                "Bvec3" => GlslTy::Bvec3,
                "Mat4" => GlslTy::Mat4,
                _ => GlslTy::Float,
            },
            Inst::Call { func, args } => {
                let cf = &self.ws.module(func.module).functions[func.func];
                match cf.return_ty.and_then(|rt| glsl_ty(self.ws, rt, cf.name_span, "return").ok()) {
                    Some(t) => t,
                    // Generic-return genType builtins (min<T>/max<T>/clamp/abs):
                    // GLSL's result type is the argument type. Load-bearing under
                    // ES300, which has no implicit int->float on assignment.
                    None if matches!(cf.name.as_str(), "min" | "max" | "clamp" | "abs")
                        && !args.is_empty() =>
                    {
                        self.ty_of(args[0])
                    }
                    None => GlslTy::Float,
                }
            }
            _ => GlslTy::Float,
        }
    }

    /// Render at a float context slot. int->float subsumption is materialized at
    /// lowering (`Inst::IntToFloat`, which `render_val` folds/wraps), so an
    /// int-typed value reaching here that is NOT already coerced is a MISSED
    /// lowering sink -- a hard emit error, never silent driver-dependent GLSL.
    /// (GLSL ES has no implicit int->float; desktop drivers vary on the corner.)
    fn render_val_t(&self, iid: Idx<Inst>, want: GlslTy) -> Result<String, CompilerError> {
        if want == GlslTy::Float && self.ty_of(iid) == GlslTy::Int {
            return Err(err(
                self.span(),
                format!(
                    "shader profile: int-typed value ({}) in a float context without an IntToFloat coercion -- a missed lowering sink (docs/shaders.md)",
                    inst_kind(&self.f.insts[iid])
                ),
            ));
        }
        self.render_val(iid)
    }

    /// Render a value instruction as a GLSL expression (inlined; MIR values are
    /// single-use, so no CSE duplication).
    fn render_val(&self, iid: Idx<Inst>) -> Result<String, CompilerError> {
        let bin = |a: &Idx<Inst>, op: &str, b: &Idx<Inst>, s: &Self| -> Result<String, CompilerError> {
            Ok(format!("({} {} {})", s.render_val(*a)?, op, s.render_val(*b)?))
        };
        let fbin = |a: &Idx<Inst>, op: &str, b: &Idx<Inst>, s: &Self| -> Result<String, CompilerError> {
            Ok(format!(
                "({} {} {})",
                s.render_val_t(*a, GlslTy::Float)?,
                op,
                s.render_val_t(*b, GlslTy::Float)?
            ))
        };
        Ok(match &self.f.insts[iid] {
            Inst::Const(Lit::Float(f)) => fmt_float(*f),
            Inst::Const(Lit::Int(i)) => i.to_string(),
            Inst::Const(Lit::Bool(b)) => b.to_string(),
            Inst::Const(Lit::Nil) => return Err(err(self.span(), "shader profile: null literal is not tier 0")),
            Inst::Const(Lit::Str(_)) => return Err(err(self.span(), "shader profile: string literal is not tier 0")),
            Inst::Load(c) => {
                // A global-base struct param has no GLSL identity -- its fields
                // are bare globals, read via `Field` (which never renders the
                // base). A `Load` reaching expression position is a copy/alias
                // (`let v = vary`) that would reference the dropped param name.
                if self.global_cells.contains(c) {
                    return Err(err(self.span(), format!(
                        "shader profile: the struct param `{}` can only be read field-by-field (its fields are GLSL globals) -- it cannot be copied or aliased",
                        self.cell_name[c]
                    )));
                }
                if let Some(fd) = self.folded_cond.get(c) {
                    self.render_folded(fd)?
                } else if let Some(&v) = self.folded_bound.get(c) {
                    self.render_val(v)?
                } else {
                    self.cell_name[c].clone()
                }
            }
            Inst::IntAdd(a, b) => bin(a, "+", b, self)?,
            Inst::FloatAdd(a, b) => fbin(a, "+", b, self)?,
            Inst::IntSub(a, b) => bin(a, "-", b, self)?,
            Inst::FloatSub(a, b) => fbin(a, "-", b, self)?,
            Inst::IntMul(a, b) => bin(a, "*", b, self)?,
            Inst::FloatMul(a, b) => fbin(a, "*", b, self)?,
            Inst::FloatDiv(a, b) => fbin(a, "/", b, self)?,
            Inst::FloatMod(a, b) => format!(
                "mod({}, {})",
                self.render_val_t(*a, GlslTy::Float)?,
                self.render_val_t(*b, GlslTy::Float)?
            ),
            // GLSL int / and % are undefined on negatives -> floored helpers.
            Inst::IntDiv(a, b) => format!("__rock_idiv({}, {})", self.render_val(*a)?, self.render_val(*b)?),
            Inst::IntMod(a, b) => format!("__rock_imod({}, {})", self.render_val(*a)?, self.render_val(*b)?),
            Inst::IntLt(a, b) => bin(a, "<", b, self)?,
            Inst::FloatLt(a, b) => fbin(a, "<", b, self)?,
            Inst::IntLeq(a, b) => bin(a, "<=", b, self)?,
            Inst::FloatLeq(a, b) => fbin(a, "<=", b, self)?,
            Inst::IntGt(a, b) => bin(a, ">", b, self)?,
            Inst::FloatGt(a, b) => fbin(a, ">", b, self)?,
            Inst::IntGeq(a, b) => bin(a, ">=", b, self)?,
            Inst::FloatGeq(a, b) => fbin(a, ">=", b, self)?,
            // Untyped Eq/NotEq: operands arrive same-typed (a mixed `float == int`
            // was int->float-coerced at lowering), so render a plain compare.
            Inst::Eq(a, b) => bin(a, "==", b, self)?,
            Inst::NotEq(a, b) => bin(a, "!=", b, self)?,
            Inst::Neg(x) => format!("(-{})", self.render_val(*x)?),
            Inst::Not(x) => format!("(!{})", self.render_val(*x)?),
            Inst::FloatToInt(x) => format!("int({})", self.render_val(*x)?),
            // int->float subsumption (inserted at lowering): fold an int literal
            // to float text, else materialize the `float(...)` conversion.
            Inst::IntToFloat(x) => match &self.f.insts[*x] {
                Inst::Const(Lit::Int(i)) => fmt_float(*i as f64),
                _ => format!("float({})", self.render_val(*x)?),
            },
            Inst::Field { base, field } => {
                if self.is_uniform_base(*base) {
                    field.as_str().to_string() // uniform global, not a struct member
                } else {
                    format!("{}.{}", self.render_val(*base)?, field)
                }
            }
            Inst::MakeValueStruct { name, fields } => {
                // Bvec3 components render at a bool context; the vecN ctors take
                // float components (int operands materialize an IntToFloat).
                let (ctor, comp_ty) = match name.as_str() {
                    "Vec2" => ("vec2", GlslTy::Float),
                    "Vec3" => ("vec3", GlslTy::Float),
                    "Vec4" => ("vec4", GlslTy::Float),
                    "Bvec3" => ("bvec3", GlslTy::Bool),
                    other => return Err(err(self.span(), format!("shader profile: value struct `{other}` is not tier 0"))),
                };
                let args: Result<Vec<String>, CompilerError> =
                    fields.iter().map(|(_, v)| self.render_val_t(*v, comp_ty)).collect();
                format!("{ctor}({})", args?.join(", "))
            }
            Inst::Call { func, args } => self.render_call(*func, args)?,
            other => return Err(err(self.span(), format!("shader profile: cannot render {} as a GLSL expression", inst_kind(other)))),
        })
    }

    fn render_call(&self, func: FuncRef, args: &[Idx<Inst>]) -> Result<String, CompilerError> {
        let cf = &self.ws.module(func.module).functions[func.func];
        let name = cf.name.as_str();
        // A non-scalar `__op_*` call is a Vec operator -> native infix. An int
        // operand (`*(Vec2, int)`) is cast to float (GLSL has no vec*int).
        if let Some(sym) = vec_op_symbol(name) {
            if args.len() == 2 {
                let render_operand = |i: usize, e: &Self| -> Result<String, CompilerError> {
                    let pt = glsl_ty(e.ws, cf.param_decls[i].ty, cf.name_span, "operand")?;
                    let s = e.render_val(args[i])?;
                    Ok(if pt == GlslTy::Int { format!("float({s})") } else { s })
                };
                return Ok(format!("({} {} {})", render_operand(0, self)?, sym, render_operand(1, self)?));
            }
        }
        // Args render against the declared param type where it maps to GLSL
        // (a generic param, e.g. `min<T>`, doesn't -- those render plain).
        let arg_t = |i: usize, a: Idx<Inst>, e: &Self| -> Result<String, CompilerError> {
            match cf.param_decls.get(i).and_then(|p| glsl_ty(e.ws, p.ty, cf.name_span, "arg").ok()) {
                Some(want) => e.render_val_t(a, want),
                None => e.render_val(a),
            }
        };
        // A `v(...)` ctor call with a vector param (the mixed ctors in
        // stdlib/vec.rock, `v(rgb, 1.0)`) must not hit the arity-keyed scalar
        // rows (`("v", 2)` -> vec2) -- the (name, arity) same-arity type-drift
        // hazard. GLSL constructors take mixed components natively, so emit
        // vecN(...) with N = the summed component count.
        if name == "v"
            && cf.param_decls.iter().any(|p| {
                matches!(
                    glsl_ty(self.ws, p.ty, cf.name_span, "ctor param").ok(),
                    Some(GlslTy::Vec2 | GlslTy::Vec3 | GlslTy::Vec4)
                )
            })
        {
            let n: usize = cf
                .param_decls
                .iter()
                .map(|p| match glsl_ty(self.ws, p.ty, cf.name_span, "ctor param").ok() {
                    Some(GlslTy::Vec2) => 2,
                    Some(GlslTy::Vec3) => 3,
                    Some(GlslTy::Vec4) => 4,
                    _ => 1,
                })
                .sum();
            let ctor = match n {
                2 => "vec2",
                3 => "vec3",
                4 => "vec4",
                _ => {
                    return Err(err(
                        self.span(),
                        format!("shader profile: v(...) ctor with {n} total components"),
                    ))
                }
            };
            let rargs: Result<Vec<String>, CompilerError> =
                args.iter().enumerate().map(|(i, a)| arg_t(i, *a, self)).collect();
            return Ok(format!("{ctor}({})", rargs?.join(", ")));
        }
        if let Some(Emission::Template(tmpl)) = GlslIntrinsics.lookup(cf) {
            let mut s = tmpl.to_string();
            for (i, a) in args.iter().enumerate() {
                s = s.replace(&format!("${}", i + 1), &arg_t(i, *a, self)?);
            }
            return Ok(s);
        }
        // An emittable user fn -> a plain call to its (mangled) GLSL name.
        let gname = mangle_fn(self.ws, cf);
        let rargs: Result<Vec<String>, CompilerError> =
            args.iter().enumerate().map(|(i, a)| arg_t(i, *a, self)).collect();
        Ok(format!("{gname}({})", rargs?.join(", ")))
    }

    fn span(&self) -> Span {
        self.fn_span
    }

    /// Emit a structured sequence of blocks starting at `start`, stopping before
    /// `stop` (its continuation). Handles straight-line code, if-diamonds,
    /// counted `for` loops, and general `while` loops; `ctx` carries the
    /// enclosing loop's break/continue targets.
    fn emit_seq(&mut self, start: Idx<Block>, stop: Option<Idx<Block>>, ctx: LoopCtx) -> Result<(), CompilerError> {
        let mut cur = start;
        loop {
            if Some(cur) == stop {
                return Ok(());
            }
            // Loop headers are dispatched structurally (before the visited guard):
            // `emit_general` re-enters `emit_seq` on the header for its condition
            // region, so the header must not already be marked visited.
            if !self.active.contains(&cur) {
                if let Some(li) = self.counted.get(&cur).copied() {
                    self.emit_counted(li)?;
                    cur = li.exit;
                    continue;
                }
                if let Some(region) = self.general.get(&cur).cloned() {
                    self.emit_general(&region, ctx)?;
                    cur = region.exit;
                    continue;
                }
            }
            if !self.visited.insert(cur) {
                return Err(err(self.span(), "shader profile: unstructured control flow (unsupported loop/branch shape)"));
            }
            self.emit_block_effects(cur)?;
            match self.f.blocks[cur].term.clone() {
                Term::Return(vs) => {
                    self.emit_return(&vs)?;
                    return Ok(());
                }
                Term::Unreachable => return Ok(()),
                Term::Goto(t) => {
                    if Some(t) == ctx.cont {
                        self.push("continue;");
                        return Ok(());
                    }
                    if Some(t) == ctx.brk {
                        self.push("break;");
                        return Ok(());
                    }
                    if Some(t) == stop {
                        return Ok(());
                    }
                    cur = t;
                }
                Term::Branch { cond, then_bb, else_bb } => {
                    // A folded short-circuit / ternary diamond emits nothing here --
                    // its expression renders at the result cell's one use. Skip to
                    // the join (ipdom); both arms are absorbed.
                    if self.folded_block.contains_key(&cur) {
                        match self.structure.ipdom(cur) {
                            Some(join) => cur = join,
                            None => return Ok(()),
                        }
                    } else {
                        match self.emit_branch(cur, cond, then_bb, else_bb, stop, ctx)? {
                            Some(m) => cur = m,
                            None => return Ok(()),
                        }
                    }
                }
                Term::Switch { .. } => {
                    return Err(err(self.span(), "shader profile: `match` is not tier 0"));
                }
                Term::Yield { .. } => {
                    return Err(err(self.span(), "shader profile: `yield` (coroutine) is not tier 0"));
                }
            }
        }
    }

    /// Whether `b` produces no statement and just forwards to another block
    /// (`Some(target)`): the empty else/then arm of a statement `if`, or a bare
    /// `break`/`continue` block.
    fn empty_forward(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        if let Term::Goto(t) = self.f.blocks[b].term {
            let has_store = self.f.blocks[b]
                .insts
                .iter()
                .any(|&i| matches!(self.f.insts[i], Inst::Store { .. }));
            if !has_store {
                return Some(t);
            }
        }
        None
    }

    /// Emit an `if`-diamond for a `Branch`. Returns the block to continue at
    /// (the merge / join), or `None` when both arms diverge and the arms have
    /// absorbed the continuation.
    fn emit_branch(
        &mut self,
        blk: Idx<Block>,
        cond: Idx<Inst>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<Option<Idx<Block>>, CompilerError> {
        let cond_s = self.render_val(cond)?;
        // Empty else -> then-only `if`; continue at the else target.
        if let Some(j) = self.empty_forward(else_bb) {
            self.push(&format!("if ({cond_s}) {{"));
            self.indent += 1;
            self.emit_seq(then_bb, Some(j), ctx)?;
            self.indent -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        // Empty then -> invert and emit the else arm only.
        if let Some(j) = self.empty_forward(then_bb) {
            self.push(&format!("if (!({cond_s})) {{"));
            self.indent += 1;
            self.emit_seq(else_bb, Some(j), ctx)?;
            self.indent -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        // Full diamond. The merge is ipdom, unless an arm diverges (ipdom
        // collapses to the virtual exit or a loop target) -- then the arms absorb
        // the continuation up to the enclosing stop and there is no fall-through.
        let merge = self
            .structure
            .ipdom(blk)
            .filter(|&m| Some(m) != ctx.cont && Some(m) != ctx.brk);
        let arm_stop = merge.or(stop);
        self.push(&format!("if ({cond_s}) {{"));
        self.indent += 1;
        self.emit_seq(then_bb, arm_stop, ctx)?;
        self.indent -= 1;
        self.push("} else {");
        self.indent += 1;
        self.emit_seq(else_bb, arm_stop, ctx)?;
        self.indent -= 1;
        self.push("}");
        Ok(merge)
    }

    /// Emit the straight-line effect statements of a block: Stores (assignments)
    /// and void (effect-only) calls like `discard()`. Value-returning insts are
    /// inlined at their use, so they produce no statement here.
    fn emit_block_effects(&mut self, b: Idx<Block>) -> Result<(), CompilerError> {
        for iid in self.f.blocks[b].insts.clone() {
            let line: Option<String> = match &self.f.insts[iid] {
                Inst::Store { cell, value } => {
                    let (cell, value) = (*cell, *value);
                    // Folded diamond result / bound temp: no storage, inlined.
                    if self.folded_cond.contains_key(&cell) || self.folded_bound.contains_key(&cell) {
                        None
                    } else {
                        let rhs = match self.cell_ty.get(&cell) {
                            Some(&want) => self.render_val_t(value, want)?,
                            None => self.render_val(value)?,
                        };
                        Some(format!("{} = {};", self.cell_name[&cell], rhs))
                    }
                }
                // A void call in statement position (discard()): its result is
                // never consumed, so it never gets inlined at a use -- emit it here.
                Inst::Call { func, args } => {
                    let cf = &self.ws.module(func.module).functions[func.func];
                    if cf.return_ty.is_none() {
                        Some(format!("{};", self.render_call(*func, args)?))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(line) = line {
                self.push(&line);
            }
        }
        Ok(())
    }

    fn emit_return(&mut self, vs: &[Idx<Inst>]) -> Result<(), CompilerError> {
        // `void main()` mode: `return <Struct { field, ... }>` destructures into a
        // named global assignment per field -- vertex varyings (a `position` field
        // -> gl_Position) or MRT fragment outputs.
        if let Some(mo) = self.main_out.clone() {
            let [v] = vs else {
                return Err(err(self.span(), "shader entry: a void-main entry must return its output struct"));
            };
            let fields: Vec<(Ustr, Idx<Inst>)> = match &self.f.insts[*v] {
                Inst::MakeValueStruct { fields, .. } => fields.iter().map(|&(n, x)| (n, x)).collect(),
                _ => {
                    return Err(err(
                        self.span(),
                        "shader entry: a void-main entry must return an output struct literal",
                    ))
                }
            };
            for (name, val) in fields {
                let (target, want) = if mo.position_is_gl && name.as_str() == "position" {
                    ("gl_Position".to_string(), GlslTy::Vec4)
                } else {
                    (name.to_string(), mo.fields.get(&name).copied().unwrap_or(GlslTy::Float))
                };
                let s = self.render_val_t(val, want)?;
                self.push(&format!("{target} = {s};"));
            }
            return Ok(());
        }
        match vs {
            [] => self.push("return;"),
            [v] => {
                let s = match self.ret_ty {
                    Some(want) => self.render_val_t(*v, want)?,
                    None => self.render_val(*v)?,
                };
                self.push(&format!("return {s};"));
            }
            _ => return Err(err(self.span(), "shader profile: multi-value return is not tier 0")),
        }
        Ok(())
    }

    fn emit_counted(&mut self, li: LoopInfo) -> Result<(), CompilerError> {
        let slot = self.cell_name[&li.slot].clone();
        let bound = self.render_val(li.bound)?;
        let cmp = if li.inclusive { "<=" } else { "<" };
        self.push(&format!("for (; {slot} {cmp} {bound}; {slot} = {slot} + 1) {{"));
        self.indent += 1;
        let body_len = self.out.len();
        let ctx = LoopCtx { cont: Some(li.latch), brk: Some(li.exit) };
        self.emit_seq(li.body_entry, None, ctx)?;
        self.trim_trailing_continue(body_len);
        self.indent -= 1;
        self.push("}");
        Ok(())
    }

    /// A general `while`: `while (true) { <cond region>; if (!(cond)) break; <body> }`.
    /// The condition region may be a multi-block diamond (`while a && b`), so it
    /// goes through the structurizer recursively.
    fn emit_general(&mut self, region: &LoopRegion, outer: LoopCtx) -> Result<(), CompilerError> {
        let h = region.header;
        let exit = region.exit;
        let (test, cond, body_start, break_when_true) = self.find_test(region)?;

        self.active.insert(h);
        self.push("while (true) {");
        self.indent += 1;

        self.emit_seq(h, Some(test), outer)?;
        self.emit_block_effects(test)?;
        self.visited.insert(test);

        let cond_s = self.render_val(cond)?;
        if break_when_true {
            self.push(&format!("if ({cond_s}) {{ break; }}"));
        } else {
            self.push(&format!("if (!({cond_s})) {{ break; }}"));
        }

        let ctx = LoopCtx { cont: Some(h), brk: Some(exit) };
        let body_len = self.out.len();
        self.emit_seq(body_start, None, ctx)?;
        self.trim_trailing_continue(body_len);

        self.indent -= 1;
        self.push("}");
        self.active.remove(&h);
        Ok(())
    }

    /// The loop's condition test: the unique `Branch` in the body with a direct
    /// successor to the exit. Returns `(test block, cond, body entry, break-when-cond-true)`.
    fn find_test(
        &self,
        region: &LoopRegion,
    ) -> Result<(Idx<Block>, Idx<Inst>, Idx<Block>, bool), CompilerError> {
        for &b in &region.body {
            if let Term::Branch { cond, then_bb, else_bb } = self.f.blocks[b].term {
                if then_bb == region.exit {
                    return Ok((b, cond, else_bb, true));
                }
                if else_bb == region.exit {
                    return Ok((b, cond, then_bb, false));
                }
            }
        }
        Err(err(self.span(), "shader profile: unstructured loop (no single condition test)"))
    }

    /// Drop a redundant trailing `continue;` that is the loop body's natural
    /// back-edge fall-through (indented exactly at the body level).
    fn trim_trailing_continue(&mut self, start: usize) {
        let line = format!("{}continue;\n", "  ".repeat(self.indent));
        if self.out.len() >= start + line.len() && self.out.ends_with(&line) {
            let cut = self.out.len() - line.len();
            if cut >= start && (cut == 0 || self.out.as_bytes()[cut - 1] == b'\n') {
                self.out.truncate(cut);
            }
        }
    }
}

// --- counted-loop recognition (mirrors skull-codegen-lua's, for the range-for
// shape lower_for_range emits) ------------------------------------------------

/// `Some(LoopInfo)` iff `h` is a `for i in lo..hi` header: a `Branch` on
/// `IntLt/IntLeq(load slot, bound)`, entered by exactly a preheader + a
/// pure-increment latch (`load slot; 1; add; store slot; goto h`). The `slot`
/// cell must be `loop_binding` -- desugar provenance that only `lower_for_range`
/// sets, so a `while i < n { ..; i = i + 1 }` (manual counter) is NOT mistaken
/// for a range-for and takes the general path instead.
fn match_counted_loop(f: &MirFunc, h: Idx<Block>) -> Option<LoopInfo> {
    let Term::Branch { cond, then_bb, else_bb } = f.blocks[h].term else {
        return None;
    };
    let (slot, bound, inclusive) = match &f.insts[cond] {
        Inst::IntLt(a, b) => (load_cell(f, *a)?, *b, false),
        Inst::IntLeq(a, b) => (load_cell(f, *a)?, *b, true),
        _ => return None,
    };
    if !f.cells[slot].loop_binding {
        return None;
    }
    // Find the pure-increment latch among h's goto-preds.
    let mut latch = None;
    for (bid, blk) in f.blocks.iter() {
        if blk.term == Term::Goto(h) && is_incr_latch(f, bid, slot) {
            latch = Some(bid);
        }
    }
    let latch = latch?;
    Some(LoopInfo {
        slot,
        bound,
        inclusive,
        body_entry: then_bb,
        latch,
        exit: else_bb,
    })
}

fn load_cell(f: &MirFunc, iid: Idx<Inst>) -> Option<Idx<Cell>> {
    match f.insts[iid] {
        Inst::Load(c) => Some(c),
        _ => None,
    }
}

/// `p`: `vj = load slot; one = 1; vinc = vj + one; store slot = vinc; goto h`.
fn is_incr_latch(f: &MirFunc, p: Idx<Block>, slot: Idx<Cell>) -> bool {
    let ins = &f.blocks[p].insts;
    // The store to slot must be an increment of slot by 1.
    for &iid in ins {
        if let Inst::Store { cell, value } = f.insts[iid] {
            if cell == slot {
                if let Inst::IntAdd(a, b) = f.insts[value] {
                    let is_one = |x: Idx<Inst>| matches!(f.insts[x], Inst::Const(Lit::Int(1)));
                    let loads_slot = |x: Idx<Inst>| matches!(f.insts[x], Inst::Load(c) if c == slot);
                    return (loads_slot(a) && is_one(b)) || (loads_slot(b) && is_one(a));
                }
                return false;
            }
        }
    }
    false
}

fn reverse_postorder(f: &MirFunc, entry: Idx<Block>) -> Vec<Idx<Block>> {
    let mut post = Vec::new();
    let mut visited: HashSet<Idx<Block>> = HashSet::default();
    let mut stack = vec![(entry, false)];
    visited.insert(entry);
    while let Some((b, processed)) = stack.pop() {
        if processed {
            post.push(b);
            continue;
        }
        stack.push((b, true));
        successors(&f.blocks[b].term, |s| {
            if visited.insert(s) {
                stack.push((s, false));
            }
        });
    }
    post.reverse();
    post
}

pub(crate) fn successors(term: &Term, mut f: impl FnMut(Idx<Block>)) {
    match term {
        Term::Goto(t) => f(*t),
        Term::Branch { then_bb, else_bb, .. } => {
            f(*then_bb);
            f(*else_bb);
        }
        Term::Switch { cases, default, .. } => {
            for (_, b) in cases {
                f(*b);
            }
            f(*default);
        }
        Term::Yield { resume, .. } => f(*resume),
        Term::Return(_) | Term::Unreachable => {}
    }
}

// --- public API --------------------------------------------------------------

/// Resolve a root fn name to a unique [`FuncRef`], preferring the user module.
fn resolve_root(ws: &Workspace, user_mid: ModuleId, name: &str) -> Result<FuncRef, CompilerError> {
    let key = Ustr::from(name);
    let Some(list): Option<&FnList> = ws.functions_by_name.get(&key) else {
        return Err(CompilerError::new_no_location(format!("shader root `{name}` not found")));
    };
    let in_user: Vec<_> = list.iter().filter(|(m, _)| *m == user_mid).collect();
    let pick = if in_user.len() == 1 {
        *in_user[0]
    } else if list.len() == 1 {
        list[0]
    } else {
        return Err(CompilerError::new_no_location(format!(
            "shader root `{name}` is overloaded/ambiguous ({} definitions); route B roots must be uniquely named",
            list.len()
        )));
    };
    Ok(FuncRef { module: pick.0, func: pick.1 })
}

/// The GLSL helper functions for tier-0 semantics GLSL's builtins get wrong:
/// floored int division/modulo (GLSL's are undefined on negative operands).
/// Emitted only when reachable code uses `IntDiv`/`IntMod`.
fn helper_prelude(uses_idiv: bool, uses_imod: bool) -> String {
    let mut s = String::new();
    if uses_idiv {
        s.push_str(
            "int __rock_idiv(int a, int b) {\n  return int(floor(float(a) / float(b)));\n}\n",
        );
    }
    if uses_imod {
        s.push_str(
            "int __rock_imod(int a, int b) {\n  return a - b * int(floor(float(a) / float(b)));\n}\n",
        );
    }
    s
}

/// Emit a GLSL function library for `roots` (+ their reachable tier-0 callees),
/// topologically ordered (callees before callers). `mid` is the user module the
/// roots are looked up in first. Errors (strict, spanned) on any profile violation.
pub fn emit_shader_library(
    ws: &Workspace,
    mid: ModuleId,
    roots: &[String],
) -> Result<String, CompilerError> {
    let scx = ScalarCtx::new_values_first_class(ws);

    let mut reach = Reach {
        ws,
        scx: &scx,
        bodies: HashMap::default(),
        gray: HashSet::default(),
        black: HashSet::default(),
        order: Vec::new(),
    };
    for r in roots {
        let fref = resolve_root(ws, mid, r)?;
        reach.visit(fref)?;
    }

    // Detect which floored-int helpers are needed.
    let (mut uses_idiv, mut uses_imod) = (false, false);
    for mf in reach.bodies.values() {
        for (_, inst) in mf.insts.iter() {
            match inst {
                Inst::IntDiv(..) => uses_idiv = true,
                Inst::IntMod(..) => uses_imod = true,
                _ => {}
            }
        }
    }

    let mut out = String::new();
    out.push_str(&helper_prelude(uses_idiv, uses_imod));
    if !out.is_empty() {
        out.push('\n');
    }
    for (i, &fref) in reach.order.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mf = &reach.bodies[&fref];
        out.push_str(&emit_fn_def(ws, fref, mf)?);
    }
    Ok(out)
}

// --- A1: fragment-entry compilation (docs/shaders.md) --------------------

/// A classified shader entry signature (A1 fragment, or an A2 pipeline stage).
struct EntryShape {
    /// Param indices whose field reads render as bare GLSL globals and which are
    /// dropped from the emitted signature: the A1 uniform struct, or a pipeline
    /// stage's attribute / uniform / varyings structs.
    global_params: Vec<usize>,
    /// Combined global field name -> GLSL type (for `ty_of` on bare-global
    /// reads). Names are collision-checked unique across all global structs.
    uniform_fields: Vec<(Ustr, GlslTy)>,
    /// Sampler params: (param index, name). Also dropped, read as bare globals.
    samplers: Vec<(usize, Ustr, GlslTy)>,
    /// `UniformArray<T>` param indices. Dropped from the signature and referenced
    /// by name (via `at()` -> indexing); NOT tier-0-typed, so they are skipped
    /// when typing param cells.
    arrays: Vec<usize>,
    /// `void main()` mode: presence switches emission to `void main()` with a
    /// return-struct destructure (vertex varyings, or MRT fragment outputs).
    main_out: Option<MainOut>,
}

/// The default fullscreen vertex stage -- identical to the hand-written
/// vignette/heat_shimmer VERTEX blocks (the loader compiles the one file twice
/// with `#define VERTEX` / `#define PIXEL`, `stone-rs/src/shader.rs`).
const FULLSCREEN_VERTEX: &str = "layout (location = 0) in vec2 aPos;
layout (location = 1) in vec2 aTexCoord;

out vec2 TexCoord;

void main() {
    gl_Position = vec4(aPos, 0.0, 1.0);
    TexCoord = aTexCoord;
}
";

/// The nominal name behind a declared type, if any (`ShimmerU`, `Sampler2D`).
fn nominal_name(ws: &Workspace, ty: TyId) -> Option<Ustr> {
    match ty_expr(ws, ty)? {
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => Some(n),
        _ => None,
    }
}

/// Classify an entry fn's params against the A1 contract: at most one uniform
/// value struct (every field tier 0), any number of `Sampler2D`, exactly one
/// `Vec2` (the fullscreen TexCoord varying); return `Vec4`.
fn classify_entry(ws: &Workspace, f: &Function) -> Result<EntryShape, CompilerError> {
    let span = f.name_span;
    let mut uniform_param = None;
    let mut uniform_fields: Vec<(Ustr, GlslTy)> = Vec::new();
    let mut samplers: Vec<(usize, Ustr, GlslTy)> = Vec::new();
    let mut tc = None;
    for (i, pd) in f.param_decls.iter().enumerate() {
        match glsl_ty(ws, pd.ty, span, "parameter") {
            Ok(t) if t.is_sampler() => samplers.push((i, pd.name, t)),
            Ok(GlslTy::Vec2) => {
                if tc.is_some() {
                    return Err(err(span, "shader entry: exactly one Vec2 param (the fullscreen TexCoord varying) is allowed"));
                }
                tc = Some(i);
            }
            Ok(other) => {
                return Err(err(span, format!(
                    "shader entry: param `{}` ({}) is not allowed -- A1 uniforms go in one uniform struct param, plus Sampler2D params and one Vec2",
                    pd.name, other.name()
                )));
            }
            Err(e) => {
                // Not a tier-0 leaf: a value struct here is the uniform struct,
                // which must carry @uniform (one rule everywhere with the A2
                // pipeline -- attributes/uniforms are never silently inferred).
                let Some(sname) = nominal_name(ws, pd.ty) else { return Err(e) };
                let Some(&sid) = ws.struct_by_name.get(&sname) else { return Err(e) };
                if !has_anno(&pd.annotations, "uniform") {
                    return Err(err(span, format!(
                        "shader entry: value-struct param `{}` needs @uniform (the uniform block marker)",
                        pd.name
                    )));
                }
                if uniform_param.is_some() {
                    return Err(err(span, "shader entry: at most one uniform struct param is allowed"));
                }
                let sd = &ws.modules[sid.module.index()].structs[sid.def];
                for fd in &sd.fields {
                    let fty = glsl_ty(ws, fd.ty, span, &format!("uniform field `{}`", fd.name))?;
                    if fty.is_sampler() {
                        return Err(err(span, format!("shader entry: uniform field `{}` cannot be a sampler -- pass samplers as fn params", fd.name)));
                    }
                    uniform_fields.push((fd.name, fty));
                }
                uniform_param = Some(i);
            }
        }
    }
    if tc.is_none() {
        return Err(err(span, "shader entry: a Vec2 param (the fullscreen TexCoord varying) is required"));
    }
    match f.return_ty.map(|rt| glsl_ty(ws, rt, span, "return")) {
        Some(Ok(GlslTy::Vec4)) => {}
        _ => return Err(err(span, "shader entry: the return type must be Vec4 (FragColor)")),
    }
    // Uniform names are GLSL globals: fields + samplers + the reserved
    // TexCoord/FragColor must be pairwise distinct.
    let mut seen: HashSet<Ustr> = [Ustr::from("TexCoord"), Ustr::from("FragColor")].into_iter().collect();
    for name in uniform_fields.iter().map(|&(n, _)| n).chain(samplers.iter().map(|&(_, n, _)| n)) {
        if !seen.insert(name) {
            return Err(err(span, format!("shader entry: uniform name `{name}` would collide (fields, samplers, TexCoord and FragColor share the GLSL global namespace)")));
        }
    }
    Ok(EntryShape {
        global_params: uniform_param.into_iter().collect(),
        uniform_fields,
        samplers,
        arrays: Vec::new(),
        main_out: None,
    })
}

/// Emit a COMPLETE single-file shader for one `@shader(fragment)` root: the
/// generated PIXEL block (declarations + reachable helpers + the entry fn +
/// `main`) and the default fullscreen VERTEX block.
pub fn emit_shader_entry(
    ws: &Workspace,
    mid: ModuleId,
    root: &str,
    profile: GlslProfile,
) -> Result<String, CompilerError> {
    Ok(emit_shader_entry_with_dependencies(ws, mid, root, profile)?.source)
}

pub struct ShaderEmission {
    pub source: String,
    pub dependencies: Vec<FuncRef>,
}

pub fn emit_shader_entry_with_dependencies(
    ws: &Workspace,
    mid: ModuleId,
    root: &str,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let scx = ScalarCtx::new_values_first_class(ws);
    let fref = resolve_root(ws, mid, root)?;
    let f = &ws.module(fref.module).functions[fref.func];

    match shader_stage(f) {
        Some("fragment") => {}
        Some(stage) => {
            return Err(err(f.name_span, format!("shader entry: @shader({stage}) is not supported yet -- only @shader(fragment) (vertex entries are A2)")));
        }
        None => {
            return Err(err(f.name_span, format!("shader entry: `{root}` has no @shader(fragment) annotation")));
        }
    }
    let shape = classify_entry(ws, f)?;

    // The entry's signature is validated by the classification above, not
    // check_signature (the uniform struct is not a tier-0 leaf); its body and
    // callees go through the same strict profile pass as fn-library roots.
    let mf = lower_function(ws, &scx, fref.module, fref.func)
        .map_err(|e| err(f.name_span, format!("shader profile: `{}` cannot lower for GLSL: {}", f.name, e.message)))?;
    let mut callees: Vec<FuncRef> = Vec::new();
    profile_body(f, &mf, &mut callees)?;

    let mut reach = Reach {
        ws,
        scx: &scx,
        bodies: HashMap::default(),
        gray: HashSet::default(),
        black: HashSet::default(),
        order: Vec::new(),
    };
    reach.gray.insert(fref); // a callee calling back into the entry = recursion
    for c in callees {
        reach.visit(c)?;
    }
    reach.gray.remove(&fref);

    reject_stage_violations(ws, Stage::Fragment, reach.bodies.values().chain(std::iter::once(&mf)), f.name_span)?;

    let (mut uses_idiv, mut uses_imod) = (false, false);
    for body in reach.bodies.values().chain(std::iter::once(&mf)) {
        for (_, inst) in body.insts.iter() {
            match inst {
                Inst::IntDiv(..) => uses_idiv = true,
                Inst::IntMod(..) => uses_imod = true,
                _ => {}
            }
        }
    }

    let mut px = String::new();
    px.push_str("in vec2 TexCoord;\nout vec4 FragColor;\n\n");
    for (n, t) in &shape.uniform_fields {
        px.push_str(&format!("uniform {} {};\n", t.name(), n));
    }
    for (_, n, t) in &shape.samplers {
        px.push_str(&format!("uniform {} {};\n", t.uniform_decl(), n));
    }
    px.push('\n');
    let prelude = helper_prelude(uses_idiv, uses_imod);
    if !prelude.is_empty() {
        px.push_str(&prelude);
        px.push('\n');
    }
    for &href in &reach.order {
        px.push_str(&emit_fn_def(ws, href, &reach.bodies[&href])?);
        px.push('\n');
    }
    px.push_str(&emit_fn_def_with(ws, fref, &mf, Some(&shape))?);
    px.push('\n');
    px.push_str(&format!("void main() {{\n  FragColor = {}(TexCoord);\n}}\n", f.name));

    // Both stages carry the profile's precision preamble: ES requires an
    // explicit fragment float precision, and the vertex declarations are
    // harmless no-ops (highp is already the vertex default).
    let (version, precision) = profile_header(profile);
    let source = format!(
        "{version}\n\n#ifdef PIXEL\n\n{precision}{px}\n#endif\n\n#ifdef VERTEX\n\n{precision}{FULLSCREEN_VERTEX}\n#endif\n"
    );
    let mut dependencies = reach.order;
    dependencies.push(fref);
    Ok(ShaderEmission { source, dependencies })
}

/// Emission profile (docs/shaders.md): the `#version` line and
/// any builtin/precision differences. `Core330` is the desktop default; `Es300`
/// is the mobile GLES target. Threaded through every A2 emission path.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum GlslProfile {
    #[default]
    Core330,
    Es300,
}

// --- A2: vertex+fragment pipeline (docs/shaders.md) ---------------------

/// The vertex stage: which shader stage a reachability walk is emitting (drives
/// per-stage builtin gating).
#[derive(Copy, Clone, PartialEq, Eq)]
enum Stage {
    Vertex,
    Fragment,
}

/// A classified `@shader(vertex)` entry.
struct VertexShape {
    /// Attribute struct param indices, in location-assignment order (per-vertex
    /// `@attr` first, then `@instance` -- the numbering continues).
    attr_params: Vec<usize>,
    /// Attribute fields in location order: (name, GLSL type).
    attrs: Vec<(Ustr, GlslTy)>,
    /// `@uniform` struct param indices (at most one).
    uniform_params: Vec<usize>,
    /// Uniform fields, declaration order.
    uniforms: Vec<(Ustr, GlslTy)>,
    /// Sampler params (rare in vertex, but legal).
    samplers: Vec<(usize, Ustr, GlslTy)>,
    /// `UniformArray<T>` params: (index, name, element type, @size capacity).
    arrays: Vec<(usize, Ustr, GlslTy, i64)>,
    /// The varyings value-struct return type.
    varyings_ty: TyId,
}

/// A classified varyings value struct.
struct VaryingsShape {
    /// Varyings EXCLUDING `position`: (name, type, `@flat`).
    varyings: Vec<(Ustr, GlslTy, bool)>,
}

/// A classified pipeline `@shader(fragment)` entry (varyings-consuming form).
struct FragShape {
    /// The varyings-struct param index (always 0 -- the first param).
    varyings_param: usize,
    uniform_params: Vec<usize>,
    uniforms: Vec<(Ustr, GlslTy)>,
    samplers: Vec<(usize, Ustr, GlslTy)>,
    /// `UniformArray<T>` params: (index, name, element type, @size capacity).
    arrays: Vec<(usize, Ustr, GlslTy, i64)>,
    /// MRT outputs: `Some` when the entry returns a value struct of Vec4/int
    /// fields -> one `layout(location=N) out <ty> <field>` per field, in
    /// declaration order. `None` = the single implicit `FragColor` output.
    outputs: Option<Vec<(Ustr, GlslTy)>>,
    /// Single-output type (Vec4, or Int for an R32I-only target). Vec4 when
    /// `outputs` is `Some` (unused there).
    single_out: GlslTy,
}

fn has_anno(annos: &[bone_semantic::hir::Annotation], name: &str) -> bool {
    annos.iter().any(|a| a.name.name.as_str() == name)
}

/// The int argument of an `@name(N)` annotation, if present (arity/type are
/// registry-validated upstream).
fn anno_int(annos: &[bone_semantic::hir::Annotation], name: &str) -> Option<i64> {
    annos
        .iter()
        .find(|a| a.name.name.as_str() == name)
        .and_then(|a| a.args.first())
        .and_then(|arg| match arg.value {
            AnnoValue::Int(n) => Some(n),
            _ => None,
        })
}

/// The element type of a `UniformArray<T>`-typed param, or `None` when the type
/// is not the uniform-array phantom. Elements are the corpus set (decision 3):
/// float, int, Vec2, Vec4.
fn uniform_array_elem(ws: &Workspace, ty: TyId, span: Span) -> Result<Option<GlslTy>, CompilerError> {
    let Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), params }) = ty_expr(ws, ty) else {
        return Ok(None);
    };
    if n.as_str() != "UniformArray" {
        return Ok(None);
    }
    let Some(first) = params.into_iter().next() else {
        return Err(err(span, "shader: UniformArray needs an element type argument"));
    };
    match glsl_ty(ws, ty.with_idx(first), span, "uniform-array element")? {
        t @ (GlslTy::Float | GlslTy::Int | GlslTy::Vec2 | GlslTy::Vec4) => Ok(Some(t)),
        other => Err(err(span, format!(
            "shader: UniformArray element type `{}` is not supported (allowed: float, int, Vec2, Vec4)",
            other.name()
        ))),
    }
}

/// The [`StructDef`] behind a nominal type, if any.
fn struct_def_of(ws: &Workspace, ty: TyId) -> Option<&StructDef> {
    let sname = nominal_name(ws, ty)?;
    let &sid = ws.struct_by_name.get(&sname)?;
    Some(&ws.modules[sid.module.index()].structs[sid.def])
}

/// Attribute/uniform fields of a struct param, GLSL-typed (a sampler-typed field
/// rejects -- samplers are params, not struct fields).
fn struct_gl_fields(ws: &Workspace, ty: TyId, span: Span, what: &str) -> Result<Vec<(Ustr, GlslTy)>, CompilerError> {
    let sd = struct_def_of(ws, ty)
        .ok_or_else(|| err(span, format!("shader: {what} must be a value struct")))?;
    let mut out = Vec::new();
    for fd in &sd.fields {
        let fty = glsl_ty(ws, fd.ty, span, &format!("{what} field `{}`", fd.name))?;
        if fty.is_sampler() {
            return Err(err(span, format!("shader: {what} field `{}` cannot be a sampler -- pass samplers as fn params", fd.name)));
        }
        if fty == GlslTy::Bvec3 {
            return Err(err(span, format!("shader: {what} field `{}` cannot be a bvec3 -- bool-vector {what}s are unsupported (bvec3 is local/temp only)", fd.name)));
        }
        out.push((fd.name, fty));
    }
    Ok(out)
}

/// Classify a `@shader(vertex)` entry: value-struct params carry a role marker
/// (`@attr` / `@instance` / `@uniform`); samplers are type-distinguished; the
/// return type is the varyings struct.
fn classify_vertex(ws: &Workspace, f: &Function) -> Result<VertexShape, CompilerError> {
    let span = f.name_span;
    let (mut per_vertex, mut instanced, mut uniform_params, mut samplers, mut arrays) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (i, pd) in f.param_decls.iter().enumerate() {
        if let Some(elem) = uniform_array_elem(ws, pd.ty, span)? {
            let n = anno_int(&pd.annotations, "size").ok_or_else(|| err(span, format!(
                "shader vertex: uniform-array param `{}` needs @size(N) (the declared GLSL array capacity)",
                pd.name
            )))?;
            arrays.push((i, pd.name, elem, n));
            continue;
        }
        match glsl_ty(ws, pd.ty, span, "parameter") {
            Ok(t) if t.is_sampler() => samplers.push((i, pd.name, t)),
            Ok(other) => {
                return Err(err(span, format!(
                    "shader vertex: param `{}` ({}) must be an @attr/@instance/@uniform value struct or a Sampler2D",
                    pd.name, other.name()
                )));
            }
            Err(_) => match (has_anno(&pd.annotations, "attr"), has_anno(&pd.annotations, "instance"), has_anno(&pd.annotations, "uniform")) {
                (true, false, false) => per_vertex.push(i),
                (false, true, false) => instanced.push(i),
                (false, false, true) => uniform_params.push(i),
                (false, false, false) => {
                    return Err(err(span, format!(
                        "shader vertex: value-struct param `{}` needs a role marker -- @attr (per-vertex attributes), @instance (per-instance attributes), or @uniform (uniform block)",
                        pd.name
                    )));
                }
                _ => return Err(err(span, format!("shader vertex: param `{}` has conflicting role markers", pd.name))),
            },
        }
    }
    if uniform_params.len() > 1 {
        return Err(err(span, "shader vertex: at most one @uniform struct param is allowed"));
    }
    let mut attr_params = Vec::new();
    let mut attrs = Vec::new();
    for &i in per_vertex.iter().chain(instanced.iter()) {
        attr_params.push(i);
        let fields = struct_gl_fields(ws, f.param_decls[i].ty, span, "attribute")?;
        // GLSL forbids `in bool` -- bool is legal only as a uniform.
        if let Some((n, _)) = fields.iter().find(|&&(_, t)| t == GlslTy::Bool) {
            return Err(err(span, format!(
                "shader vertex: attribute `{n}` cannot be bool (GLSL forbids bool vertex inputs) -- use int or float"
            )));
        }
        attrs.extend(fields);
    }
    let mut uniforms = Vec::new();
    for &i in &uniform_params {
        uniforms.extend(struct_gl_fields(ws, f.param_decls[i].ty, span, "uniform")?);
    }
    let varyings_ty = f
        .return_ty
        .ok_or_else(|| err(span, "shader vertex: entry must return a varyings value struct"))?;
    Ok(VertexShape { attr_params, attrs, uniform_params, uniforms, samplers, arrays, varyings_ty })
}

/// Classify the varyings struct: a required `position: Vec4` field (-> gl_Position,
/// suppressed as a varying), plus the other fields as varyings. An int-typed
/// varying without `@flat` rejects (GLSL mandates flat integer interpolation).
fn classify_varyings(ws: &Workspace, ty: TyId, span: Span) -> Result<VaryingsShape, CompilerError> {
    let sd = struct_def_of(ws, ty)
        .ok_or_else(|| err(span, "shader: the varyings type must be a value struct"))?;
    let mut has_position = false;
    let mut varyings = Vec::new();
    for fd in &sd.fields {
        let fty = glsl_ty(ws, fd.ty, span, &format!("varying `{}`", fd.name))?;
        if fty == GlslTy::Bvec3 {
            return Err(err(span, format!("shader: varying `{}` cannot be a bvec3 (GLSL forbids bool-vector interpolants) -- bvec3 is local/temp only", fd.name)));
        }
        if fd.name.as_str() == "position" {
            if fty != GlslTy::Vec4 {
                return Err(err(span, "shader: the varyings `position` field must be Vec4 (it becomes gl_Position)"));
            }
            has_position = true;
            continue;
        }
        let flat = has_anno(&fd.annotations, "flat");
        if fty == GlslTy::Int && !flat {
            return Err(err(span, format!(
                "shader: int-typed varying `{}` needs @flat (GLSL requires flat integer interpolation)",
                fd.name
            )));
        }
        // GLSL forbids bool in/out variables entirely (even flat).
        if fty == GlslTy::Bool {
            return Err(err(span, format!(
                "shader: varying `{}` cannot be bool (GLSL forbids bool varyings) -- use float or int + @flat",
                fd.name
            )));
        }
        varyings.push((fd.name, fty, flat));
    }
    if !has_position {
        return Err(err(span, "shader: the varyings struct must have a `position: Vec4` field (it becomes gl_Position)"));
    }
    Ok(VaryingsShape { varyings })
}

/// Classify a pipeline `@shader(fragment)` entry: the first param is the varyings
/// struct (its type must equal the vertex entry's return type); the rest are
/// `@uniform` structs and samplers.
fn classify_pipeline_fragment(ws: &Workspace, f: &Function, vert_varyings_ty: TyId) -> Result<FragShape, CompilerError> {
    let span = f.name_span;
    let Some(first) = f.param_decls.first() else {
        return Err(err(span, "shader fragment: the entry must take the varyings struct as its first param"));
    };
    if nominal_name(ws, first.ty).is_none() || nominal_name(ws, first.ty) != nominal_name(ws, vert_varyings_ty) {
        return Err(err(span, "shader fragment: the first param must be the varyings struct (the vertex entry's return type) -- the pair's varyings types do not match"));
    }
    let (mut uniform_params, mut uniforms, mut samplers, mut arrays) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (i, pd) in f.param_decls.iter().enumerate().skip(1) {
        if let Some(elem) = uniform_array_elem(ws, pd.ty, span)? {
            let n = anno_int(&pd.annotations, "size").ok_or_else(|| err(span, format!(
                "shader fragment: uniform-array param `{}` needs @size(N) (the declared GLSL array capacity)",
                pd.name
            )))?;
            arrays.push((i, pd.name, elem, n));
            continue;
        }
        match glsl_ty(ws, pd.ty, span, "parameter") {
            Ok(t) if t.is_sampler() => samplers.push((i, pd.name, t)),
            Ok(other) => {
                return Err(err(span, format!("shader fragment: param `{}` ({}) must be @uniform or a Sampler2D", pd.name, other.name())));
            }
            Err(_) => {
                if !has_anno(&pd.annotations, "uniform") {
                    return Err(err(span, format!("shader fragment: value-struct param `{}` needs @uniform", pd.name)));
                }
                uniforms.extend(struct_gl_fields(ws, pd.ty, span, "uniform")?);
                uniform_params.push(i);
            }
        }
    }
    if uniform_params.len() > 1 {
        return Err(err(span, "shader fragment: at most one @uniform struct param is allowed"));
    }
    // Return: a single Vec4 (implicit FragColor), a single int (an R32I-only
    // target, e.g. the silhouette mask), OR a value struct of Vec4/int fields
    // (MRT -- one `layout(location=N) out <ty> <field>` per field, in order;
    // int fields feed R32I attachments).
    let (outputs, single_out) = match f.return_ty.map(|rt| (rt, glsl_ty(ws, rt, span, "return"))) {
        Some((_, Ok(GlslTy::Vec4))) => (None, GlslTy::Vec4),
        Some((_, Ok(GlslTy::Int))) => (None, GlslTy::Int),
        Some((rt, Err(_))) => {
            let sd = struct_def_of(ws, rt).ok_or_else(|| {
                err(span, "shader fragment: the return type must be Vec4 (FragColor), int (an integer id target), or a value struct of Vec4/int fields (MRT outputs)")
            })?;
            let mut outs = Vec::new();
            for fd in &sd.fields {
                let fty = glsl_ty(ws, fd.ty, span, &format!("MRT output `{}`", fd.name))?;
                if fty != GlslTy::Vec4 && fty != GlslTy::Int {
                    return Err(err(span, format!(
                        "shader fragment: MRT output `{}` ({}) must be Vec4 or int (each output is a `layout(location=N) out <ty>`)",
                        fd.name, fty.name()
                    )));
                }
                outs.push((fd.name, fty));
            }
            if outs.is_empty() {
                return Err(err(span, "shader fragment: the MRT output struct must have at least one field"));
            }
            (Some(outs), GlslTy::Vec4)
        }
        _ => return Err(err(span, "shader fragment: the return type must be Vec4 (FragColor), int (an integer id target), or a value struct of Vec4/int fields (MRT outputs)")),
    };
    Ok(FragShape { varyings_param: 0, uniform_params, uniforms, samplers, arrays, outputs, single_out })
}

/// Reject GPU builtins reached from the wrong stage: frag_coord/fwidth/discard
/// are fragment-only, vertex_id is vertex-only.
fn reject_stage_violations<'a>(
    ws: &Workspace,
    stage: Stage,
    bodies: impl Iterator<Item = &'a MirFunc>,
    span: Span,
) -> Result<(), CompilerError> {
    for mf in bodies {
        for (_, inst) in mf.insts.iter() {
            if let Inst::Call { func, .. } = inst {
                let n = ws.module(func.module).functions[func.func].name;
                match (stage, n.as_str()) {
                    (Stage::Vertex, "fwidth" | "frag_coord" | "discard") => {
                        return Err(err(span, format!("shader vertex: `{n}()` is a fragment-stage builtin, not available in the vertex stage")));
                    }
                    (Stage::Fragment, "vertex_id") => {
                        return Err(err(span, format!("shader fragment: `{n}()` is a vertex-stage builtin, not available in the fragment stage")));
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Lower a shader entry, profile it (strict tier-0 + per-stage builtin gating),
/// and emit its reachable helper fns + the floored-int prelude. Returns the
/// entry's own MIR plus the concatenated helper GLSL.
fn lower_reach_helpers(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
    stage: Stage,
) -> Result<(MirFunc, String, Vec<FuncRef>), CompilerError> {
    let f = &ws.module(fref.module).functions[fref.func];
    let mf = lower_function(ws, scx, fref.module, fref.func)
        .map_err(|e| err(f.name_span, format!("shader profile: `{}` cannot lower for GLSL: {}", f.name, e.message)))?;
    let mut callees: Vec<FuncRef> = Vec::new();
    profile_body(f, &mf, &mut callees)?;

    let mut reach = Reach {
        ws,
        scx,
        bodies: HashMap::default(),
        gray: HashSet::default(),
        black: HashSet::default(),
        order: Vec::new(),
    };
    reach.gray.insert(fref);
    for c in callees {
        reach.visit(c)?;
    }
    reach.gray.remove(&fref);

    reject_stage_violations(ws, stage, reach.bodies.values().chain(std::iter::once(&mf)), f.name_span)?;

    let (mut uses_idiv, mut uses_imod) = (false, false);
    for body in reach.bodies.values().chain(std::iter::once(&mf)) {
        for (_, inst) in body.insts.iter() {
            match inst {
                Inst::IntDiv(..) => uses_idiv = true,
                Inst::IntMod(..) => uses_imod = true,
                _ => {}
            }
        }
    }
    let mut helpers = String::new();
    let prelude = helper_prelude(uses_idiv, uses_imod);
    if !prelude.is_empty() {
        helpers.push_str(&prelude);
        helpers.push('\n');
    }
    for &href in &reach.order {
        helpers.push_str(&emit_fn_def(ws, href, &reach.bodies[&href])?);
        helpers.push('\n');
    }
    let mut dependencies = reach.order;
    dependencies.push(fref);
    Ok((mf, helpers, dependencies))
}

/// Reject a fragment-stage read of the varyings `position` field: it is
/// gl_Position (vertex-only), not a fragment input.
fn reject_position_read(ws: &Workspace, mf: &MirFunc, varyings_cell: Idx<Cell>, span: Span) -> Result<(), CompilerError> {
    let _ = ws;
    for (_, inst) in mf.insts.iter() {
        if let Inst::Field { base, field } = inst {
            if field.as_str() == "position" && matches!(mf.insts[*base], Inst::Load(c) if c == varyings_cell) {
                return Err(err(span, "shader fragment: `position` is gl_Position (vertex-only) and is not emitted as a varying -- it cannot be read in the fragment stage"));
            }
        }
    }
    Ok(())
}

fn emit_vertex_stage(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
    v: &VertexShape,
    varyings: &VaryingsShape,
) -> Result<(String, Vec<FuncRef>), CompilerError> {
    let shape = EntryShape {
        global_params: v.attr_params.iter().copied().chain(v.uniform_params.iter().copied()).collect(),
        uniform_fields: v.attrs.iter().copied().chain(v.uniforms.iter().copied()).collect(),
        samplers: v.samplers.clone(),
        arrays: v.arrays.iter().map(|&(i, ..)| i).collect(),
        main_out: Some(MainOut {
            fields: varyings.varyings.iter().map(|&(n, t, _)| (n, t)).collect(),
            position_is_gl: true,
        }),
    };
    let (mf, helpers, dependencies) = lower_reach_helpers(ws, scx, fref, Stage::Vertex)?;

    let mut out = String::new();
    for (loc, (n, t)) in v.attrs.iter().enumerate() {
        out.push_str(&format!("layout (location = {loc}) in {} {};\n", t.name(), n));
    }
    out.push('\n');
    if !v.uniforms.is_empty() || !v.arrays.is_empty() {
        for (n, t) in &v.uniforms {
            out.push_str(&format!("uniform {} {};\n", t.name(), n));
        }
        for (_, n, t, cap) in &v.arrays {
            out.push_str(&format!("uniform {} {}[{cap}];\n", t.name(), n));
        }
        out.push('\n');
    }
    for (n, t, flat) in &varyings.varyings {
        let q = if *flat { "flat " } else { "" };
        out.push_str(&format!("{q}out {} {};\n", t.name(), n));
    }
    out.push('\n');
    out.push_str(&helpers);
    out.push_str(&emit_fn_def_with(ws, fref, &mf, Some(&shape))?);
    Ok((out, dependencies))
}

fn emit_fragment_stage(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
    fs: &FragShape,
    varyings: &VaryingsShape,
) -> Result<(String, Vec<FuncRef>), CompilerError> {
    let f = &ws.module(fref.module).functions[fref.func];
    // MRT: the entry is emitted as `void main()` destructuring its returned
    // output struct into the `layout(location=N) out` globals (like the vertex
    // varyings path). Single-output keeps the `vec4 <fn>()` + wrapper form.
    let main_out = fs.outputs.as_ref().map(|outs| MainOut {
        fields: outs.iter().copied().collect(),
        position_is_gl: false,
    });
    let shape = EntryShape {
        global_params: std::iter::once(fs.varyings_param).chain(fs.uniform_params.iter().copied()).collect(),
        uniform_fields: varyings.varyings.iter().map(|&(n, t, _)| (n, t)).chain(fs.uniforms.iter().copied()).collect(),
        samplers: fs.samplers.clone(),
        arrays: fs.arrays.iter().map(|&(i, ..)| i).collect(),
        main_out,
    };
    let (mf, helpers, dependencies) = lower_reach_helpers(ws, scx, fref, Stage::Fragment)?;
    reject_position_read(ws, &mf, mf.params[fs.varyings_param], f.name_span)?;

    let mut out = String::new();
    for (n, t, flat) in &varyings.varyings {
        let q = if *flat { "flat " } else { "" };
        out.push_str(&format!("{q}in {} {};\n", t.name(), n));
    }
    match &fs.outputs {
        None => out.push_str(&format!("out {} FragColor;\n\n", fs.single_out.name())),
        Some(outs) => {
            for (loc, (n, t)) in outs.iter().enumerate() {
                out.push_str(&format!("layout(location = {loc}) out {} {n};\n", t.name()));
            }
            out.push('\n');
        }
    }
    for (n, t) in &fs.uniforms {
        out.push_str(&format!("uniform {} {};\n", t.name(), n));
    }
    for (_, n, t, cap) in &fs.arrays {
        out.push_str(&format!("uniform {} {}[{cap}];\n", t.name(), n));
    }
    for (_, n, t) in &fs.samplers {
        out.push_str(&format!("uniform {} {};\n", t.uniform_decl(), n));
    }
    out.push('\n');
    out.push_str(&helpers);
    out.push_str(&emit_fn_def_with(ws, fref, &mf, Some(&shape))?);
    // Single-output: the entry fn returns Vec4; main() forwards it to FragColor.
    // MRT: the entry IS `void main()` (destructured), so no wrapper.
    if fs.outputs.is_none() {
        out.push('\n');
        out.push_str(&format!("void main() {{\n  FragColor = {}();\n}}\n", f.name));
    }
    Ok((out, dependencies))
}

/// Reject duplicate global names across BOTH stages (decision 7): attributes,
/// uniforms, varyings, samplers and the reserved gl_Position/FragColor share
/// one GLSL namespace -- the linked program has a single uniform namespace, so
/// a vertex/fragment same-name pair is a link error (or a silent alias), never
/// two distinct bindings. Varyings are one name in both stages (inserted once).
fn check_pipeline_collisions(v: &VertexShape, varyings: &VaryingsShape, fs: &FragShape, span: Span) -> Result<(), CompilerError> {
    // gl_Position is always reserved. `FragColor` is reserved only for the
    // single-output form -- with MRT the outputs are explicit named fields (one
    // of which may itself be `FragColor`), so they go through the collision check
    // like any other name rather than being pre-seeded.
    let mut seen: HashSet<Ustr> = [Ustr::from("gl_Position")].into_iter().collect();
    if fs.outputs.is_none() {
        seen.insert(Ustr::from("FragColor"));
    }
    let mrt_outputs = fs.outputs.iter().flatten().map(|&(n, _)| n);
    let names = v.attrs.iter().map(|&(n, _)| n)
        .chain(v.uniforms.iter().map(|&(n, _)| n))
        .chain(v.samplers.iter().map(|&(_, n, _)| n))
        .chain(v.arrays.iter().map(|&(_, n, ..)| n))
        .chain(varyings.varyings.iter().map(|&(n, _, _)| n))
        .chain(fs.uniforms.iter().map(|&(n, _)| n))
        .chain(fs.samplers.iter().map(|&(_, n, _)| n))
        .chain(fs.arrays.iter().map(|&(_, n, ..)| n))
        .chain(mrt_outputs);
    for n in names {
        if !seen.insert(n) {
            return Err(err(span, format!("shader pipeline: name `{n}` would collide (attributes, uniforms, varyings, samplers and reserved names share one GLSL namespace across both stages)")));
        }
    }
    Ok(())
}

/// A2: compile a `@shader(vertex)` + `@shader(fragment)` pair into one
/// `#ifdef VERTEX`/`#ifdef PIXEL` file (docs/shaders.md). The vertex entry's
/// return type (the varyings value struct) must be exactly the fragment entry's
/// first param type. Vertex params are classified by annotation: `@attr` (per-
/// vertex attribute struct), `@instance` (per-instance attribute divisor), and
/// `@uniform` (uniform block); the varyings `position: Vec4` field drives
/// `gl_Position` and is suppressed as a varying.
pub fn emit_shader_pipeline(
    ws: &Workspace,
    mid: ModuleId,
    vert_root: &str,
    frag_root: &str,
    profile: GlslProfile,
) -> Result<String, CompilerError> {
    Ok(emit_shader_pipeline_with_dependencies(
        ws, mid, vert_root, frag_root, profile,
    )?
    .source)
}

pub fn emit_shader_pipeline_with_dependencies(
    ws: &Workspace,
    mid: ModuleId,
    vert_root: &str,
    frag_root: &str,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let scx = ScalarCtx::new_values_first_class(ws);
    let vfref = resolve_root(ws, mid, vert_root)?;
    let ffref = resolve_root(ws, mid, frag_root)?;
    let vf = &ws.module(vfref.module).functions[vfref.func];
    let ff = &ws.module(ffref.module).functions[ffref.func];
    if shader_stage(vf) != Some("vertex") {
        return Err(err(vf.name_span, format!("shader pipeline: the vertex root `{vert_root}` must carry @shader(vertex)")));
    }
    if shader_stage(ff) != Some("fragment") {
        return Err(err(ff.name_span, format!("shader pipeline: the fragment root `{frag_root}` must carry @shader(fragment)")));
    }

    let vshape = classify_vertex(ws, vf)?;
    let varyings = classify_varyings(ws, vshape.varyings_ty, vf.name_span)?;
    let fshape = classify_pipeline_fragment(ws, ff, vshape.varyings_ty)?;
    check_pipeline_collisions(&vshape, &varyings, &fshape, vf.name_span)?;

    let (vertex_block, vertex_dependencies) =
        emit_vertex_stage(ws, &scx, vfref, &vshape, &varyings)?;
    let (pixel_block, pixel_dependencies) =
        emit_fragment_stage(ws, &scx, ffref, &fshape, &varyings)?;

    let (version, precision) = profile_header(profile);
    let source = format!(
        "{version}\n\n#ifdef VERTEX\n\n{precision}{vertex_block}\n#endif\n\n#ifdef PIXEL\n\n{precision}{pixel_block}\n#endif\n"
    );
    let dependencies = vertex_dependencies
        .into_iter()
        .chain(pixel_dependencies)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Ok(ShaderEmission { source, dependencies })
}

/// CLI entry point: dispatch on the root's `@shader` annotation -- an annotated
/// root compiles as a whole-shader fragment entry (exactly one root), plain
/// roots as a fn library.
pub fn emit_shader(
    ws: &Workspace,
    mid: ModuleId,
    roots: &[String],
    profile: GlslProfile,
) -> Result<String, CompilerError> {
    let annotated = roots
        .iter()
        .filter(|r| {
            resolve_root(ws, mid, r)
                .ok()
                .is_some_and(|fref| shader_stage(&ws.module(fref.module).functions[fref.func]).is_some())
        })
        .count();
    match annotated {
        0 => emit_shader_library(ws, mid, roots),
        1 if roots.len() == 1 => emit_shader_entry(ws, mid, &roots[0], profile),
        _ => Err(err(
            Span::default(),
            "rock shader: a @shader(fragment) entry compiles alone -- pass exactly one root fn",
        )),
    }
}

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

/// A complete GLSL shader pair driving one `@test(returns = ...)` value test
/// (`docs/multi-backend-testing-plan.md` G1). The fragment shader wraps the
/// emitted root fn and writes its scalar result to `layout(location = 0)`; the
/// vertex shader draws a fullscreen triangle (no vertex attributes). Both carry
/// the profile's `#version` line so the GL runner compiles them as one program.
#[derive(Debug)]
pub struct ValueTestShaders {
    pub vertex: String,
    pub fragment: String,
    pub ret: ValueTestRet,
}

/// The profile's `#version` line plus the fragment-stage precision preamble
/// (ES needs an explicit float/int precision; core does not).
fn profile_header(profile: GlslProfile) -> (&'static str, &'static str) {
    match profile {
        GlslProfile::Core330 => ("#version 330 core", ""),
        GlslProfile::Es300 => ("#version 300 es", "precision highp float;\nprecision highp int;\n"),
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
    let ret = match f.return_ty.map(|rt| glsl_ty(ws, rt, f.name_span, "return")) {
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
            ))
        }
        _ => {
            return Err(err(
                f.name_span,
                "value test: the root fn must return int/float/bool",
            ))
        }
    };

    // The fn library runs the strict tier-0 profile pass; a body outside the
    // subset rejects here (the SKIP reason -- the A1 fn-emission work list).
    let defs = emit_shader_library(ws, mid, std::slice::from_ref(&root.to_string()))?;
    let call = mangle_fn(ws, f);

    let (version, precision) = profile_header(profile);
    let (out_ty, out_expr) = match ret {
        ValueTestRet::Int => ("int", format!("{call}()")),
        ValueTestRet::Float => ("float", format!("{call}()")),
        ValueTestRet::Bool => ("int", format!("({call}() ? 1 : 0)")),
    };
    let fragment = format!(
        "{version}\n{precision}\n{defs}\nlayout(location = 0) out {out_ty} _rock_value_out;\n\
         void main() {{\n  _rock_value_out = {out_expr};\n}}\n"
    );
    // A fullscreen triangle from gl_VertexID -- no vertex buffers/attributes, so
    // the runner binds only an empty VAO. gl_VertexID is core since 3.0 / ES 3.0.
    let vertex = format!(
        "{version}\nvoid main() {{\n  \
         vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n  \
         gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n}}\n"
    );
    Ok(ValueTestShaders { vertex, fragment, ret })
}

/// True iff `root` resolves to a `@shader(vertex)` fn -- the CLI routes to the
/// two-root vertex+fragment pipeline form when any root is a vertex entry.
pub fn is_vertex_shader_root(ws: &Workspace, mid: ModuleId, root: &str) -> bool {
    resolve_root(ws, mid, root)
        .ok()
        .is_some_and(|fref| shader_stage(&ws.module(fref.module).functions[fref.func]) == Some("vertex"))
}

/// True iff `root` resolves to a `@shader(fragment)` fn -- the CLI uses this to
/// admit `--profile` for the single-root fullscreen form while still rejecting
/// it for the route-B fn library (which emits no `#version` line to switch).
pub fn is_fragment_shader_root(ws: &Workspace, mid: ModuleId, root: &str) -> bool {
    resolve_root(ws, mid, root)
        .ok()
        .is_some_and(|fref| shader_stage(&ws.module(fref.module).functions[fref.func]) == Some("fragment"))
}

/// The `@shader(<stage>)` annotation's stage argument, if the fn carries one.
pub fn shader_stage(f: &Function) -> Option<&'static str> {
    let ann = f.annotations.iter().find(|a| a.name.name.as_str() == "shader")?;
    match ann.args.first().map(|a| &a.value) {
        Some(AnnoValue::Ident(id)) if id.name.as_str() == "fragment" => Some("fragment"),
        Some(AnnoValue::Ident(id)) if id.name.as_str() == "vertex" => Some("vertex"),
        _ => Some("<unknown>"),
    }
}

#[cfg(test)]
mod assertion_flip_tests {
    use super::*;
    use ustr::Ustr;

    fn build_ws(src: &str) -> (Workspace, ModuleId) {
        let mut modules: Vec<skull_parser::Ast> = rock_host::stdlib_loader::stdlib_files()
            .iter()
            .map(|f| {
                let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
                skull_parser::parse_module(f.contents, id).expect("stdlib parse")
            })
            .collect();
        modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
        let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("mid");
        (ws, mid)
    }

    /// The unfuck's flipped assertion: a MISSED int->float coercion sink reaches
    /// the emitter as a raw int-typed value in a float slot and must be a HARD
    /// emit error, never silent driver-dependent GLSL. Here we lower
    /// `f(i: int) -> float { return i }` (whose return sink DOES coerce) and then
    /// strip the `IntToFloat` to simulate a missed sink.
    #[test]
    fn missed_sink_is_a_hard_emit_error() {
        let (ws, mid) = build_ws("@pure fn f(i: int) -> float { return i }");
        let scx = ScalarCtx::new_values_first_class(&ws);
        let fref = resolve_root(&ws, mid, "f").expect("root");
        let mut mf = lower_function(&ws, &scx, fref.module, fref.func).expect("lower");

        // The coercion is present today (regression pin for the return sink).
        assert!(
            mf.insts.iter().any(|(_, i)| matches!(i, Inst::IntToFloat(_))),
            "expected an IntToFloat coercion at the return sink"
        );

        // Strip it: rewrite each `return` to the raw int operand.
        let bids: Vec<_> = mf.blocks.iter().map(|(b, _)| b).collect();
        for bid in bids {
            if let Term::Return(vs) = mf.blocks[bid].term.clone() {
                let stripped: Vec<_> = vs
                    .iter()
                    .map(|&v| match mf.insts[v] {
                        Inst::IntToFloat(x) => x,
                        _ => v,
                    })
                    .collect();
                mf.set_term(bid, Term::Return(stripped));
            }
        }

        let r = emit_fn_def(&ws, fref, &mf);
        let msg = r.expect_err("a missed int->float sink must be a hard emit error").message;
        assert!(msg.contains("missed lowering sink"), "unexpected error: {msg}");
    }
}

#[cfg(test)]
mod value_test_shader {
    use super::*;
    use ustr::Ustr;

    fn build_ws(src: &str) -> (Workspace, ModuleId) {
        let mut modules: Vec<skull_parser::Ast> = rock_host::stdlib_loader::stdlib_files()
            .iter()
            .map(|f| {
                let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
                skull_parser::parse_module(f.contents, id).expect("stdlib parse")
            })
            .collect();
        modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
        let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("mid");
        (ws, mid)
    }

    #[test]
    fn int_return_wraps_root_and_writes_int_output() {
        let (ws, mid) = build_ws("fn t() -> int { return min(3, 5) }");
        let s = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect("emit");
        assert_eq!(s.ret, ValueTestRet::Int);
        assert!(s.fragment.starts_with("#version 330 core\n"), "{}", s.fragment);
        assert!(s.fragment.contains("layout(location = 0) out int _rock_value_out;"), "{}", s.fragment);
        assert!(s.fragment.contains("_rock_value_out = t_ret_int();"), "{}", s.fragment);
        assert!(s.fragment.contains("int t_ret_int()"), "callee def present: {}", s.fragment);
        assert!(s.vertex.contains("gl_VertexID"), "{}", s.vertex);
    }

    #[test]
    fn float_return_uses_r32f_shape() {
        let (ws, mid) = build_ws("fn t() -> float { return clamp(9.0, 0.0, 5.0) }");
        let s = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect("emit");
        assert_eq!(s.ret, ValueTestRet::Float);
        assert!(s.fragment.contains("out float _rock_value_out;"), "{}", s.fragment);
        assert!(s.fragment.contains("_rock_value_out = t_ret_float();"), "{}", s.fragment);
    }

    #[test]
    fn bool_return_writes_zero_or_one_to_int_output() {
        let (ws, mid) = build_ws("fn t() -> bool { return 3.0 == 3.0 }");
        let s = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect("emit");
        assert_eq!(s.ret, ValueTestRet::Bool);
        assert!(s.fragment.contains("out int _rock_value_out;"), "{}", s.fragment);
        assert!(s.fragment.contains("_rock_value_out = (t_ret_bool() ? 1 : 0);"), "{}", s.fragment);
    }

    #[test]
    fn es_profile_carries_version_and_precision() {
        let (ws, mid) = build_ws("fn t() -> float { return abs(-6.5) }");
        let s = emit_value_test(&ws, mid, "t", GlslProfile::Es300).expect("emit");
        assert!(s.fragment.starts_with("#version 300 es\n"), "{}", s.fragment);
        assert!(s.fragment.contains("precision highp float;"), "{}", s.fragment);
        assert!(s.vertex.starts_with("#version 300 es\n"), "{}", s.vertex);
    }

    #[test]
    fn parametered_root_rejects() {
        let (ws, mid) = build_ws("fn t(x: int) -> int { return x }");
        let e = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect_err("params reject");
        assert!(e.message.contains("no parameters"), "{}", e.message);
    }

    #[test]
    fn non_scalar_return_rejects() {
        let (ws, mid) = build_ws("fn t() -> Vec2 { return vec2(1.0, 2.0) }");
        let e = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect_err("vec reject");
        assert!(e.message.contains("not a scalar"), "{}", e.message);
    }

    #[test]
    fn body_outside_tier0_rejects_with_profile_reason() {
        // A string is well above tier 0 -- the fn library profile pass rejects,
        // surfacing the SKIP reason rather than emitting a guess.
        let (ws, mid) = build_ws("fn t() -> int { let s = \"hi\"; return 1 }");
        let e = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect_err("string reject");
        assert!(!e.message.is_empty(), "reason present");
    }
}
