//! Name mangling -- the one place a function becomes its emitted symbol, shared
//! by the Lua, GLSL, and JS backends.
//!
//! The scheme is ALWAYS-MANGLE, keyed on the function's own `(name, declared
//! parameter types)` ONLY -- decl-set-independent, position-independent, and
//! containing NO `__` anywhere (legal in the Lua, JS, and GLSL ES identifier
//! grammars alike). Every function mangles except the verbatim exemptions:
//! `extern`, `@nomangle`, `@test`, `main`, `test_*` (deliberate surface names --
//! `extern(lua)` IS the Lua builtin; `@nomangle` is an explicit shadow the author
//! owns; `main`/`test_*`/`@test` are runner entry points). A function with no
//! parameters gets the arity marker `_0` (`foo()` -> `foo_0`), so a mangled name
//! never equals its source name.
//!
//! The separator is a single `_`; `rock-cli/tests/mangle_corpus_scan.rs`
//! checks it for collisions across a program's modules.
//! [`collision_errors`] enforces the property permanently: any residual
//! collision (two distinct signatures mangling alike, or a mangled name equal to
//! an exempt bare name) is rejected, never silently renamed.

use bone_core::hir::{self, Function, TyId, TypeExpr};
use bone_core::prelude::Span;
use bone_core::workspace::Workspace;
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
pub fn spec_fn_name(ws: &Workspace, f: &Function, key: &[crate::monoty::MonoTy]) -> Option<String> {
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
///
/// It is written in the SYMBOL spelling: this text lands in an identifier, and
/// a v1 nominal's canonical name is its DefKey, which is not one. The identity
/// keeps the key; only the emitted spelling of a nominal changes, exactly as
/// [`encode_into`] already does for the signature suffix.
fn encode_mono_type(ty: &crate::monoty::MonoTy, out: &mut String) {
    ty.write_canonical_as(out, crate::monoty::NominalSpelling::Symbol);
}

/// True if `f` keeps its bare surface name (never mangled).
///
/// A v1 definition's `name` is its DefKey, so the runner-entry-point tests
/// (`main`, `test_*`) read the DECLARED name out of the key rather than the key
/// itself -- `v1pkg::main` is still `main` to the runtime.
pub fn is_exempt(f: &Function) -> bool {
    let name = declared_name(f.name.as_str());
    f.is_extern
        || has_annotation(f, "nomangle")
        || has_annotation(f, "test")
        || name == "main"
        || name.starts_with("test_")
}

/// The declared (last) segment of a DefKey. A legacy key IS the declared name.
/// Classifiers use this spelling; emitted identifiers use [`ident_of_key`]
/// instead.
pub fn declared_name(key: &str) -> &str {
    match last_path_separator(key) {
        Some(cut) => &key[cut + 2..],
        None => key,
    }
}

/// Byte offset of the last `::` in `key`. A plain backward byte scan: the
/// general substring searcher this replaces set itself up anew for every
/// call, and classifiers ask this of every callee a body names.
fn last_path_separator(key: &str) -> Option<usize> {
    let bytes = key.as_bytes();
    (1..bytes.len())
        .rev()
        .find(|&i| bytes[i] == b':' && bytes[i - 1] == b':')
        .map(|i| i - 1)
}

/// The identifier form of a DefKey. A legacy key is a bare name and
/// passes through byte-identical, so legacy emission cannot move. A v1 key
/// `<package>::<module path>::<name>` becomes `<name>_<digest>`: `::` is not in
/// any identifier grammar, and the prefix is never flattened textually, so
/// module `a::b` and a module literally named `a_b` cannot converge.
///
/// The separator is a single `_` because this module's scheme forbids `__`
/// ANYWHERE -- it is reserved in the GLSL ES identifier grammar, which the
/// shared mangler must also satisfy.
/// [`collision_errors`] rejects a residual collision either way.
pub fn ident_of_key(key: &str) -> std::borrow::Cow<'_, str> {
    match last_path_separator(key) {
        None => std::borrow::Cow::Borrowed(key),
        Some(cut) => std::borrow::Cow::Owned(format!(
            "{}{SEP}{}",
            &key[cut + 2..],
            prefix_digest(&key[..cut])
        )),
    }
}

thread_local! {
    static IDENT_CACHE: std::cell::RefCell<rustc_hash::FxHashMap<ustr::Ustr, ustr::Ustr>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// [`ident_of_key`] for an interned name, memoized per thread. The ident is a
/// pure function of the name, and every call site of a function mangles its
/// callee's name again, so the digest and formatting run once per name per
/// worker instead of once per reference.
pub fn ident_of_name(name: ustr::Ustr) -> ustr::Ustr {
    IDENT_CACHE.with(|cache| {
        if let Some(ident) = cache.borrow().get(&name) {
            return *ident;
        }
        let ident = match ident_of_key(name.as_str()) {
            std::borrow::Cow::Borrowed(_) => name,
            std::borrow::Cow::Owned(owned) => ustr::Ustr::from(&owned),
        };
        cache.borrow_mut().insert(name, ident);
        ident
    })
}

/// [`ident_of_key`] for a name that is used as an emitted symbol: the Lua global
/// a definition occupies, the `_G` key the state bridge installs under it, and
/// the identifiers minted from it. The compiler's own maps stay keyed on the
/// LOGICAL name (the DefKey), so this belongs where a name is WRITTEN into the
/// artifact, never where one is looked up.
pub fn symbol_of_key(name: ustr::Ustr) -> ustr::Ustr {
    ident_of_name(name)
}

/// The emitted symbol of the nominal type keyed `key`: its C typedef, `__ty`
/// tag, `<T>_ct` constructor and every registry key minted from it. A
/// `@nomangle` struct keeps its declared name, because inline Lua spells it
/// (cdefs, `ffi.new`, tag compares); every other type is [`symbol_of_key`].
/// [`collision_errors`] rejects two types that would share a symbol.
pub fn type_symbol(ws: &Workspace, key: ustr::Ustr) -> ustr::Ustr {
    let exempt = ws
        .struct_by_name
        .get(&key)
        .is_some_and(|sid| ws.modules[sid.module.index()].structs[sid.def].nomangle);
    if exempt {
        ustr::Ustr::from(declared_name(key.as_str()))
    } else {
        symbol_of_key(key)
    }
}

/// The digest of a DefKey's `<package>::<module path>` prefix. Hashes the
/// prefix BYTES with std's `DefaultHasher` (the `StaticCell.init_hash`
/// precedent): stable across processes AND across compiler builds, unlike a
/// `Ustr` hash, whose seed is per-build.
pub fn prefix_digest(prefix: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    prefix.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

fn has_annotation(f: &Function, name: &str) -> bool {
    f.annotations.iter().any(|a| a.is_builtin(name))
}

/// The emitted name for function `f`, identical for its definition and every
/// call site (so they cannot drift), identical with or without a backend sibling
/// or another overload elsewhere (so a hot reload never orphans a running
/// global).
pub fn mangle_fn(ws: &Workspace, f: &Function) -> String {
    if is_exempt(f) {
        // The exemption IS the surface name: `main`, a `test_*` runner entry,
        // an `extern` foreign symbol, an author-owned `@nomangle`. A v1 key
        // carries its module prefix, which none of those consumers know.
        return declared_name(f.name.as_str()).to_string();
    }
    // Append directly into one buffer -- no per-fragment `String`/`Vec` (this is
    // on the per-call-site codegen path; the old collect+join allocated N+2
    // strings + a vec per reference).
    let mut out = String::with_capacity(f.name.len() + 16);
    out.push_str(ident_of_name(f.name).as_str());
    for pd in &f.param_decls {
        out.push_str(SEP);
        encode_into(ws, pd.ty, &mut out);
    }
    // The arity marker keeps a parameterless symbol off its source name, which
    // Lua's `_G[name]` would otherwise reach (ZERO_ARG_MANGLING).
    if f.param_decls.is_empty() {
        out.push_str(SEP);
        out.push('0');
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
            hir::BuiltinTy::Uint => "uint",
            hir::BuiltinTy::Float => "float",
            hir::BuiltinTy::F32 => "f32",
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
                hir::TypeCtor::Ref => {
                    out.push_str("ref");
                    out.push_str(SEP);
                    append_args(out);
                }
                hir::TypeCtor::Nominal(name) => {
                    // A v1 nominal's name is its DefKey; the signature suffix is
                    // an identifier too, so it takes the same form.
                    out.push_str(ident_of_name(name).as_str());
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

fn function_collision_error(
    ws: &Workspace,
    name: &str,
    prev: &Function,
    f: &Function,
) -> CompilerError {
    let prev_signature = sig_key(ws, prev);
    let signature = sig_key(ws, f);
    let msg = if prev_signature.split("\x1f#ret#\x1f").next()
        == signature.split("\x1f#ret#\x1f").next()
    {
        format!(
            "functions `{}` and `{}` differ only in return type; both would emit as \
             `{name}` -- rename one of the functions (return types do not distinguish overloads)",
            prev.name, f.name,
        )
    } else {
        format!(
            "mangled name `{name}` is produced by two distinct function signatures; \
             the emitted symbol would resolve to the wrong body -- rename one of the \
             overloads (mangling is keyed on name + declared parameter types)"
        )
    };
    CompilerError::new_with_label(f.name_span, msg).with_label(
        prev.name_span,
        format!("other declaration of `{}`", prev.name),
    )
}

/// The permanent collision check (reject, don't fall back). Returns a spanned
/// error per illegal collision:
///   - two DISTINCT signatures producing the same mangled name, or
///   - a mangled name equal to a distinct exempt signature's bare emitted name
///     (so a mangled fn can never shadow, or be shadowed by, an `extern`/`@nomangle`/
///     `main`/`test_*` surface name).
///
/// Same discipline as `mint_synthetic` /
/// `is_reserved_lexical`: the compiler refuses the program rather than emit a
/// name that would silently resolve to the wrong body. Keyed by mangled name for
/// deterministic ordering across the rayon-scheduled emit.
pub fn collision_errors(ws: &Workspace) -> Vec<(String, CompilerError)> {
    use rustc_hash::FxHashMap as HashMap;
    // mangled name -> (a representative Function, name_span) for a non-exempt fn.
    // `sig_key` (the structural distinctness check) is computed LAZILY -- only
    // when two fns hash to the same mangled name, which the corpus proves rare.
    let mut seen: HashMap<String, (&Function, Span)> = HashMap::default();
    let mut exempt_names: HashMap<String, Vec<&Function>> = HashMap::default();
    let mut out: Vec<(String, CompilerError)> = Vec::new();

    for m in ws.modules.iter() {
        for (_, f) in m.functions.iter() {
            if is_exempt(f) {
                // The EMITTED name, which for a v1 key is its declared segment
                // -- comparing the raw DefKey here would never match a mangled
                // name and the exempt-collision check would go silent.
                exempt_names.entry(mangle_fn(ws, f)).or_default().push(f);
                continue;
            }
            let name = mangle_fn(ws, f);
            match seen.get(&name) {
                Some((prev, _)) if sig_key(ws, prev) != sig_key(ws, f) => {
                    out.push((name.clone(), function_collision_error(ws, &name, prev, f)));
                }
                Some(_) => {} // same overload (backend variant / re-decl): fine.
                None => {
                    seen.insert(name, (f, f.name_span));
                }
            }
        }
    }

    // Two distinct `<package>::<module path>` prefixes whose digests coincide
    // would hand their definitions the same emitted identifier. Plan 2.1: reject
    // it, never silently rename. One pass over `ws.defs` covers callables, types
    // and globals; sorted so the reported pair does not depend on map order.
    let mut by_prefix: HashMap<&str, Vec<Span>> = HashMap::default();
    for (key, meta) in &ws.defs {
        if let Some(cut) = key.as_str().rfind("::") {
            by_prefix
                .entry(&key.as_str()[..cut])
                .or_default()
                .push(meta.span);
        }
    }
    let mut prefixes: Vec<(&str, Vec<Span>)> = by_prefix.into_iter().collect();
    prefixes.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut by_digest: HashMap<String, &str> = HashMap::default();
    for (prefix, spans) in &prefixes {
        let digest = prefix_digest(prefix);
        match by_digest.get(digest.as_str()) {
            // Prefixes are distinct here, so a seen digest is a collision.
            Some(prev) => {
                for &span in spans {
                    let msg = format!(
                        "module path digest `{digest}` is produced by both `{prev}` and `{prefix}`;                      their definitions would share emitted identifiers -- rename one of the modules"
                    );
                    out.push((digest.clone(), CompilerError::new_with_label(span, msg)));
                }
            }
            None => {
                by_digest.insert(digest, prefix);
            }
        }
    }

    // Two nominal types with one emitted symbol would share a typedef, a
    // constructor and a runtime tag. Only a `@nomangle` struct can reach
    // another type's symbol: its declared name against a legacy type or
    // another package's `@nomangle` struct of that name.
    let mut type_keys: Vec<(ustr::Ustr, Span)> = Vec::new();
    for m in ws.modules.iter() {
        type_keys.extend(m.structs.iter().map(|(_, d)| (d.name, d.name_span)));
        type_keys.extend(m.enums.iter().map(|(_, d)| (d.name, d.name_span)));
        type_keys.extend(m.newtypes.iter().map(|(_, d)| (d.name, d.name_span)));
    }
    type_keys.sort_by_key(|a| a.0);
    type_keys.dedup_by(|a, b| a.0 == b.0);
    let mut type_owner: HashMap<ustr::Ustr, ustr::Ustr> = HashMap::default();
    for (key, span) in &type_keys {
        let symbol = type_symbol(ws, *key);
        match type_owner.get(&symbol) {
            Some(prev) => {
                let msg = format!(
                    "types `{prev}` and `{key}` would both emit as `{symbol}`; \
                     a `@nomangle` struct owns its declared name -- rename one of them"
                );
                out.push((
                    symbol.to_string(),
                    CompilerError::new_with_label(*span, msg),
                ));
            }
            None => {
                type_owner.insert(symbol, *key);
            }
        }
    }

    // A non-exempt mangled name equal to an exempt bare name.
    for (name, (f, _)) in &seen {
        if let Some(exempt) = exempt_names.get(name) {
            for prev in exempt {
                // Backend variants of one signature remain one logical function,
                // even when the extern variant keeps a bare name.
                if sig_key(ws, prev) != sig_key(ws, f) {
                    out.push((name.clone(), function_collision_error(ws, name, prev, f)));
                }
            }
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
        MonoTy::App {
            ctor: MonoCtor::Nominal(Ustr::from(name)),
            params,
        }
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
            enc(&MonoTy::Nullable(Box::new(MonoTy::Nullable(Box::new(
                MonoTy::Int
            ))))),
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
            enc(&MonoTy::App {
                ctor: MonoCtor::Array,
                params: vec![MonoTy::Float]
            }),
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
