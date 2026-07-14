//! skull-codegen-glsl -- GLSL function-library emission (route B, `docs/glsl-spike.md`).
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
//! spec from `docs/glsl-spike.md`, keyed by the callee's mangled signature (name +
//! GLSL param types) -- the future `@intrinsic` GLSL column.

use bone_semantic::hir::{BuiltinTy, Function, TyId, TypeCtor, TypeExpr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{FnList, ModuleId, Span};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_codegen_lua::{compute_overloaded_names, mangle_fn};
use skull_mir::lower::{lower_function, ScalarCtx};
use skull_mir::mir::*;
use skull_parser::CompilerError;
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
        if f.is_extern || f.lua_body.is_some() {
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
        glsl_ty(ws, rt, f.name_span, "return")?;
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
            Inst::Const(Lit::Nil) => {
                return Err(err(span, format!("shader profile: `{}` uses a non-tier-0 construct (null / optional) that has no GLSL lowering", f.name)));
            }
            // Allowed tier-0 instructions.
            Inst::Const(_)
            | Inst::Load(_)
            | Inst::IntAdd(..) | Inst::IntSub(..) | Inst::IntMul(..) | Inst::IntDiv(..) | Inst::IntMod(..)
            | Inst::FloatAdd(..) | Inst::FloatSub(..) | Inst::FloatMul(..) | Inst::FloatDiv(..) | Inst::FloatMod(..)
            | Inst::IntLt(..) | Inst::IntLeq(..) | Inst::IntGt(..) | Inst::IntGeq(..)
            | Inst::FloatLt(..) | Inst::FloatLeq(..) | Inst::FloatGt(..) | Inst::FloatGeq(..)
            | Inst::Eq(..) | Inst::NotEq(..)
            | Inst::Neg(_) | Inst::Not(_) | Inst::FloatToInt(_)
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
        Inst::MakeArray(_) | Inst::MakeVec(_) => "array/Vec",
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

/// The GLSL intrinsic template for a callee, or `None` if it must be emitted as a
/// GLSL fn definition. `$1`/`$2`/`$3` are argument placeholders. Keyed by the
/// callee's mangled signature (name + GLSL param types) -- the `docs/glsl-spike.md`
/// tier-0 mapping table. Vector operators (`__op_*` reaching here are non-scalar,
/// so Vec) are handled at the call site, not here.
fn intrinsic_template(f: &Function) -> Option<&'static str> {
    let n = f.name.as_str();
    let arity = f.param_decls.iter().count();
    // Only stdlib-shaped math maps; a param must be a tier-0 scalar/vec (a user
    // fn that merely shares a name but takes, say, a Vec, still resolves here by
    // name -- acceptable for the restricted profile, the stdlib owns these names).
    match (n, arity) {
        ("sin", 1) => Some("sin($1)"),
        ("cos", 1) => Some("cos($1)"),
        ("tan", 1) => Some("tan($1)"),
        ("sqrt", 1) => Some("sqrt($1)"),
        ("abs", 1) => Some("abs($1)"),
        ("floor", 1) => Some("floor($1)"),
        ("ceil", 1) => Some("ceil($1)"),
        ("fract", 1) => Some("fract($1)"),
        ("sign", 1) => Some("sign($1)"),
        ("pow", 2) => Some("pow($1, $2)"),
        ("atan2", 2) => Some("atan($1, $2)"),
        // GLSL round() half-way rounding is implementation-defined; pin floor(x+0.5).
        ("round", 1) => Some("floor($1 + 0.5)"),
        ("min", 2) => Some("min($1, $2)"),
        ("max", 2) => Some("max($1, $2)"),
        ("clamp", 3) => Some("clamp($1, $2, $3)"),
        ("lerp", 3) => Some("mix($1, $2, $3)"),
        ("smoothstep", 3) => Some("smoothstep($1, $2, $3)"),
        // int<->float identity casts.
        ("int_to_float" | "float" | "to_float", 1) => Some("float($1)"),
        // Vec2 helpers (the field-math bodies never emit).
        ("vec2_dot", 2) => Some("dot($1, $2)"),
        ("vec2_length", 1) => Some("length($1)"),
        ("vec2_distance", 2) => Some("distance($1, $2)"),
        ("v", 2) => Some("vec2($1, $2)"),
        ("v", 1) => Some("vec2($1)"),
        ("v", 0) | ("zero", 0) => Some("vec2(0.0)"),
        _ => None,
    }
}

/// Whether a callee is rendered inline at call sites (an intrinsic template or a
/// Vec operator's native infix), rather than emitted as a GLSL fn definition. A
/// `__op_*` reaching codegen as a real call is always non-scalar (scalar ops lower
/// to native MIR insts), so a 2-arg `__op_*` is always a Vec operator.
fn renders_inline(f: &Function) -> bool {
    intrinsic_template(f).is_some()
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

struct FnEmit<'a> {
    ws: &'a Workspace,
    overloaded: &'a HashSet<Ustr>,
    f: &'a MirFunc,
    /// Cell -> emitted GLSL identifier.
    cell_name: HashMap<Idx<Cell>, String>,
    /// Cell -> GLSL type (params from signature, others from first store).
    cell_ty: HashMap<Idx<Cell>, GlslTy>,
    /// Loop headers -> their recognized counted-loop shape.
    loops: HashMap<Idx<Block>, LoopInfo>,
    /// Preheader stores that initialize a loop counter (rendered as the for-loop's
    /// counter init, so the plain store is suppressed... actually kept, see walk).
    out: String,
    indent: usize,
    /// Guard against unstructured control flow / infinite walks.
    visited: HashSet<Idx<Block>>,
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

/// Emit `<ret> <name>(<params>) { <body> }` for one lowered fn.
fn emit_fn_def(
    ws: &Workspace,
    overloaded: &HashSet<Ustr>,
    fref: FuncRef,
    mf: &MirFunc,
) -> Result<String, CompilerError> {
    let f = &ws.module(fref.module).functions[fref.func];
    let name = mangle_fn(ws, f, overloaded);
    let ret = match f.return_ty {
        Some(rt) => glsl_ty(ws, rt, f.name_span, "return")?.name(),
        None => "void",
    };

    let mut e = FnEmit {
        ws,
        overloaded,
        f: mf,
        cell_name: HashMap::default(),
        cell_ty: HashMap::default(),
        loops: HashMap::default(),
        out: String::new(),
        indent: 1,
        visited: HashSet::default(),
    };
    e.assign_cell_names();
    e.assign_param_types(f)?;
    e.recognize_loops();
    e.compute_cell_types();

    // Parameter list.
    let params: Vec<String> = mf
        .params
        .iter()
        .map(|&c| {
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
        if let Some(ty) = e.cell_ty.get(&cid) {
            decls.push_str(&format!("  {} {};\n", ty.name(), e.cell_name[&cid]));
        }
    }

    e.emit_seq(mf.entry, None)?;
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

    fn assign_param_types(&mut self, f: &Function) -> Result<(), CompilerError> {
        for (i, &c) in self.f.params.iter().enumerate() {
            let ty = glsl_ty(self.ws, f.param_decls[i].ty, f.name_span, "parameter")?;
            self.cell_ty.insert(c, ty);
        }
        Ok(())
    }

    /// Type every non-param cell from the value of its first store, in RPO.
    fn compute_cell_types(&mut self) {
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
            Inst::Load(c) => self.cell_ty.get(c).copied().unwrap_or(GlslTy::Float),
            Inst::Field { .. } => GlslTy::Float, // Vec component
            Inst::MakeValueStruct { name, .. } => match name.as_str() {
                "Vec2" => GlslTy::Vec2,
                "Vec3" => GlslTy::Vec3,
                "Vec4" => GlslTy::Vec4,
                _ => GlslTy::Float,
            },
            Inst::Call { func, .. } => {
                let cf = &self.ws.module(func.module).functions[func.func];
                match cf.return_ty.and_then(|rt| glsl_ty(self.ws, rt, cf.name_span, "return").ok()) {
                    Some(t) => t,
                    None => GlslTy::Float,
                }
            }
            _ => GlslTy::Float,
        }
    }

    fn recognize_loops(&mut self) {
        let mut loops = HashMap::default();
        for (h, _) in self.f.blocks.iter() {
            if let Some(li) = match_counted_loop(self.f, h) {
                loops.insert(h, li);
            }
        }
        self.loops = loops;
    }

    /// Render a value instruction as a GLSL expression (inlined; MIR values are
    /// single-use, so no CSE duplication).
    fn render_val(&self, iid: Idx<Inst>) -> Result<String, CompilerError> {
        let bin = |a: &Idx<Inst>, op: &str, b: &Idx<Inst>, s: &Self| -> Result<String, CompilerError> {
            Ok(format!("({} {} {})", s.render_val(*a)?, op, s.render_val(*b)?))
        };
        Ok(match &self.f.insts[iid] {
            Inst::Const(Lit::Float(f)) => fmt_float(*f),
            Inst::Const(Lit::Int(i)) => i.to_string(),
            Inst::Const(Lit::Bool(b)) => b.to_string(),
            Inst::Const(Lit::Nil) => return Err(err(self.span(), "shader profile: null literal is not tier 0")),
            Inst::Const(Lit::Str(_)) => return Err(err(self.span(), "shader profile: string literal is not tier 0")),
            Inst::Load(c) => self.cell_name[c].clone(),
            Inst::IntAdd(a, b) | Inst::FloatAdd(a, b) => bin(a, "+", b, self)?,
            Inst::IntSub(a, b) | Inst::FloatSub(a, b) => bin(a, "-", b, self)?,
            Inst::IntMul(a, b) | Inst::FloatMul(a, b) => bin(a, "*", b, self)?,
            Inst::FloatDiv(a, b) => bin(a, "/", b, self)?,
            Inst::FloatMod(a, b) => format!("mod({}, {})", self.render_val(*a)?, self.render_val(*b)?),
            // GLSL int / and % are undefined on negatives -> floored helpers.
            Inst::IntDiv(a, b) => format!("__rock_idiv({}, {})", self.render_val(*a)?, self.render_val(*b)?),
            Inst::IntMod(a, b) => format!("__rock_imod({}, {})", self.render_val(*a)?, self.render_val(*b)?),
            Inst::IntLt(a, b) | Inst::FloatLt(a, b) => bin(a, "<", b, self)?,
            Inst::IntLeq(a, b) | Inst::FloatLeq(a, b) => bin(a, "<=", b, self)?,
            Inst::IntGt(a, b) | Inst::FloatGt(a, b) => bin(a, ">", b, self)?,
            Inst::IntGeq(a, b) | Inst::FloatGeq(a, b) => bin(a, ">=", b, self)?,
            Inst::Eq(a, b) => bin(a, "==", b, self)?,
            Inst::NotEq(a, b) => bin(a, "!=", b, self)?,
            Inst::Neg(x) => format!("(-{})", self.render_val(*x)?),
            Inst::Not(x) => format!("(!{})", self.render_val(*x)?),
            Inst::FloatToInt(x) => format!("int({})", self.render_val(*x)?),
            Inst::Field { base, field } => format!("{}.{}", self.render_val(*base)?, field),
            Inst::MakeValueStruct { name, fields } => {
                let ctor = match name.as_str() {
                    "Vec2" => "vec2",
                    "Vec3" => "vec3",
                    "Vec4" => "vec4",
                    other => return Err(err(self.span(), format!("shader profile: value struct `{other}` is not tier 0"))),
                };
                let args: Result<Vec<String>, CompilerError> =
                    fields.iter().map(|(_, v)| self.render_val(*v)).collect();
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
        if let Some(tmpl) = intrinsic_template(cf) {
            let mut s = tmpl.to_string();
            for (i, a) in args.iter().enumerate() {
                s = s.replace(&format!("${}", i + 1), &self.render_val(*a)?);
            }
            return Ok(s);
        }
        // An emittable user fn -> a plain call to its (mangled) GLSL name.
        let gname = mangle_fn(self.ws, cf, self.overloaded);
        let rargs: Result<Vec<String>, CompilerError> = args.iter().map(|a| self.render_val(*a)).collect();
        Ok(format!("{gname}({})", rargs?.join(", ")))
    }

    fn span(&self) -> Span {
        Span::default()
    }

    /// Emit a structured sequence of blocks starting at `start`, stopping before
    /// `stop` (its continuation). The restricted profile admits straight-line
    /// code, counted `for` loops, and (later) if-diamonds.
    fn emit_seq(&mut self, start: Idx<Block>, stop: Option<Idx<Block>>) -> Result<(), CompilerError> {
        let mut cur = start;
        loop {
            if Some(cur) == stop {
                return Ok(());
            }
            if !self.visited.insert(cur) {
                return Err(err(self.span(), "shader profile: unstructured control flow (unsupported loop/branch shape)"));
            }
            // Loop header?
            if let Some(li) = self.loops.get(&cur).copied() {
                self.emit_loop(li)?;
                cur = li.exit;
                continue;
            }
            // Straight-line effects.
            self.emit_block_effects(cur)?;
            match self.f.blocks[cur].term.clone() {
                Term::Return(vs) => {
                    self.emit_return(&vs)?;
                    return Ok(());
                }
                Term::Unreachable => return Ok(()),
                Term::Goto(t) => {
                    cur = t;
                }
                Term::Branch { .. } => {
                    return Err(err(self.span(), "shader profile: conditional branch (if / &&) is not yet supported on GLSL"));
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

    /// Emit the straight-line effect statements of a block (Stores). Value insts
    /// are inlined at use, so only effects produce statements here.
    fn emit_block_effects(&mut self, b: Idx<Block>) -> Result<(), CompilerError> {
        for iid in self.f.blocks[b].insts.clone() {
            if let Inst::Store { cell, value } = self.f.insts[iid] {
                let rhs = self.render_val(value)?;
                let line = format!("{} = {};", self.cell_name[&cell], rhs);
                self.push(&line);
            }
        }
        Ok(())
    }

    fn emit_return(&mut self, vs: &[Idx<Inst>]) -> Result<(), CompilerError> {
        match vs {
            [] => self.push("return;"),
            [v] => {
                let s = self.render_val(*v)?;
                self.push(&format!("return {s};"));
            }
            _ => return Err(err(self.span(), "shader profile: multi-value return is not tier 0")),
        }
        Ok(())
    }

    fn emit_loop(&mut self, li: LoopInfo) -> Result<(), CompilerError> {
        let slot = self.cell_name[&li.slot].clone();
        let bound = self.render_val(li.bound)?;
        let cmp = if li.inclusive { "<=" } else { "<" };
        self.push(&format!("for (; {slot} {cmp} {bound}; {slot} = {slot} + 1) {{"));
        self.indent += 1;
        // The loop body runs from body_entry, jumping back at the latch.
        self.emit_seq(li.body_entry, Some(li.latch))?;
        self.indent -= 1;
        self.push("}");
        Ok(())
    }
}

// --- counted-loop recognition (mirrors skull-codegen-lua's, for the range-for
// shape lower_for_range emits) ------------------------------------------------

/// `Some(LoopInfo)` iff `h` is a `for i in lo..hi` header: a `Branch` on
/// `IntLt/IntLeq(load slot, bound)`, entered by exactly a preheader + a
/// pure-increment latch (`load slot; 1; add; store slot; goto h`).
fn match_counted_loop(f: &MirFunc, h: Idx<Block>) -> Option<LoopInfo> {
    let Term::Branch { cond, then_bb, else_bb } = f.blocks[h].term else {
        return None;
    };
    let (slot, bound, inclusive) = match &f.insts[cond] {
        Inst::IntLt(a, b) => (load_cell(f, *a)?, *b, false),
        Inst::IntLeq(a, b) => (load_cell(f, *a)?, *b, true),
        _ => return None,
    };
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

fn successors(term: &Term, mut f: impl FnMut(Idx<Block>)) {
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
    let overloaded = compute_overloaded_names(ws);

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
        out.push_str(&emit_fn_def(ws, &overloaded, fref, mf)?);
    }
    Ok(out)
}
