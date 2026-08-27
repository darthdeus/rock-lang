//! Corpus-wide collision scan for the unified always-mangle scheme (unfuck #65,
//! `docs/mangle-unification-plan.md` step 1).
//!
//! The unified scheme is "today's `encode_ty` fragments, one SEPARATOR, zero
//! `__`". Two separator candidates were on the table: (A) single `_`
//! (`draw_Vec2_ret_unit`, most readable) and (B) `_Z`
//! (`draw_ZVec2_Zret_Zunit`, matches the proven shader shim rewrite). This test
//! mangles EVERY function across stdlib + stone engine + every game + `tests/`
//! under both candidates and reports collisions. A collision is:
//!   - two DISTINCT signatures `(name, param tys, ret ty)` producing one string, OR
//!   - a produced string equal to any exempt fn's bare emitted name.
//! Zero collisions under (A) adopts (A); otherwise (B).
//!
//! `sig_key` is the ground-truth structural signature (kind-tagged, delimiter
//! `\x1f`), so "distinct signature" is decided independently of the candidate
//! being scored — a candidate that maps two distinct `sig_key`s to one string is
//! a real collision, not an artifact of the scoring encoder.

use std::collections::{HashMap, HashSet};

use bone_semantic::hir::{self, Function, TyId, TypeExpr};
use bone_semantic::inference::Workspace;
use ustr::Ustr;

fn is_exempt(f: &Function) -> bool {
    let name = f.name.as_str();
    f.is_extern
        || f.annotations.iter().any(|a| a.name.name.as_str() == "nomangle")
        || f.annotations.iter().any(|a| a.name.name.as_str() == "test")
        || name == "main"
        || name.starts_with("test_")
}

/// The candidate mangle: `encode_ty` fragments joined by `sep`, no `__`.
/// `sep` is the ONLY separator (parameterizing A=`_` vs B=`_Z`).
fn mangle(ws: &Workspace, f: &Function, sep: &str) -> String {
    let mut parts: Vec<String> = f.param_decls.iter().map(|pd| encode_ty(ws, pd.ty, sep)).collect();
    parts.push("ret".to_string());
    parts.push(match f.return_ty {
        Some(ty) => encode_ty(ws, ty, sep),
        None => "unit".to_string(),
    });
    format!("{}{}{}", f.name.as_str(), sep, parts.join(sep))
}

fn encode_ty(ws: &Workspace, ty: TyId, sep: &str) -> String {
    let te = match ty {
        TyId::Static(m, idx) => ws.module(m).ty_arena[idx],
        TyId::Local(_) => return "any".to_string(),
    };
    let enc = |child| encode_ty(ws, ty.with_idx(child), sep);
    match te {
        TypeExpr::Builtin(b) => match b {
            hir::BuiltinTy::Any => "any",
            hir::BuiltinTy::Unit => "unit",
            hir::BuiltinTy::Int => "int",
            hir::BuiltinTy::Uint => "uint",
            hir::BuiltinTy::Float => "float",
            hir::BuiltinTy::Bool => "bool",
            hir::BuiltinTy::String => "string",
            hir::BuiltinTy::Never => "never",
            hir::BuiltinTy::Error => "error",
        }
        .to_string(),
        TypeExpr::Generic(_, name) | TypeExpr::Infer(_, name) => name.to_string(),
        TypeExpr::Nullable(inner) => format!("{}{sep}opt", enc(inner)),
        TypeExpr::App { ctor, params } => {
            let args = params.into_iter().map(enc).collect::<Vec<_>>().join(sep);
            match ctor {
                hir::TypeCtor::Array => {
                    if args.is_empty() {
                        "arr".to_string()
                    } else {
                        format!("arr{sep}{args}")
                    }
                }
                hir::TypeCtor::Map => format!("map{sep}{args}"),
                hir::TypeCtor::Ref => format!("ref{sep}{args}"),
                hir::TypeCtor::Nominal(name) => {
                    if params.is_empty() {
                        name.to_string()
                    } else {
                        format!("{name}{sep}{args}")
                    }
                }
            }
        }
        TypeExpr::Function { params, ret } => {
            let r = encode_ty(ws, ty.with_idx(ret), sep);
            let ps = params.into_iter().map(enc).collect::<Vec<_>>().join(sep);
            if ps.is_empty() {
                format!("fn{sep}ret{sep}{r}")
            } else {
                format!("fn{sep}{ps}{sep}ret{sep}{r}")
            }
        }
        TypeExpr::Tuple { elements } => {
            let n = elements.into_iter().count();
            let ps = elements.into_iter().map(enc).collect::<Vec<_>>().join(sep);
            if ps.is_empty() {
                format!("tuple{n}")
            } else {
                format!("tuple{n}{sep}{ps}")
            }
        }
    }
}

/// Ground-truth structural signature: kind-tagged so NO two distinct type
/// structures share a key (the fixed-token clash `arr` vs a nominal named `arr`
/// is disambiguated by the `a(` vs `n(arr` prefixes). Delimiter `\x1f` never
/// appears in a Rock identifier.
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
        None => s.push_str("u"),
    }
    s
}

fn ty_key(ws: &Workspace, ty: TyId, out: &mut String) {
    let te = match ty {
        TyId::Static(m, idx) => ws.module(m).ty_arena[idx],
        TyId::Local(_) => {
            out.push_str("L");
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
                hir::TypeCtor::Ref => out.push_str("ref("),
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

/// Workspace root (integration tests run with CWD = the crate dir).
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// Load `dir` (relative to the repo root) as a project workspace (its stick.toml
/// deps + stdlib).
fn workspace_for_dir(rel: &str) -> Option<Workspace> {
    let dir = repo_root().join(rel);
    let dir = dir.to_str().unwrap();
    let analysis = match rock_host::compilation_units::analyze_files_in_directory(dir) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("  [{rel}] analyze failed: {e}");
            return None;
        }
    };
    let unit = analysis.project_compilation_unit();
    let sources = match rock_host::compilation_units::load_sources_for_compilation_unit(&unit) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("  [{rel}] load_sources failed: {e}");
            return None;
        }
    };
    let mut modules: Vec<skull_parser::Ast> = rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect();
    for file in &sources.files {
        let f = file.borrow();
        let id = Ustr::from(f.file.to_id_path().as_str());
        match skull_parser::parse_module_clean(&f.contents, id) {
            Ok(m) => modules.push(m),
            Err(e) => {
                eprintln!("  [{dir}] parse failed for {id}: {}", e.message);
                return None;
            }
        }
    }
    match bone_semantic::semantic_passes::semantic_pass(&mut modules) {
        Ok(ws) => Some(ws),
        Err(e) => {
            eprintln!("  [{dir}] semantic_pass returned Err: {}", e.message);
            None
        }
    }
}

/// stdlib alone (the always-linked corpus).
fn workspace_stdlib_only() -> Workspace {
    let mut modules: Vec<skull_parser::Ast> = rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect();
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("stdlib semantic_pass")
}

/// One workspace's collision picture under a given separator. A collision is a
/// within-workspace event only (two different games never co-compile, so a
/// cross-project string clash is not a real collision).
struct WsScan {
    fns: usize,
    /// mangled string -> distinct sig_keys producing it (>1 => a sig-collision).
    a_groups: HashMap<String, HashSet<String>>,
    b_groups: HashMap<String, HashSet<String>>,
    exempt_names: HashSet<String>,
    /// A-scheme mangled names that (illegally) contain `__`.
    a_double_underscore: Vec<String>,
}

fn scan_ws(ws: &Workspace) -> WsScan {
    let mut s = WsScan {
        fns: 0,
        a_groups: HashMap::new(),
        b_groups: HashMap::new(),
        exempt_names: HashSet::new(),
        a_double_underscore: Vec::new(),
    };
    for module in ws.modules.iter() {
        for (_, f) in module.functions.iter() {
            if is_exempt(f) {
                s.exempt_names.insert(f.name.to_string());
                continue;
            }
            s.fns += 1;
            let key = sig_key(ws, f);
            let a = mangle(ws, f, "_");
            // A `__` the SCHEME introduced (empty segment / a `name_` boundary),
            // as opposed to a `__` already in the author's chosen name (the
            // `__rock_*`/`__debug_*` runtime namespace, which the author owns and
            // which only errors if it reaches GLSL -- the __sw_adapt_es assert).
            if a.contains("__") && !f.name.as_str().contains("__") {
                s.a_double_underscore.push(a.clone());
            }
            s.a_groups.entry(a).or_default().insert(key.clone());
            s.b_groups.entry(mangle(ws, f, "_Z")).or_default().insert(key);
        }
    }
    s
}

/// (sig-collision count, exempt-name-collision count) for one group map.
fn collisions(groups: &HashMap<String, HashSet<String>>, exempt: &HashSet<String>) -> (usize, usize) {
    let sig = groups.values().filter(|s| s.len() > 1).count();
    let exempt_hit = groups.keys().filter(|k| exempt.contains(*k)).count();
    (sig, exempt_hit)
}

#[test]
fn corpus_mangle_collision_scan() {
    // (label, workspace). stdlib alone first (the always-linked corpus), then
    // each project loaded as its own compilation unit (stick.toml deps + stdlib).
    let mut workspaces: Vec<(String, Workspace)> = vec![("stdlib(alone)".to_string(), workspace_stdlib_only())];
    let dirs = [
        "stone", "mag", "bitgun", "games/match3", "games/spark", "games/survivors",
        "games/chess", "games/void", "games/vfxlab", "games/fm", "games/sample-game",
    ];
    let mut skipped: Vec<&str> = Vec::new();
    for d in &dirs {
        match workspace_for_dir(d) {
            Some(ws) => workspaces.push((d.to_string(), ws)),
            None => skipped.push(d),
        }
    }

    eprintln!("=== mangle corpus scan (unfuck #65 step 1) ===");
    eprintln!("skipped (load/parse/typecheck failed): {skipped:?}");

    let mut total_fns = 0usize;
    let mut a_sig_total = 0usize;
    let mut a_exempt_total = 0usize;
    let mut b_sig_total = 0usize;
    let mut b_exempt_total = 0usize;
    let mut a_examples: Vec<String> = Vec::new();
    let mut a_du: Vec<String> = Vec::new();

    for (label, ws) in &workspaces {
        let s = scan_ws(ws);
        total_fns += s.fns;
        a_du.extend(s.a_double_underscore.iter().cloned());
        let (a_sig, a_exempt) = collisions(&s.a_groups, &s.exempt_names);
        let (b_sig, b_exempt) = collisions(&s.b_groups, &s.exempt_names);
        a_sig_total += a_sig;
        a_exempt_total += a_exempt;
        b_sig_total += b_sig;
        b_exempt_total += b_exempt;
        eprintln!(
            "  {label:24} fns={:<5} A(sig={a_sig},exempt={a_exempt})  B(sig={b_sig},exempt={b_exempt})",
            s.fns
        );
        for (mangled, keys) in &s.a_groups {
            if keys.len() > 1 {
                a_examples.push(format!("[{label}] A sig-collision `{mangled}` <- {} distinct sigs", keys.len()));
            }
        }
        for k in s.a_groups.keys() {
            if s.exempt_names.contains(k) {
                a_examples.push(format!("[{label}] A `{k}` collides with an exempt bare name"));
            }
        }
    }

    eprintln!("--- totals across {} workspaces ---", workspaces.len());
    eprintln!("non-exempt fns scanned (sum): {total_fns}");
    eprintln!("(A) single '_'  : sig-collisions={a_sig_total}  exempt-name-collisions={a_exempt_total}");
    eprintln!("(B) '_Z'        : sig-collisions={b_sig_total}  exempt-name-collisions={b_exempt_total}");
    for e in a_examples.iter().take(20) {
        eprintln!("  {e}");
    }

    // Every game + stone must have loaded -- a scan that silently skipped them is
    // not the corpus the decision claims to cover.
    assert!(skipped.is_empty(), "corpora failed to load: {skipped:?} (scan is not corpus-wide)");

    // The scheme must be `__`-free by construction (ANGLE/GLSL ES reserves `__`).
    a_du.sort();
    a_du.dedup();
    assert!(
        a_du.is_empty(),
        "(A) produced {} mangled names containing `__` (must be zero): {:?}",
        a_du.len(),
        &a_du[..a_du.len().min(10)]
    );

    // B is the safe fallback and MUST be clean.
    assert_eq!(b_sig_total, 0, "(B) '_Z' must have zero signature collisions in the corpus");
    assert_eq!(b_exempt_total, 0, "(B) '_Z' must not collide with any exempt bare name");

    // DECISION: adopt (A) iff it is clean per-workspace; else (B).
    let adopt_a = a_sig_total == 0 && a_exempt_total == 0;
    eprintln!("DECISION: adopt {}", if adopt_a { "(A) single '_'" } else { "(B) '_Z'" });

    // The adopted scheme is (A) single `_`: the corpus (stdlib + stone + every
    // game, ~23k non-exempt fns) is (A)-clean per compilation unit. If a future
    // corpus edit introduces an (A) collision this pins it RED here (the compiler
    // also rejects it permanently -- see the collision check, unfuck #65 step 4).
    assert!(
        adopt_a,
        "corpus is no longer clean under (A) single '_': sig={a_sig_total} exempt={a_exempt_total}. \
         The shipped scheme is (A); an (A) collision must be resolved (rename the fn) or the \
         scheme reconsidered -- see docs/mangle-unification-plan.md."
    );
}
