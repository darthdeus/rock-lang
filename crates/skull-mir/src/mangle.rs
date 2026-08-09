//! Name mangling -- the one place a function becomes its emitted symbol, shared
//! by the Lua, GLSL, and JS backends.
//!
//! The scheme is ALWAYS-MANGLE, keyed on the function's own `(name, declared
//! signature)` ONLY -- decl-set-independent, position-independent, and
//! containing NO `__` anywhere (legal in the Lua, JS, and GLSL ES identifier
//! grammars alike). Every function mangles except the verbatim exemptions:
//! `extern`, `@nomangle`, `@test`, `main`, `test_*` (deliberate surface names --
//! `extern(lua)` IS the Lua builtin; `@nomangle` is an explicit shadow the author
//! owns; `main`/`test_*`/`@test` are runner entry points).
//!
//! The separator is a single `_` (adopted after a corpus-wide collision scan
//! found zero collisions over stdlib + stone + every game, ~23k fns --
//! `rock-cli/tests/mangle_corpus_scan.rs`, `docs/mangle-unification-plan.md`).
//! [`collision_errors`] enforces the property permanently: any residual
//! collision (two distinct signatures mangling alike, or a mangled name equal to
//! an exempt bare name) is rejected, never silently renamed.

use bone_semantic::hir::{self, Function, TyId, TypeExpr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::Span;
use skull_parser::CompilerError;

/// The mangle separator. A single `_`; see the module docs for why.
const SEP: &str = "_";

/// The emitted name for the copy of `f` specialized at `key` -- the instantiation
/// the call site already lowered its arguments for.
///
/// Derived from [`mangle_fn`] plus the reified type trees in declaration order, so the
/// definition and every call site agree without a side table, exactly as the base
/// mangle does. `None` for an EMPTY key -- no instantiation to spell -- which
/// producers no longer hand out, so this is the floor rather than a decision.
///
/// An exempt (`@nomangle`, `extern`, ...) callee still gets a suffixed name here.
/// Its bare name belongs to the ONE body the author owns; a specialized copy is a
/// different function and must not claim it.
pub fn spec_fn_name(
    ws: &Workspace,
    f: &Function,
    key: &[crate::monoty::MonoTy],
) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    let mut out = mangle_fn(ws, f);
    out.push_str(SEP);
    out.push_str("spec");
    for ty in key {
        out.push_str(SEP);
        encode_mono_type(ty, &mut out);
    }
    Some(out)
}

/// A specialized body's symbol IS the type's canonical identity
/// ([`MonoTy::canonical`](crate::monoty::MonoTy::canonical)) -- one encoder, so
/// a symbol and an instance identity cannot disagree about which type they name.
fn encode_mono_type(ty: &crate::monoty::MonoTy, out: &mut String) {
    ty.write_canonical(out);
}

/// True if `f` keeps its bare surface name (never mangled).
pub fn is_exempt(f: &Function) -> bool {
    let name = f.name.as_str();
    f.is_extern
        || has_annotation(f, "nomangle")
        || has_annotation(f, "test")
        || name == "main"
        || name.starts_with("test_")
}

fn has_annotation(f: &Function, name: &str) -> bool {
    f.annotations.iter().any(|a| a.name.name.as_str() == name)
}

/// The emitted name for function `f`, identical for its definition and every
/// call site (so they cannot drift), identical with or without a backend sibling
/// or another overload elsewhere (so a hot reload never orphans a running
/// global).
pub fn mangle_fn(ws: &Workspace, f: &Function) -> String {
    if is_exempt(f) {
        return f.name.to_string();
    }
    // Append directly into one buffer -- no per-fragment `String`/`Vec` (this is
    // on the per-call-site codegen path; the old collect+join allocated N+2
    // strings + a vec per reference).
    let mut out = String::with_capacity(f.name.len() + 16);
    out.push_str(f.name.as_str());
    for pd in &f.param_decls {
        out.push_str(SEP);
        encode_into(ws, pd.ty, &mut out);
    }
    out.push_str(SEP);
    out.push_str("ret");
    out.push_str(SEP);
    match f.return_ty {
        Some(ty) => encode_into(ws, ty, &mut out),
        None => out.push_str("unit"),
    }
    out
}

/// Append a type's mangling fragment to `out`. Generic nominal `{name}_{args}`,
/// fn types `fn_{ps}_ret_{r}` (empty `ps` collapses so no `__`), tuples
/// `tupleN_{ps}`.
fn encode_into(ws: &Workspace, ty: TyId, out: &mut String) {
    let te = match ty {
        TyId::Static(m, idx) => ws.module(m).ty_arena[idx],
        TyId::Local(_) => {
            out.push_str("any");
            return;
        }
    };
    match te {
        TypeExpr::Builtin(b) => out.push_str(match b {
            hir::BuiltinTy::Any => "any",
            hir::BuiltinTy::Unit => "unit",
            hir::BuiltinTy::Int => "int",
            hir::BuiltinTy::Float => "float",
            hir::BuiltinTy::Bool => "bool",
            hir::BuiltinTy::String => "string",
            hir::BuiltinTy::Never => "never",
            // Inference-recovery poison; never reaches a real (mangled)
            // signature, but the match must stay exhaustive.
            hir::BuiltinTy::Error => "error",
        }),
        TypeExpr::Generic(_, name) | TypeExpr::Infer(_, name) => out.push_str(name.as_str()),
        TypeExpr::Nullable(inner) => {
            encode_into(ws, ty.with_idx(inner), out);
            out.push_str(SEP);
            out.push_str("opt");
        }
        TypeExpr::App { ctor, params } => {
            let append_args = |out: &mut String| {
                for (i, p) in params.into_iter().enumerate() {
                    if i > 0 {
                        out.push_str(SEP);
                    }
                    encode_into(ws, ty.with_idx(p), out);
                }
            };
            match ctor {
                hir::TypeCtor::Array => {
                    out.push_str("arr");
                    if !params.is_empty() {
                        out.push_str(SEP);
                        append_args(out);
                    }
                }
                hir::TypeCtor::Map => {
                    out.push_str("map");
                    out.push_str(SEP);
                    append_args(out);
                }
                hir::TypeCtor::Nominal(name) => {
                    out.push_str(name.as_str());
                    if !params.is_empty() {
                        out.push_str(SEP);
                        append_args(out);
                    }
                }
            }
        }
        TypeExpr::Function { params, ret } => {
            out.push_str("fn");
            // A nullary fn type (`fn() -> X`) drops the empty param segment so the
            // result never contains `__`.
            for p in params {
                out.push_str(SEP);
                encode_into(ws, ty.with_idx(p), out);
            }
            out.push_str(SEP);
            out.push_str("ret");
            out.push_str(SEP);
            encode_into(ws, ty.with_idx(ret), out);
        }
        TypeExpr::Tuple { elements } => {
            let n = elements.into_iter().count();
            out.push_str("tuple");
            out.push_str(&n.to_string());
            for e in elements {
                out.push_str(SEP);
                encode_into(ws, ty.with_idx(e), out);
            }
        }
    }
}

/// Ground-truth structural signature key: kind-tagged so no two distinct type
/// structures share a key (the fixed-token clash `arr` vs a nominal named `arr`
/// is disambiguated by the `arr(` vs `nom:arr(` prefixes). Delimiters `\x1f`/
/// `\x1e` never appear in a Rock identifier. Used only to decide whether two
/// functions with one mangled name are the SAME overload (a backend variant /
/// re-declaration, legitimate) or DISTINCT signatures colliding (rejected).
fn sig_key(ws: &Workspace, f: &Function) -> String {
    let mut s = String::new();
    s.push_str(f.name.as_str());
    for pd in &f.param_decls {
        s.push('\x1f');
        ty_key(ws, pd.ty, &mut s);
    }
    s.push_str("\x1f#ret#\x1f");
    match f.return_ty {
        Some(ty) => ty_key(ws, ty, &mut s),
        None => s.push('u'),
    }
    s
}

fn ty_key(ws: &Workspace, ty: TyId, out: &mut String) {
    let te = match ty {
        TyId::Static(m, idx) => ws.module(m).ty_arena[idx],
        TyId::Local(_) => {
            out.push('L');
            return;
        }
    };
    match te {
        TypeExpr::Builtin(b) => {
            out.push_str("b:");
            out.push_str(&format!("{b:?}"));
        }
        TypeExpr::Generic(_, name) | TypeExpr::Infer(_, name) => {
            out.push_str("g:");
            out.push_str(name.as_str());
        }
        TypeExpr::Nullable(inner) => {
            out.push_str("opt(");
            ty_key(ws, ty.with_idx(inner), out);
            out.push(')');
        }
        TypeExpr::App { ctor, params } => {
            match ctor {
                hir::TypeCtor::Array => out.push_str("arr("),
                hir::TypeCtor::Map => out.push_str("map("),
                hir::TypeCtor::Nominal(name) => {
                    out.push_str("nom:");
                    out.push_str(name.as_str());
                    out.push('(');
                }
            }
            for p in params {
                ty_key(ws, ty.with_idx(p), out);
                out.push('\x1e');
            }
            out.push(')');
        }
        TypeExpr::Function { params, ret } => {
            out.push_str("fn(");
            for p in params {
                ty_key(ws, ty.with_idx(p), out);
                out.push('\x1e');
            }
            out.push_str(")->");
            ty_key(ws, ty.with_idx(ret), out);
        }
        TypeExpr::Tuple { elements } => {
            out.push_str("tup(");
            for e in elements {
                ty_key(ws, ty.with_idx(e), out);
                out.push('\x1e');
            }
            out.push(')');
        }
    }
}

/// The permanent collision check (reject, don't fall back). Returns a spanned
/// error per illegal collision:
///   - two DISTINCT signatures producing the same mangled name, or
///   - a mangled name equal to an exempt function's bare emitted name
/// (so a mangled fn can never shadow, or be shadowed by, an `extern`/`@nomangle`/
/// `main`/`test_*` surface name). Same discipline as `mint_synthetic` /
/// `is_reserved_lexical`: the compiler refuses the program rather than emit a
/// name that would silently resolve to the wrong body. Keyed by mangled name for
/// deterministic ordering across the rayon-scheduled emit.
pub fn collision_errors(ws: &Workspace) -> Vec<(String, CompilerError)> {
    use std::collections::HashMap;
    // mangled name -> (a representative Function, name_span) for a non-exempt fn.
    // `sig_key` (the structural distinctness check) is computed LAZILY -- only
    // when two fns hash to the same mangled name, which the corpus proves rare.
    let mut seen: HashMap<String, (&Function, Span)> = HashMap::new();
    let mut exempt_names: HashMap<String, Span> = HashMap::new();
    let mut out: Vec<(String, CompilerError)> = Vec::new();

    for m in ws.modules.iter() {
        for (_, f) in m.functions.iter() {
            if is_exempt(f) {
                exempt_names.entry(f.name.to_string()).or_insert(f.name_span);
                continue;
            }
            let name = mangle_fn(ws, f);
            match seen.get(&name) {
                Some((prev, _)) if sig_key(ws, prev) != sig_key(ws, f) => {
                    let msg = format!(
                        "mangled name `{name}` is produced by two distinct function signatures; \
                         the emitted symbol would resolve to the wrong body -- rename one of the \
                         overloads (mangling is keyed on name + declared signature)"
                    );
                    out.push((name.clone(), CompilerError::new_with_label(f.name_span, msg)));
                }
                Some(_) => {} // same overload (backend variant / re-decl): fine.
                None => {
                    seen.insert(name, (f, f.name_span));
                }
            }
        }
    }

    // A non-exempt mangled name equal to an exempt bare name.
    for (name, (_, span)) in &seen {
        if exempt_names.contains_key(name) {
            let msg = format!(
                "mangled name `{name}` collides with the bare name of an `extern`/`@nomangle`/\
                 `main`/`test_*` function; the two would share one emitted symbol -- rename the \
                 mangled function"
            );
            out.push((name.clone(), CompilerError::new_with_label(*span, msg)));
        }
    }

    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::encode_mono_type;
    use crate::monoty::{MonoCtor, MonoTy};
    use ustr::Ustr;

    fn nominal(name: &str, params: Vec<MonoTy>) -> MonoTy {
        MonoTy::App { ctor: MonoCtor::Nominal(Ustr::from(name)), params }
    }

    /// Every constructor and composition the encoder can see, spelled out. The
    /// encoding IS the emitted symbol for a specialized body, so any change to
    /// it renames functions across every artifact; these strings are the record
    /// of what the symbols are, independent of which type tree produces them.
    #[test]
    fn mono_type_encoding_is_these_exact_strings() {
        let enc = |ty: &MonoTy| {
            let mut s = String::new();
            encode_mono_type(ty, &mut s);
            s
        };

        assert_eq!(enc(&MonoTy::Int), "p_int");
        assert_eq!(enc(&MonoTy::Float), "p_float");
        assert_eq!(enc(&MonoTy::Bool), "p_bool");
        assert_eq!(enc(&MonoTy::String), "p_string");

        assert_eq!(enc(&MonoTy::Nullable(Box::new(MonoTy::Int))), "o_p_int");
        assert_eq!(
            enc(&MonoTy::Nullable(Box::new(MonoTy::Nullable(Box::new(MonoTy::Int))))),
            "o_o_p_int"
        );

        assert_eq!(enc(&MonoTy::Tuple(vec![])), "t_0_z");
        assert_eq!(enc(&MonoTy::Tuple(vec![MonoTy::Int])), "t_1_p_int_z");
        assert_eq!(
            enc(&MonoTy::Tuple(vec![MonoTy::Int, nominal("Vec2", vec![])])),
            "t_2_p_int_a_n4_Vec2_0_z_z"
        );
        assert_eq!(
            enc(&MonoTy::Tuple(vec![
                MonoTy::Nullable(Box::new(nominal("Vec2", vec![]))),
                MonoTy::Int,
            ])),
            "t_2_o_a_n4_Vec2_0_z_p_int_z"
        );

        assert_eq!(
            enc(&MonoTy::App { ctor: MonoCtor::Array, params: vec![MonoTy::Float] }),
            "a_v_1_p_float_z"
        );
        assert_eq!(
            enc(&MonoTy::App {
                ctor: MonoCtor::Map,
                params: vec![MonoTy::String, MonoTy::Int],
            }),
            "a_m_2_p_string_p_int_z"
        );
        assert_eq!(enc(&nominal("Vec2", vec![])), "a_n4_Vec2_0_z");
        assert_eq!(
            enc(&nominal("Entity", vec![nominal("Mob", vec![])])),
            "a_n6_Entity_1_a_n3_Mob_0_z_z"
        );

        // Totality: no instantiation carries these today, and each still has one
        // symbol of its own rather than colliding with a neighbour.
        assert_eq!(enc(&MonoTy::Any), "p_any");
        assert_eq!(enc(&MonoTy::Unit), "p_unit");
        assert_eq!(enc(&MonoTy::Never), "p_never");
        assert_eq!(
            enc(&MonoTy::Function {
                params: vec![MonoTy::Int],
                ret: Box::new(MonoTy::Float),
            }),
            "f_1_p_int_p_float_z"
        );
        // Argument order and arrow direction are identity, not decoration.
        assert_ne!(
            enc(&MonoTy::Function {
                params: vec![MonoTy::Int],
                ret: Box::new(MonoTy::Float)
            }),
            enc(&MonoTy::Function {
                params: vec![MonoTy::Float],
                ret: Box::new(MonoTy::Int)
            })
        );
    }

    #[test]
    fn mono_type_encoding_preserves_tree_boundaries() {
        let nested = nominal("VecIter", vec![nominal("V2", vec![])]);
        let flat = nominal("VecIter_1_a_n2_V2_0_z", vec![]);
        let mut nested_name = String::new();
        let mut flat_name = String::new();
        encode_mono_type(&nested, &mut nested_name);
        encode_mono_type(&flat, &mut flat_name);

        assert_eq!(nested_name, "a_n7_VecIter_1_a_n2_V2_0_z_z");
        assert_ne!(nested_name, flat_name);
    }
}
