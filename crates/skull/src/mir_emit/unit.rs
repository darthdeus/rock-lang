use super::*;

fn render_default_lit(body: &bone_semantic::hir::Body, e: Idx<Expr>) -> Option<String> {
    match body.expr_arena[e] {
        Expr::Null | Expr::Unit => Some("nil".to_string()),
        Expr::Int(n) => Some(n.to_string()),
        Expr::Float(f) => Some(format!("{:?}", f.0)),
        Expr::Bool(b) => Some(b.to_string()),
        Expr::String(s) => Some(skull_codegen_lua::lua_string(s.as_str())),
        Expr::UnaryMinus(inner, _) => render_default_lit(body, inner).map(|s| format!("-{}", s)),
        _ => None,
    }
}

/// Fills a default a Lua or indirect caller omitted; a Rock direct call
/// already passes it. A nullable parameter's nil can be an explicit `null`,
/// so it gets no fill when null overrides its default.
pub(super) fn default_fill_prologue(
    ws: &Workspace,
    mid: ModuleId,
    fid: Idx<bone_semantic::hir::Function>,
    f: &bone_semantic::hir::Function,
) -> String {
    let mut prologue = String::new();
    let Some(fbody) = ws.module(mid).bodies.get(&BodyOwner::Function(fid)) else {
        return prologue;
    };
    for (i, def) in fbody.param_defaults.iter().enumerate() {
        if let (Some(d), Some(pd)) = (def, f.param_decls.get(i)) {
            if skull_mir::lower::param_null_overrides_default(ws, fbody, i, pd.ty) {
                continue;
            }
            if let Some(lit) = render_default_lit(fbody, *d) {
                let p = pd.name.as_str();
                let _ = writeln!(prologue, "if {p} == nil then {p} = {lit} end");
            }
        }
    }
    prologue
}

#[derive(Debug)]
pub(super) enum ExternRet {
    None,
    /// LuaJIT hands back a boxed cdata for a 64-bit C integer, and keys a
    /// table by cdata identity, so the wrapper coerces to a number.
    Int,
    Uint,
    /// Returned by value: the C ABI is `void f(T* out, ..)`.
    ValueStruct(String),
    /// The `cdef` declares the return `uint8_t`: LuaJIT cannot record a
    /// `bool`-returning C call from a trace rooted at the wrapper (`NYI:
    /// unsupported C function type`). The `cdef` and this arm's `~= 0` must
    /// change together.
    Bool,
    /// Floats and strings already cross as Lua values; a `Ptr<T>` stays cdata.
    AsIs,
}

/// The `ffi.cdef` is inside an opaque body, so the declared type is the only
/// contract: an unresolvable one is an error, never "return it as-is".
pub(super) fn extern_ret(
    ws: &Workspace,
    f: &bone_semantic::hir::Function,
) -> Result<ExternRet, CompilerError> {
    use skull_mir::monoty::{MonoCtor, MonoSubst, MonoTy, MonoTyReason};

    let Some(ret) = f.return_ty else {
        return Ok(ExternRet::None);
    };
    let unrealizable = |what: &str| {
        CompilerError::new_with_label(
            f.name_span,
            format!(
                "extern `{}` returns {what}, which has no C representation: the wrapper \
                 cannot know what the callee hands back or how to give it the declared \
                 Rock type. Declare a concrete return type.",
                f.name
            ),
        )
    };
    let describe = |r: MonoTyReason| -> CompilerError {
        match r {
            MonoTyReason::UnboundGeneric(n) => {
                unrealizable(&format!("the unbound generic parameter `{n}`"))
            }
            MonoTyReason::DepthLimit => unrealizable("a type that does not bottom out"),
            MonoTyReason::RecoveryError => unrealizable("a type inference could not recover"),
            other => unrealizable(&format!(
                "a type the compiler cannot normalize ({})",
                other.bucket()
            )),
        }
    };

    let ty = skull_mir::monoty::convert_decl(ws, ret, &MonoSubst::default()).map_err(describe)?;
    let ty = skull_mir::monoty::resolve_newtype_chain(ws, ty).map_err(|reason| {
        let detail = match reason {
            skull_mir::monoty::NewtypeResolutionError::Conversion(reason) => {
                return describe(reason);
            }
            other => other.to_string(),
        };
        unrealizable(&format!("a newtype representation ({detail})"))
    })?;

    Ok(match ty {
        MonoTy::Int => ExternRet::Int,
        MonoTy::Uint => ExternRet::Uint,
        MonoTy::Bool => ExternRet::Bool,
        MonoTy::App {
            ctor: MonoCtor::Nominal(name),
            ..
        } if crate::is_value_struct(ws, name) => ExternRet::ValueStruct(name.as_str().to_string()),
        _ => ExternRet::AsIs,
    })
}

#[derive(Default)]
pub(super) struct UnitOut {
    pub(super) lua: String,
    pub(super) hard_error: Option<CompilerError>,
    pub(super) refs: Option<(Ustr, Vec<Ustr>, Vec<Ustr>)>,
    pub(super) pending_init: Option<(Ustr, Ustr, String)>,
    // Reachability roots: these bodies are always emitted.
    pub(super) calls: Vec<FuncRef>,
    pub(super) specs: Vec<skull_mir::bound::SpecDemand>,
    pub(super) code_artifact: skull_mir::code_identity::BodyCodeArtifact,
    pub(super) global_names: Vec<Ustr>,
    // A unit has no chunk to carry these, and most production embeds are in
    // global initializers.
    pub(super) comptime_reads: Vec<skull_mir::mir::ComptimeRead>,
    pub(super) allocation_bodies: Vec<crate::allocation_report::AllocationBody>,
    pub(super) times: PhaseTimes,
    pub(super) attribution: UnitAttribution,
}

pub(super) fn emit_lowered(
    ws: &Workspace,
    cg: &CodegenCtx,
    out: &mut UnitOut,
    mf: &MirFunc,
    lua: String,
    body_name: &str,
    collect_refs: Option<Ustr>,
    t_emit: Duration,
) {
    let tv = Instant::now();
    if let Some(key) = collect_refs {
        let (gs, cs) = direct_refs(ws, cg, mf);
        out.refs = Some((key, gs, cs));
    }
    out.calls = skull_mir::reach::call_targets(mf);
    out.global_names = skull_mir::reach::global_ref_names(mf);
    crate::push_value_ctor_calls(&lua, &mut out.global_names);
    out.comptime_reads = mf.comptime_reads.clone();
    out.lua = lua;
    let mut code_artifact =
        skull_mir::code_identity::CodeArtifact::from_mir_tree_with_volatile_vars(
            mf,
            &ws.comptime_inputs.volatile_vars,
        );
    code_artifact.set_body_names(mf, body_name);
    out.code_artifact = code_artifact.into();
    out.times.validation += tv.elapsed();
    out.times.validation_work = true;
    out.times.emit += t_emit;
    out.times.emit_work |= t_emit > Duration::ZERO;
    let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(t_emit);
    out.times.regalloc += regalloc;
    out.times.regalloc_work |= regalloc > Duration::ZERO;
}

/// A decorator or global-initializer body, which runs once at chunk load.
pub(super) struct UnitBody<'a> {
    pub(super) mid: ModuleId,
    pub(super) owner: BodyOwner,
    pub(super) symbol: &'a str,
    pub(super) check_name: &'a str,
    pub(super) source_name: &'a str,
    pub(super) span: bone_semantic::prelude::Span,
}

impl UnitBody<'_> {
    /// Convert, check and print a lowered unit body with its hoisted
    /// functions. `None` once `out.hard_error` is set.
    pub(super) fn emit(
        &self,
        ws: &Workspace,
        scx: &skull_mir::lower::ScalarCtx,
        cg: &CodegenCtx,
        mf: &mut MirFunc,
        out: &mut UnitOut,
    ) -> Option<String> {
        out.attribution.examined = true;
        out.attribution.hir_nodes = hir_nodes_for_owner(ws, self.mid, self.owner);
        out.attribution.mir_instructions = mf.insts.len();
        out.attribution.mir_lowered = mf.insts.len();
        out.times.lower_work = out.attribution.mir_instructions > 0;
        if let Err(e) = convert_unit_body(ws, cg, mf, self.symbol, &mut out.times) {
            out.hard_error = Some(e);
            return None;
        }
        let tv = Instant::now();
        let check =
            check_unit_spec_bindings(ws, self.mid, self.check_name, mf, self.span, &mut out.specs);
        out.times.validation += tv.elapsed();
        out.times.validation_work = true;
        if let Err(e) = check {
            out.hard_error = Some(e);
            return None;
        }
        skull_codegen_lua::regalloc_time_reset();
        let te = Instant::now();
        // Runs once at chunk load, outside the definitions section the binding pass covers.
        let emission =
            emit_named_with_allocation_report(ws, cg, scx.interner(), self.symbol, mf, &[], None);
        let mut lua = emission.lua;
        if let Some(body) = crate::allocation_report::collect_emitted_body(
            ws,
            scx.interner(),
            self.symbol,
            self.source_name,
            mf,
            emission.allocation_sites,
        ) {
            out.allocation_bodies.push(body);
        }
        let root_emit = te.elapsed();
        out.times.emit += root_emit;
        out.times.emit_work |= root_emit > Duration::ZERO;
        let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(root_emit);
        out.times.regalloc += regalloc;
        out.times.regalloc_work |= regalloc > Duration::ZERO;
        if let Err(e) = emit_pass_nested_fns(
            ws,
            cg,
            scx,
            &mut mf.nested_fns,
            &mf.statics,
            Some(&mut out.allocation_bodies),
            &mut lua,
            &mut out.times,
        ) {
            out.hard_error = Some(e);
            return None;
        }
        Some(lua)
    }
}
