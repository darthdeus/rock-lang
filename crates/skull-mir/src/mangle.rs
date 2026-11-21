//! Name mangling shared by the Lua, GLSL and JS backends. Mangled names never
//! contain `__`, which GLSL ES reserves.
//!
//! Every function mangles on its own name and declared parameter types except
//! externs with implicit names, `@nomangle`, `@test`, `main` and `test_*`. A function with no
//! parameters gets `_0`, so a mangled name never equals its source name.
//! [`collision_errors_of`] rejects any residual collision; nothing is renamed.

use bone_core::hir::{self, Function, TyId, TypeExpr};
use bone_core::prelude::Span;
use bone_core::workspace::Workspace;
use skull_parser::CompilerError;

const SEP: &str = "_";

/// Derived from [`mangle_fn`] plus the reified type trees, so definition and
/// call sites agree without a side table. An exempt callee still gets a
/// suffix: its bare name belongs to the one body the author owns.
pub(crate) fn spec_fn_name(
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

/// The symbol is the type's canonical identity in symbol spelling: a v1
/// nominal's DefKey is not an identifier, so only its spelling changes.
fn encode_mono_type(ty: &crate::monoty::MonoTy, out: &mut String) {
    ty.write_canonical_as(out, crate::monoty::NominalSpelling::Symbol);
}

/// A v1 definition's name is its DefKey, so the runner entry points read the
/// declared name: `v1pkg::main` is still `main` to the runtime.
pub fn is_exempt(f: &Function) -> bool {
    let name = declared_name(f.name.as_str());
    (f.is_extern && f.lua_extern_path.is_none())
        || has_annotation(f, "nomangle")
        || has_annotation(f, "test")
        || name == "main"
        || name.starts_with("test_")
}

/// Classifiers use this spelling; emitted identifiers use [`ident_of_key`].
pub fn declared_name(key: &str) -> &str {
    match last_path_separator(key) {
        Some(cut) => &key[cut + 2..],
        None => key,
    }
}

fn last_path_separator(key: &str) -> Option<usize> {
    let bytes = key.as_bytes();
    (1..bytes.len())
        .rev()
        .find(|&i| bytes[i] == b':' && bytes[i - 1] == b':')
        .map(|i| i - 1)
}

/// A qualified key becomes `<name>_<digest>`: the prefix is never flattened
/// textually, so module `a::b` and a module named `a_b` cannot converge.
pub fn ident_of_key(key: &str) -> std::borrow::Cow<'_, str> {
    let artifact = bone_core::modules::artifact_name(key);
    if artifact != key {
        return std::borrow::Cow::Borrowed(artifact);
    }
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

pub(crate) fn ident_of_name(name: ustr::Ustr) -> ustr::Ustr {
    IDENT_CACHE.with(|cache| {
        if let Some(ident) = cache.borrow().get(&name) {
            return *ident;
        }
        let ident = match ident_of_key(name.as_str()) {
            std::borrow::Cow::Borrowed(ident) => ustr::Ustr::from(ident),
            std::borrow::Cow::Owned(owned) => ustr::Ustr::from(&owned),
        };
        cache.borrow_mut().insert(name, ident);
        ident
    })
}

/// The compiler's maps stay keyed on the logical DefKey, so use this where a
/// name is written into the artifact, never where one is looked up.
pub fn symbol_of_key(name: ustr::Ustr) -> ustr::Ustr {
    ident_of_name(name)
}

/// A `@nomangle` struct keeps its declared name because inline Lua spells it
/// (cdefs, `ffi.new`, tag compares).
pub fn type_symbol(ws: &Workspace, key: ustr::Ustr) -> ustr::Ustr {
    let exempt = ws
        .nominals
        .struct_id(key)
        .is_some_and(|sid| ws.modules[sid.module.index()].structs[sid.def].nomangle);
    if exempt {
        ustr::Ustr::from(declared_name(key.as_str()))
    } else {
        symbol_of_key(key)
    }
}

/// The interned singleton of a unit variant, reference-equal across uses and
/// usable as a map key.
pub fn enum_singleton_global(ws: &Workspace, key: ustr::Ustr, variant: ustr::Ustr) -> ustr::Ustr {
    ustr::Ustr::from(format!("__rock_enum_{}_{variant}", type_symbol(ws, key)).as_str())
}

/// Indexed by ordinal.
pub fn enum_names_global(ws: &Workspace, key: ustr::Ustr) -> ustr::Ustr {
    ustr::Ustr::from(format!("__rock_enum_{}_names", type_symbol(ws, key)).as_str())
}

/// Hashes the prefix bytes with std's `DefaultHasher`: stable across processes
/// and compiler builds, unlike a `Ustr` hash, whose seed is per-build.
pub fn prefix_digest(prefix: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    prefix.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

fn has_annotation(f: &Function, name: &str) -> bool {
    f.annotations.iter().any(|a| a.is_builtin(name))
}

/// Identical for the definition and every call site, and with or without a
/// backend sibling or another overload, so a hot reload never orphans a global.
pub fn mangle_fn(ws: &Workspace, f: &Function) -> String {
    if is_exempt(f) {
        // The exemption is the surface name; a v1 key carries its module prefix,
        // which none of those consumers know.
        return declared_name(f.name.as_str()).to_string();
    }
    let mut out = String::with_capacity(f.name.len() + 16);
    // Synthetic operators already carry package identity in their parameter types.
    let name = if f.is_synthetic && declared_name(f.name.as_str()).starts_with("__op_") {
        declared_name(f.name.as_str())
    } else {
        ident_of_name(f.name).as_str()
    };
    out.push_str(name);
    for pd in &f.param_decls {
        out.push_str(SEP);
        encode_into(ws, pd.ty, &mut out);
    }
    // The arity marker keeps a parameterless symbol off its source name, which
    // Lua's `_G[name]` would otherwise reach.
    if f.param_decls.is_empty() {
        out.push_str(SEP);
        out.push('0');
    }
    out
}

/// Generic nominal `{name}_{args}`, fn types `fn_{ps}_ret_{r}` (empty `ps`
/// collapses so no `__`), tuples `tupleN_{ps}`.
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
            // Inference-recovery poison never reaches a real signature.
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
                    // A v1 nominal's name is its DefKey; the suffix is an identifier too.
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
            // A nullary fn type drops the empty param segment so the result never
            // contains `__`.
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

fn same_declared_type(ws: &Workspace, a: TyId, b: TyId) -> bool {
    let (TyId::Static(am, ai), TyId::Static(bm, bi)) = (a, b) else {
        return matches!((a, b), (TyId::Local(_), TyId::Local(_)));
    };
    let at = ws.module(am).ty_arena[ai];
    let bt = ws.module(bm).ty_arena[bi];
    match (at, bt) {
        // Backend variants may declare the same parameter name in different arenas.
        (
            TypeExpr::Generic(_, a) | TypeExpr::Infer(_, a),
            TypeExpr::Generic(_, b) | TypeExpr::Infer(_, b),
        ) => a == b,
        _ => at.equivalent(
            bt,
            || false,
            |x, y| same_declared_type(ws, a.with_idx(x), b.with_idx(y)),
        ),
    }
}

fn function_collision_error(
    ws: &Workspace,
    name: &str,
    prev: &Function,
    f: &Function,
) -> Option<CompilerError> {
    let same_parameters = prev.name == f.name
        && prev.param_decls.len() == f.param_decls.len()
        && prev
            .param_decls
            .iter()
            .zip(&f.param_decls)
            .all(|(a, b)| same_declared_type(ws, a.ty, b.ty));
    let msg = if same_parameters {
        let same_return = match (prev.return_ty, f.return_ty) {
            (Some(a), Some(b)) => same_declared_type(ws, a, b),
            (None, None) => true,
            _ => false,
        };
        if same_return {
            return None;
        }
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
    Some(CompilerError::new_with_label(f.name_span, msg).with_label(
        prev.name_span,
        format!("other declaration of `{}`", prev.name),
    ))
}

/// Rejects two distinct signatures with one emitted name, in deterministic order.
pub fn collision_errors_of<'a>(
    ws: &Workspace,
    names: impl IntoIterator<Item = (&'a Function, &'a str)>,
) -> Vec<(String, CompilerError)> {
    use rustc_hash::FxHashMap as HashMap;
    let mut seen: HashMap<&str, &Function> = HashMap::default();
    let mut exempt_names: HashMap<&str, Vec<&Function>> = HashMap::default();
    let mut out: Vec<(String, CompilerError)> = Vec::new();

    for (f, name) in names {
        if is_exempt(f) {
            // The emitted name: comparing a raw v1 DefKey would never match a mangled
            // name and the exempt-collision check would go silent.
            exempt_names.entry(name).or_default().push(f);
            continue;
        }
        if let Some(prev) = seen.get(name) {
            if let Some(error) = function_collision_error(ws, name, prev, f) {
                out.push((name.to_string(), error));
            }
        } else {
            seen.insert(name, f);
        }
    }

    // Two prefixes whose digests coincide would share emitted identifiers, so
    // reject it. Sorted so the reported pair does not depend on map order.
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

    // Two nominal types with one symbol would share a typedef, constructor and
    // runtime tag; only a `@nomangle` struct can reach another type's symbol.
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

    for (name, f) in &seen {
        if let Some(exempt) = exempt_names.get(name) {
            for prev in exempt {
                // Backend variants of one signature remain one logical function, even when
                // the extern variant keeps a bare name.
                if let Some(error) = function_collision_error(ws, name, prev, f) {
                    out.push((name.to_string(), error));
                }
            }
        }
    }

    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    fn named_package_workspace(src: &str) -> bone_core::workspace::Workspace {
        let mut ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("operators.rock"))
            .expect("parse");
        ast.package = Some(skull_parser::PackageTag {
            id: 1,
            self_name: "image_package".to_string(),
            identity_storage: Default::default(),
            root: String::new(),
        });
        bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic pass")
    }

    #[test]
    fn operator_base_symbols_keep_their_declared_identity() {
        let ws = named_package_workspace(
            "newtype Image(int)\noperator +(a: int, b: int) -> int { return a }",
        );
        let functions = &ws.functions_by_name[&ustr::ustr("image_package::__op_add")];
        let add = functions
            .iter()
            .map(|&(module, id)| &ws.module(module).functions[id])
            .collect::<Vec<_>>();
        let synthetic = add
            .iter()
            .find(|f| f.is_synthetic)
            .expect("newtype operator");
        let user = add.iter().find(|f| !f.is_synthetic).expect("user operator");

        let image = super::ident_of_key("image_package::Image");
        let synthetic_symbol = super::mangle_fn(&ws, synthetic);
        let user_symbol = super::mangle_fn(&ws, user);
        assert_eq!(synthetic_symbol, format!("__op_add_{image}_{image}"));
        assert_eq!(
            user_symbol,
            format!("{}_int_int", super::ident_of_name(user.name))
        );
        assert_ne!(synthetic_symbol, user_symbol);
    }

    #[test]
    fn standalone_types_and_root_symbols_keep_their_file_identity() {
        let prefix = bone_core::modules::STANDALONE_PACKAGE_PREFIX;
        let a_name = format!("{prefix}a.rock::P");
        let b_name = format!("{prefix}b.rock::P");
        let a = nominal(&a_name, vec![]);
        let b = nominal(&b_name, vec![]);
        assert_ne!(a.canonical(), b.canonical());
        assert_eq!(a.artifact_key(), nominal("P", vec![]).canonical());
        let child_name = format!("{prefix}a.rock::child::P");
        let child = nominal(&child_name, vec![]);
        assert_eq!(child.artifact_key(), child.canonical());
        let root_name = format!("{prefix}script.rock::helper");
        assert_eq!(super::ident_of_key(&root_name), "helper");
        let nested_name = format!("{prefix}script.rock::child::helper");
        assert_ne!(super::ident_of_key(&nested_name), "helper");
    }

    use super::encode_mono_type;
    use crate::monoty::{MonoCtor, MonoTy};
    use ustr::Ustr;

    fn nominal(name: &str, params: Vec<MonoTy>) -> MonoTy {
        MonoTy::App {
            ctor: MonoCtor::Nominal(Ustr::from(name)),
            params,
        }
    }

    /// The encoding is the emitted symbol of a specialized body, so any change
    /// renames functions across every artifact.
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

        // Totality: each still has one symbol of its own.
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
