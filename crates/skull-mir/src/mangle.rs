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
    let mut parts: Vec<String> = f.param_decls.iter().map(|pd| encode_ty(ws, pd.ty)).collect();
    parts.push("ret".to_string());
    parts.push(match f.return_ty {
        Some(ty) => encode_ty(ws, ty),
        None => "unit".to_string(),
    });
    format!("{}{SEP}{}", f.name.as_str(), parts.join(SEP))
}

/// Encode a type into a mangling fragment. No `__` is ever produced: the
/// separator is `SEP` everywhere (generic nominal `{name}_{args}`, fn types
/// `fn_{ps}_ret_{r}`, tuples `tupleN_{ps}`).
fn encode_ty(ws: &Workspace, ty: TyId) -> String {
    let te = match ty {
        TyId::Static(m, idx) => ws.module(m).ty_arena[idx],
        TyId::Local(_) => return "any".to_string(),
    };
    let enc = |child| encode_ty(ws, ty.with_idx(child));
    match te {
        TypeExpr::Builtin(b) => match b {
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
        }
        .to_string(),
        TypeExpr::Generic(_, name) | TypeExpr::Infer(_, name) => name.to_string(),
        TypeExpr::Nullable(inner) => format!("{}{SEP}opt", enc(inner)),
        TypeExpr::App { ctor, params } => {
            let args = params.into_iter().map(enc).collect::<Vec<_>>().join(SEP);
            match ctor {
                hir::TypeCtor::Array => {
                    if args.is_empty() {
                        "arr".to_string()
                    } else {
                        format!("arr{SEP}{args}")
                    }
                }
                hir::TypeCtor::Map => format!("map{SEP}{args}"),
                hir::TypeCtor::Nominal(name) => {
                    if params.is_empty() {
                        name.to_string()
                    } else {
                        format!("{name}{SEP}{args}")
                    }
                }
            }
        }
        TypeExpr::Function { params, ret } => {
            let r = encode_ty(ws, ty.with_idx(ret));
            let ps = params.into_iter().map(enc).collect::<Vec<_>>().join(SEP);
            // A nullary fn type (`fn() -> X`) has no param segment; joining an
            // empty `ps` with SEP on both sides would produce `fn__ret_X` -- the
            // one `__` this scheme must never emit. Drop the empty segment.
            if ps.is_empty() {
                format!("fn{SEP}ret{SEP}{r}")
            } else {
                format!("fn{SEP}{ps}{SEP}ret{SEP}{r}")
            }
        }
        TypeExpr::Tuple { elements } => {
            let n = elements.into_iter().count();
            let ps = elements.into_iter().map(enc).collect::<Vec<_>>().join(SEP);
            if ps.is_empty() {
                format!("tuple{n}")
            } else {
                format!("tuple{n}{SEP}{ps}")
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
    // mangled name -> (first sig_key, name_span) for a non-exempt fn.
    let mut seen: HashMap<String, (String, Span)> = HashMap::new();
    let mut exempt_names: HashMap<String, Span> = HashMap::new();
    let mut out: Vec<(String, CompilerError)> = Vec::new();

    for m in ws.modules.iter() {
        for (_, f) in m.functions.iter() {
            if is_exempt(f) {
                exempt_names.entry(f.name.to_string()).or_insert(f.name_span);
                continue;
            }
            let name = mangle_fn(ws, f);
            let key = sig_key(ws, f);
            match seen.get(&name) {
                Some((prev_key, _)) if *prev_key != key => {
                    let msg = format!(
                        "mangled name `{name}` is produced by two distinct function signatures; \
                         the emitted symbol would resolve to the wrong body -- rename one of the \
                         overloads (mangling is keyed on name + declared signature)"
                    );
                    out.push((name.clone(), CompilerError::new_with_label(f.name_span, msg)));
                }
                Some(_) => {} // same overload (backend variant / re-decl): fine.
                None => {
                    seen.insert(name, (key, f.name_span));
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
