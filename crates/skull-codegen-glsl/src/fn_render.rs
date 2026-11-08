//! Value types and expressions, calls, block statements and returns.

use super::*;

impl FnEmit<'_> {
    /// The GLSL type of a value instruction.
    pub(crate) fn ty_of(&self, iid: Idx<Inst>) -> Result<GlslTy, CompilerError> {
        Ok(match &self.f.insts[iid] {
            Inst::Const(Lit::Float(_)) => GlslTy::Float,
            Inst::Const(Lit::Int(_)) => GlslTy::Int,
            Inst::Const(Lit::Uint(_)) => GlslTy::Uint,
            Inst::Const(Lit::Bool(_)) => GlslTy::Bool,
            Inst::Const(Lit::Nil) => {
                return Err(err(
                    self.span(),
                    "shader profile: a null/optional value has no GLSL type",
                ));
            }
            Inst::Const(Lit::Str(_)) => {
                return Err(err(
                    self.span(),
                    "shader profile: string literal is not tier 0",
                ));
            }
            Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::FloatMod(..) => GlslTy::Float,
            Inst::IntAdd(..)
            | Inst::IntSub(..)
            | Inst::IntMul(..)
            | Inst::IntDiv(..)
            | Inst::IntMod(..) => GlslTy::Int,
            Inst::UintAdd(..)
            | Inst::UintSub(..)
            | Inst::UintMul(..)
            | Inst::UintDiv(..)
            | Inst::UintMod(..) => GlslTy::Uint,
            Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::UintLt(..)
            | Inst::UintLeq(..)
            | Inst::UintGt(..)
            | Inst::UintGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::Not(_) => GlslTy::Bool,
            Inst::Neg(x) | Inst::BitNot(x) | Inst::UintBitNot(x) => self.ty_of(*x)?,
            Inst::FloatToInt(_) => GlslTy::Int,
            Inst::IntToFloat(_) => GlslTy::Float,
            Inst::UintToFloat(_) | Inst::FloatToF32(_) => GlslTy::Float,
            Inst::IntToUint(_) | Inst::FloatToUint(_) | Inst::IntBitcastUint(_) => GlslTy::Uint,
            Inst::UintToInt(_) | Inst::UintBitcastInt(_) => GlslTy::Int,
            Inst::BitAnd(..)
            | Inst::BitOr(..)
            | Inst::BitXor(..)
            | Inst::Shl(..)
            | Inst::Shr(..) => GlslTy::Int,
            Inst::UintBitAnd(..)
            | Inst::UintBitOr(..)
            | Inst::UintBitXor(..)
            | Inst::UintShl(..)
            | Inst::UintShr(..) => GlslTy::Uint,
            Inst::Load(c) => self.cell_ty.get(c).copied().ok_or_else(|| {
                err(
                    self.span(),
                    "shader profile: a value is read from a local that has no GLSL type",
                )
            })?,
            Inst::Global(name) => match const_lit(self.ws, *name) {
                Some(lit) => const_lit_ty(&lit),
                // `profile_body` rejects every non-constant global read first.
                None => {
                    return Err(err(
                        self.span(),
                        format!(
                            "internal error: global `{name}` reached GLSL emission without a constant value; the shader profile pass must reject it"
                        ),
                    ));
                }
            },
            Inst::Field { base, field } => {
                if self.is_uniform_base(*base) {
                    self.uniform_fields.get(field).copied().ok_or_else(|| {
                        err(self.span(), format!("shader profile: `{field}` is not a value field of the shader interface; a sampler field is opaque -- pass it directly to a texture call"))
                    })?
                } else {
                    match self.ty_of(*base)? {
                        GlslTy::Bvec3 => GlslTy::Bool,
                        GlslTy::Vec2 | GlslTy::Vec3 | GlslTy::Vec4 => GlslTy::Float,
                        other => {
                            return Err(err(
                                self.span(),
                                format!(
                                    "shader profile: field `{field}` read on a `{}` value has no GLSL lowering",
                                    other.name()
                                ),
                            ));
                        }
                    }
                }
            }
            Inst::MakeValueStruct { name, .. } | Inst::MaterializeValueStruct { name, .. } => {
                match name.as_str() {
                    "Vec2" | "Vec2f" => GlslTy::Vec2,
                    "Vec3" | "Vec3f" => GlslTy::Vec3,
                    "Vec4" | "Vec4f" => GlslTy::Vec4,
                    "Bvec3" => GlslTy::Bvec3,
                    "Mat4" => GlslTy::Mat4,
                    other => {
                        return Err(err(
                            self.span(),
                            format!("shader profile: value struct `{other}` is not tier 0"),
                        ));
                    }
                }
            }
            Inst::Call { func, args } => {
                let cf = &self.ws.module(func.module).functions[func.func];
                if self.concrete_plan.is_some() {
                    let sig = self.concrete_call_signature(iid)?;
                    let id = sig.result.ok_or_else(|| {
                        err(
                            self.span(),
                            format!(
                                "shader profile: call to `{}` has no concrete result type",
                                cf.name
                            ),
                        )
                    })?;
                    concrete_glsl_ty(self.interner, id, self.span(), "call result")?
                } else {
                    let csub = self.callee_inst(iid).subst(self.ws, self.interner);
                    match cf
                        .return_ty
                        .and_then(|rt| glsl_ty(self.ws, &csub, rt, cf.name_span, "return").ok())
                    {
                        Some(t) => t,
                        // Generic-return genType builtins (min<T>/max<T>/clamp/abs):
                        // GLSL's result type is the argument type. Load-bearing under
                        // GLSL, which requires the correct assignment type.
                        None if matches!(
                            declared_name(cf.name.as_str()),
                            "min" | "max" | "clamp" | "abs"
                        ) && !args.is_empty() =>
                        {
                            self.ty_of(args[0])?
                        }
                        None => {
                            return Err(err(
                                self.span(),
                                format!(
                                    "shader profile: call to `{}` has no tier-0 GLSL result type",
                                    cf.name
                                ),
                            ));
                        }
                    }
                }
            }
            other => {
                return Err(err(
                    self.span(),
                    format!(
                        "shader profile: {} has no GLSL value type",
                        inst_kind(other)
                    ),
                ));
            }
        })
    }

    /// Render at a float context slot. int->float subsumption is materialized at
    /// lowering (`Inst::IntToFloat`, which `render_val` folds/wraps), so an
    /// int-typed value reaching here that is NOT already coerced is a MISSED
    /// lowering sink -- a hard emit error, never silent driver-dependent GLSL.
    /// (GLSL ES has no implicit int->float; desktop drivers vary on the corner.)
    pub(crate) fn render_val_t(
        &self,
        iid: Idx<Inst>,
        want: GlslTy,
    ) -> Result<String, CompilerError> {
        if want == GlslTy::Float && self.ty_of(iid)? == GlslTy::Int {
            return Err(err(
                self.span(),
                format!(
                    "shader profile: int-typed value ({}) in a float context without an IntToFloat coercion -- a missed lowering sink",
                    inst_kind(&self.f.insts[iid])
                ),
            ));
        }
        self.render_val(iid)
    }

    /// Render an expression or a reference to its retained call result.
    pub(crate) fn render_val(&self, iid: Idx<Inst>) -> Result<String, CompilerError> {
        if let Some(name) = self.value_name.get(&iid) {
            return Ok(name.clone());
        }
        let bin =
            |a: &Idx<Inst>, op: &str, b: &Idx<Inst>, s: &Self| -> Result<String, CompilerError> {
                Ok(format!(
                    "({} {} {})",
                    s.render_val(*a)?,
                    op,
                    s.render_val(*b)?
                ))
            };
        let fbin =
            |a: &Idx<Inst>, op: &str, b: &Idx<Inst>, s: &Self| -> Result<String, CompilerError> {
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
            Inst::Const(Lit::Uint(i)) => format!("{i}u"),
            Inst::Const(Lit::Bool(b)) => b.to_string(),
            Inst::Const(Lit::Nil) => return Err(err(self.span(), "shader profile: null literal is not tier 0")),
            Inst::Const(Lit::Str(_)) => return Err(err(self.span(), "shader profile: string literal is not tier 0")),
            // Admitted by `profile_body` only when it folds, so a miss here is a
            // profile/emit divergence, not user error.
            Inst::Global(name) => const_lit(self.ws, *name)
                .as_ref()
                .and_then(render_const_lit)
                .ok_or_else(|| err(self.span(), format!(
                    "shader profile: const `{name}` passed the profile pass but did not fold at emit -- a compiler bug"
                )))?,
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
            Inst::UintAdd(a, b) => bin(a, "+", b, self)?,
            Inst::UintSub(a, b) => bin(a, "-", b, self)?,
            Inst::UintMul(a, b) => bin(a, "*", b, self)?,
            Inst::UintDiv(a, b) => bin(a, "/", b, self)?,
            Inst::UintMod(a, b) => bin(a, "%", b, self)?,
            Inst::FloatDiv(a, b) => fbin(a, "/", b, self)?,
            Inst::FloatMod(a, b) => format!(
                "mod({}, {})",
                self.render_val_t(*a, GlslTy::Float)?,
                self.render_val_t(*b, GlslTy::Float)?
            ),
            // GLSL int / and % are undefined on negatives -> floored helpers.
            Inst::IntDiv(a, b) => format!("rock_idiv({}, {})", self.render_val(*a)?, self.render_val(*b)?),
            Inst::IntMod(a, b) => format!("rock_imod({}, {})", self.render_val(*a)?, self.render_val(*b)?),
            Inst::IntLt(a, b) => bin(a, "<", b, self)?,
            Inst::FloatLt(a, b) => fbin(a, "<", b, self)?,
            Inst::IntLeq(a, b) => bin(a, "<=", b, self)?,
            Inst::FloatLeq(a, b) => fbin(a, "<=", b, self)?,
            Inst::IntGt(a, b) => bin(a, ">", b, self)?,
            Inst::FloatGt(a, b) => fbin(a, ">", b, self)?,
            Inst::IntGeq(a, b) => bin(a, ">=", b, self)?,
            Inst::FloatGeq(a, b) => fbin(a, ">=", b, self)?,
            Inst::UintLt(a, b) => bin(a, "<", b, self)?,
            Inst::UintLeq(a, b) => bin(a, "<=", b, self)?,
            Inst::UintGt(a, b) => bin(a, ">", b, self)?,
            Inst::UintGeq(a, b) => bin(a, ">=", b, self)?,
            // Untyped Eq/NotEq: operands arrive same-typed (a mixed `float == int`
            // was int->float-coerced at lowering), so render a plain compare.
            Inst::Eq(a, b) => bin(a, "==", b, self)?,
            Inst::NotEq(a, b) => bin(a, "!=", b, self)?,
            Inst::Neg(x) => format!("(-{})", self.render_val(*x)?),
            Inst::Not(x) => format!("(!{})", self.render_val(*x)?),
            Inst::BitNot(x) | Inst::UintBitNot(x) => format!("(~{})", self.render_val(*x)?),
            Inst::BitAnd(a, b) | Inst::UintBitAnd(a, b) => bin(a, "&", b, self)?,
            Inst::BitOr(a, b) | Inst::UintBitOr(a, b) => bin(a, "|", b, self)?,
            Inst::BitXor(a, b) | Inst::UintBitXor(a, b) => bin(a, "^", b, self)?,
            Inst::Shl(a, b) | Inst::UintShl(a, b) => bin(a, "<<", b, self)?,
            Inst::Shr(a, b) | Inst::UintShr(a, b) => bin(a, ">>", b, self)?,
            Inst::FloatToInt(x) => format!("int({})", self.render_val(*x)?),
            // int->float subsumption (inserted at lowering): fold an int literal
            // to float text, else materialize the `float(...)` conversion.
            Inst::IntToFloat(x) => match &self.f.insts[*x] {
                Inst::Const(Lit::Int(i)) => fmt_float(*i as f64),
                _ => format!("float({})", self.render_val(*x)?),
            },
            Inst::UintToFloat(x) => format!("float({})", self.render_val(*x)?),
            // Shader floats are already single precision.
            Inst::FloatToF32(x) => self.render_val(*x)?,
            Inst::IntToUint(x) | Inst::FloatToUint(x) => format!("uint({})", self.render_val(*x)?),
            Inst::UintToInt(x) => format!("int({})", self.render_val(*x)?),
            Inst::UintBitcastInt(x) => format!("uintBitsToInt({})", self.render_val(*x)?),
            Inst::IntBitcastUint(x) => format!("intBitsToUint({})", self.render_val(*x)?),
            Inst::Field { base, field } => {
                if self.is_uniform_base(*base) {
                    field.as_str().to_string() // uniform global, not a struct member
                } else {
                    format!("{}.{}", self.render_val(*base)?, field)
                }
            }
            // `MaterializeValueStruct` renders identically: on the GPU an
            // authorized box is just a `vec2(..)` ctor -- the authorization is a
            // Lua-side concept (`authorize_gpu_boundary_boxes`).
            Inst::MakeValueStruct { name, fields, .. } | Inst::MaterializeValueStruct { name, fields, .. } => {
                // GLSL must never see the post-normalization LEAF form (dotted
                // field names, `flatten_nobox_nested_ctors`). It cannot today --
                // this crate never calls `skull_transform`, so it never runs
                // `cleanup` -- and it must not start: GLSL has real nested ctors
                // where the nested form is correct and free, so flattening would
                // be actively wrong here. Fail loud if a pipeline change ever
                // routes normalized MIR to this emitter.
                debug_assert!(
                    !fields.iter().any(|(f, _)| f.contains('.')),
                    "GLSL received the nobox leaf form for `{name}`: this emitter \
                     must not run on `cleanup`-normalized MIR"
                );
                // Bvec3 components render at a bool context; the vecN ctors take
                // float components (int operands materialize an IntToFloat).
                let (ctor, comp_ty) = match name.as_str() {
                    "Vec2" | "Vec2f" => ("vec2", GlslTy::Float),
                    "Vec3" | "Vec3f" => ("vec3", GlslTy::Float),
                    "Vec4" | "Vec4f" => ("vec4", GlslTy::Float),
                    "Bvec3" => ("bvec3", GlslTy::Bool),
                    other => return Err(err(self.span(), format!("shader profile: value struct `{other}` is not tier 0"))),
                };
                let args: Result<Vec<String>, CompilerError> =
                    fields.iter().map(|(_, v)| self.render_val_t(*v, comp_ty)).collect();
                format!("{ctor}({})", args?.join(", "))
            }
            Inst::Call { func, args } => self.render_call(iid, *func, args)?,
            other => return Err(err(self.span(), format!("shader profile: cannot render {} as a GLSL expression", inst_kind(other)))),
        })
    }

    /// Render a direct call. `iid` names the call SITE, which is where the
    /// callee's instantiation is recorded -- both the emitted name and every
    /// declared param/return type resolve through it, so a specialized callee
    /// and its definition cannot disagree.
    fn render_call(
        &self,
        iid: Idx<Inst>,
        func: FuncRef,
        args: &[Idx<Inst>],
    ) -> Result<String, CompilerError> {
        let cf = &self.ws.module(func.module).functions[func.func];
        let callee = self.callee_inst(iid);
        let param_types = self.call_param_types(iid, cf)?;
        let name = declared_name(cf.name.as_str());
        // A non-scalar `__op_*` call is a Vec operator -> native infix. An int
        // operand (`*(Vec2, int)`) is cast to float (GLSL has no vec*int).
        if let Some(sym) = vec_op_symbol(name) {
            if args.len() == 2 {
                let render_operand = |i: usize, e: &Self| -> Result<String, CompilerError> {
                    let pt = param_types.get(i).and_then(|t| *t).ok_or_else(|| {
                        err(
                            e.span(),
                            "shader profile: vector operator parameter has no GLSL type",
                        )
                    })?;
                    let s = e.render_val(args[i])?;
                    Ok(if pt == GlslTy::Int {
                        format!("float({s})")
                    } else {
                        s
                    })
                };
                return Ok(format!(
                    "({} {} {})",
                    render_operand(0, self)?,
                    sym,
                    render_operand(1, self)?
                ));
            }
        }
        // Args render against the declared param type where it maps to GLSL
        // (a generic param, e.g. `min<T>`, doesn't -- those render plain).
        let arg_t = |i: usize, a: Idx<Inst>, e: &Self| -> Result<String, CompilerError> {
            match param_types.get(i).copied().flatten() {
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
            && param_types
                .iter()
                .flatten()
                .any(|ty| matches!(ty, GlslTy::Vec2 | GlslTy::Vec3 | GlslTy::Vec4))
        {
            let n: usize = param_types
                .iter()
                .map(|ty| match ty {
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
                    ));
                }
            };
            let rargs: Result<Vec<String>, CompilerError> = args
                .iter()
                .enumerate()
                .map(|(i, a)| arg_t(i, *a, self))
                .collect();
            return Ok(format!("{ctor}({})", rargs?.join(", ")));
        }
        if let Some(Emission::Template(tmpl)) = GlslIntrinsics.lookup(cf) {
            let mut s = tmpl.to_string();
            let vector_bound_ty = if matches!(name, "min" | "max" | "clamp") {
                param_types
                    .first()
                    .copied()
                    .flatten()
                    .filter(|ty| matches!(ty, GlslTy::Vec2 | GlslTy::Vec3 | GlslTy::Vec4))
            } else {
                None
            };
            for (i, a) in args.iter().enumerate() {
                let mut arg = arg_t(i, *a, self)?;
                if i > 0 && param_types.get(i).copied().flatten() == Some(GlslTy::Float) {
                    if let Some(ty) = vector_bound_ty {
                        // Mesa's GLSL constant folder can read unsplatted scalar lanes as zero.
                        arg = format!("{}({arg})", ty.name());
                    }
                }
                s = s.replace(&format!("${}", i + 1), &arg);
            }
            return Ok(s);
        }
        // An emittable user fn -> a plain call to THIS INSTANCE's GLSL name.
        let gname = glsl_name(self.ws, self.interner, callee);
        let rargs: Result<Vec<String>, CompilerError> = args
            .iter()
            .enumerate()
            .map(|(i, a)| arg_t(i, *a, self))
            .collect();
        Ok(format!("{gname}({})", rargs?.join(", ")))
    }

    pub(crate) fn span(&self) -> Span {
        self.fn_span
    }

    /// Emit the straight-line effect statements of a block: Stores (assignments)
    /// and void (effect-only) calls like `discard()`, plus shared vector calls.
    pub(crate) fn emit_block_effects(&mut self, b: Idx<Block>) -> Result<(), CompilerError> {
        for iid in self.f.blocks[b].insts.clone() {
            let line: Option<String> = match &self.f.insts[iid] {
                Inst::Store { cell, value } => {
                    let (cell, value) = (*cell, *value);
                    // Folded diamond result / bound temp: no storage, inlined.
                    if self.folded_cond.contains_key(&cell) || self.folded_bound.contains_key(&cell)
                    {
                        None
                    } else {
                        let rhs = match self.cell_ty.get(&cell) {
                            Some(&want) => self.render_val_t(value, want)?,
                            None => self.render_val(value)?,
                        };
                        Some(format!("{} = {};", self.cell_name[&cell], rhs))
                    }
                }
                Inst::Call { func, args } => {
                    let cf = &self.ws.module(func.module).functions[func.func];
                    if let Some(name) = self.value_name.get(&iid) {
                        Some(format!(
                            "{} {name} = {};",
                            self.ty_of(iid)?.name(),
                            self.render_call(iid, *func, args)?
                        ))
                    } else if cf.return_ty.is_none() {
                        Some(format!("{};", self.render_call(iid, *func, args)?))
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

    pub(crate) fn emit_return(&mut self, vs: &[Idx<Inst>]) -> Result<(), CompilerError> {
        // `void main()` mode: `return <Struct { field, ... }>` destructures into a
        // named global assignment per field -- vertex varyings (a `position` field
        // -> gl_Position) or MRT fragment outputs (a `FragDepth` field ->
        // gl_FragDepth).
        if let Some(mo) = self.main_out.clone() {
            let [v] = vs else {
                return Err(err(
                    self.span(),
                    "shader entry: a void-main entry must return its output struct",
                ));
            };
            let fields: Vec<(Ustr, Idx<Inst>)> = match &self.f.insts[*v] {
                Inst::MakeValueStruct { fields, .. }
                | Inst::MaterializeValueStruct { fields, .. } => {
                    fields.iter().map(|&(n, x)| (n, x)).collect()
                }
                _ => {
                    return Err(err(
                        self.span(),
                        "shader entry: a void-main entry must return an output struct literal",
                    ));
                }
            };
            for (name, val) in fields {
                let builtin = mo.builtin.filter(|(field, _, _)| name.as_str() == *field);
                let (target, want) = if let Some((_, gl, ty)) = builtin {
                    (gl.to_string(), ty)
                } else {
                    // bone typechecks the returned struct against the varyings/MRT
                    // struct, so every field is in the map. Reject rather than
                    // defaulting: a classify/emit drift would otherwise silently
                    // emit a Float-typed assignment for a vec/int output.
                    let ty = mo.fields.get(&name).copied().ok_or_else(|| {
                        err(
                            self.span(),
                            format!(
                                "shader entry: returned field `{name}` is not an output of this entry's \
                                 varyings/MRT struct"
                            ),
                        )
                    })?;
                    (name.to_string(), ty)
                };
                let s = self.render_val_t(val, want)?;
                self.push(&format!("{target} = {s};"));
            }
            self.push("return;");
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
            _ => {
                return Err(err(
                    self.span(),
                    "shader profile: multi-value return is not tier 0",
                ));
            }
        }
        Ok(())
    }
}
