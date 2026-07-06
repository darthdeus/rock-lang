//! Name mangling — the one place a `FuncRef` becomes its emitted Lua name.
//!
//! bone-mir-design (lines 160-164): the MIR `Call` carries a `FuncRef`, NOT a
//! mangled string; mangling is the codegen's job. An *overloaded* name mangles
//! by its declared signature (rock's scheme, `rock-semantic/src/mangle_ast.rs`);
//! the definition and every call site mangle the SAME target so they agree. A
//! uniquely named fn, `@nomangle`/`@test`, `main`, `test_*`, and `extern` keep
//! their surface form.
//!
//! These are pure functions of `(Workspace, Function)`. The dumb printer reads
//! types HERE for name resolution only — never to choose an emission shape (the
//! IR is already typed, D8/P3).

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use bone_semantic::hir::{self, Function, TyId, TypeExpr};
use bone_semantic::inference::Workspace;
use ustr::Ustr;

/// Names with more than one definition across the workspace — the ones that
/// need mangling to disambiguate. A uniquely-named fn keeps its surface name.
pub fn compute_overloaded_names(ws: &Workspace) -> HashSet<Ustr> {
    let mut counts: HashMap<Ustr, usize> = HashMap::default();
    for module in ws.modules.iter() {
        for (_, f) in module.functions.iter() {
            *counts.entry(f.name).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(name, _)| name)
        .collect()
}

/// True if `f` carries the annotation `@<name>` (e.g. `@nomangle`, `@test`).
fn has_annotation(f: &Function, name: &str) -> bool {
    f.annotations
        .iter()
        .any(|a| a.name.name.as_str() == name)
}

/// The emitted Lua name for function `f`. Both the definition's `function <name>`
/// header and a call site to it route through here, so they cannot drift.
pub fn mangle_fn(ws: &Workspace, f: &Function, overloaded: &HashSet<Ustr>) -> String {
    let name = f.name.as_str();
    if !overloaded.contains(&f.name)
        || f.is_extern
        || has_annotation(f, "nomangle")
        || has_annotation(f, "test")
        || name == "main"
        || name.starts_with("test_")
    {
        return name.to_string();
    }

    let mut parts: Vec<String> = f.param_decls.iter().map(|pd| encode_ty(ws, pd.ty)).collect();
    parts.push("ret".to_string());
    parts.push(match f.return_ty {
        Some(ty) => encode_ty(ws, ty),
        None => "unit".to_string(),
    });
    format!("{}__{}", name, parts.join("_"))
}

/// Encode a type into a mangling fragment (mirrors rock's `mangle_ast` `encode`).
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
        TypeExpr::Nullable(inner) => format!("{}_opt", enc(inner)),
        TypeExpr::App { ctor, params } => {
            let args = params.into_iter().map(enc).collect::<Vec<_>>().join("_");
            match ctor {
                hir::TypeCtor::Array => {
                    if args.is_empty() {
                        "arr".to_string()
                    } else {
                        format!("arr_{args}")
                    }
                }
                hir::TypeCtor::Map => format!("map_{args}"),
                hir::TypeCtor::Nominal(name) => {
                    if params.is_empty() {
                        name.to_string()
                    } else {
                        format!("{name}__{args}")
                    }
                }
            }
        }
        TypeExpr::Function { params, ret } => {
            let ps = params.into_iter().map(enc).collect::<Vec<_>>().join("_");
            format!("fn_{}__ret_{}", ps, encode_ty(ws, ty.with_idx(ret)))
        }
        TypeExpr::Tuple { elements } => {
            let n = elements.into_iter().count();
            let ps = elements.into_iter().map(enc).collect::<Vec<_>>().join("_");
            format!("tuple{n}_{ps}")
        }
    }
}
