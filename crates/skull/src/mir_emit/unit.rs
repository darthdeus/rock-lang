//! Per-definition emit units and their merge into the program text.

use super::*;

/// Render a literal default-value expression as Lua (for inline(lua) and
/// `extern(ffi)` default fills). `None` for a non-literal default.
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

/// Callee-side `if p == nil then p = <lit> end` fills for a generated Lua
/// wrapper. A Rock direct call already passes a literal default at the site;
/// this covers an indirect or Lua call that omitted it. `inline(lua)` and
/// `extern(ffi)` share the same fill because both wrappers are the body the
/// omitted argument actually arrives at. A nullable parameter with a non-null
/// default gets no fill: its nil is an explicit `null` (EXPLICIT_NULL_DEFAULT).
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

/// What an `extern(ffi)` wrapper does with the callee's return, decided ONCE
/// from the declared Rock type.
///
/// The `ffi.cdef` lives inside an opaque `inline(lua)` body, so the declaration
/// is the only contract the compiler has -- which is exactly why an unresolvable
/// declared type is a hard error here and not a default. Falling back to
/// "return it as-is" would silently reproduce the boxed-handle bug this
/// classification exists to prevent, and would do it precisely where the
/// compiler has admitted it does not know what the type is.
#[derive(Debug)]
pub(super) enum ExternRet {
    /// No return value.
    None,
    /// Rock `int`. LuaJIT hands back a BOXED cdata for a 64-bit C integer
    /// (`uint64_t`, `int64_t`), so the wrapper coerces to a number. A cdata
    /// leaking through is not cosmetic: LuaJIT keys a table by cdata IDENTITY,
    /// so a handle used as a `Map<int, V>` key only ever hits on the box that
    /// was inserted (`b1_ffi_handle.rock`).
    Int,
    /// Rock `uint` uses the same numeric bridge, but remains a distinct static
    /// type so its caller-side normalization and ABI identity are preserved.
    Uint,
    /// A `value` struct returned BY VALUE; the C ABI is then
    /// `void f(T* out, ..)`, so the wrapper bridges an out-param.
    ValueStruct(String),
    /// Rock `bool`. The C side declares the return `uint8_t`, not `bool`, so
    /// the wrapper turns the byte into a Lua boolean -- the same coercion
    /// principle as `Int`, applied to the other representation the declaration
    /// promises but the C ABI does not hand back directly.
    ///
    /// The reason the `cdef` says `uint8_t` is LuaJIT, not the ABI: recording a
    /// `bool`-returning C call needs a Lua frame INSIDE the trace above the call
    /// (`crec_call` -> `crec_snap_caller`, `lj_crecord.c`), and a trace rooted
    /// at this one-line wrapper has none, so it aborts `NYI: unsupported C
    /// function type` every time the wrapper itself becomes a trace root. A
    /// `uint8_t` return records unconditionally. Rust `extern "C" fn -> bool`
    /// already returns one byte 0/1 in that register, so only the `cdef` and
    /// this arm move -- and they MUST move together: `~= 0` over a `bool` cdef
    /// compares a Lua boolean to 0 and is always true.
    Bool,
    /// Everything else crosses as the callee produced it: LuaJIT already hands
    /// back a Lua value for `float`/`double` and a string, and a pointer return
    /// is declared `Ptr<T>` and stays a cdata by design.
    AsIs,
}

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
    // Every non-conversion is a REASON, so none of them can read as "some other
    // opaque type" the way a partial normalizer's fallback node would -- which
    // matters most for the cycle check below, where a summarized instantiation
    // could manufacture a false match.
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

/// One emitted definition (function / decorator / global-init): the Lua text to
/// append plus the facts the surrounding assembly merges back in. Produced
/// independently per definition (see [`par_map`]) so the heavy lower+cleanup+emit
/// work fans out; results are folded back in source order so output is stable.
#[derive(Default)]
pub(super) struct UnitOut {
    /// Lua to append to the chunk (empty for a no-op definition).
    pub(super) lua: String,
    /// A rejection that fails the compile.
    pub(super) hard_error: Option<CompilerError>,
    /// `(emit_key, direct_global_reads, direct_callees)` for global-init ordering.
    pub(super) refs: Option<(Ustr, Vec<Ustr>, Vec<Ustr>)>,
    /// `(global_name, init_key, load_line)` for a global initializer.
    pub(super) pending_init: Option<(Ustr, Ustr, String)>,
    /// Callees this definition still calls -- reachability roots, since decorator
    /// bodies and global initializers are always emitted.
    pub(super) calls: Vec<FuncRef>,
    /// Specialized callees this definition needs emitted (see `Chunk::specs`).
    pub(super) specs: Vec<skull_mir::bound::SpecDemand>,
    /// Compiler-owned logical callable identities in this emitted body.
    pub(super) code_artifact: skull_mir::code_identity::CodeArtifact,
    /// Module-global names read/written (fn-as-value edges; see `Chunk`).
    pub(super) global_names: Vec<Ustr>,
    /// Comptime inputs this unit's body baked in (`build_var`/`embed_string`).
    /// Always-emitted units have no Chunk to carry these, and ALL in-tree
    /// production comptime reads are global initializers -- dropping them here
    /// is how the cimgui embeds went missing from the watch set.
    pub(super) comptime_reads: Vec<skull_mir::mir::ComptimeRead>,
    pub(super) allocation_bodies: Vec<crate::allocation_report::AllocationBody>,
    pub(super) times: PhaseTimes,
    pub(super) attribution: UnitAttribution,
}

/// Record one emitted definition's Lua, emit time, and (for the non-coroutine
/// paths) its call-graph facts. `collect_refs` carries the emit key when the
/// definition seeds global-init ordering; `None` for coroutines, which never do.
/// Reads the per-thread regalloc accumulator that the caller reset before emit.
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
    out.comptime_reads = mf.comptime_reads.clone();
    out.lua = lua;
    out.code_artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree_with_volatile_vars(
        mf,
        &ws.comptime_inputs.volatile_vars,
    );
    out.code_artifact.set_body_names(mf, body_name);
    out.times.validation += tv.elapsed();
    out.times.validation_work = true;
    out.times.emit += t_emit;
    out.times.emit_work |= t_emit > Duration::ZERO;
    let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(t_emit);
    out.times.regalloc += regalloc;
    out.times.regalloc_work |= regalloc > Duration::ZERO;
}
