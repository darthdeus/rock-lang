//! The `@intrinsic` mechanism (docs/intrinsics-plan.md, slice 1): the fail-closed
//! key check catches a backend-table key that no `@intrinsic` declaration backs,
//! and the generic-key pin fixes what a generic intrinsic's stable registry key
//! is (the declared-signature mangle, NOT a monomorphization -- there is none).
use crate::common::{ws_of, ws_with_stdlib};

use skull_codegen_lua::mangle_fn;
use skull_mir::intrinsics::{
    declared_intrinsics, expand_kind, is_intrinsic, is_native_required, unbacked_entries, Emission,
    ExpandKind, IntrinsicEntry, IntrinsicTable,
};

/// A stand-in backend table: two entries, one of which no declaration backs.
struct ToyTable;
impl IntrinsicTable for ToyTable {
    fn backend(&self) -> &'static str {
        "toy"
    }
    fn lookup(&self, _f: &bone_semantic::hir::Function) -> Option<Emission> {
        None
    }
    fn entries(&self) -> Vec<IntrinsicEntry> {
        vec![
            IntrinsicEntry { name: "sin", arity: 1 },
            IntrinsicEntry { name: "drifted", arity: 2 },
        ]
    }
}

// Fail-closed: an entry whose (name, arity) matches no `@intrinsic` declaration
// is surfaced (a test would go red), while a backed entry is not -- pure data,
// no Workspace, so the checker's algorithm is exercised on its own.
#[test]
fn unbacked_entries_flags_only_the_drifted_key() {
    let declared = [(ustr::Ustr::from("sin"), 1usize)].into_iter().collect();
    let un = unbacked_entries(&ToyTable, &declared);
    assert_eq!(un.len(), 1, "exactly the unbacked entry, got: {un:?}");
    assert_eq!(un[0].name, "drifted");
    assert_eq!(un[0].arity, 2);
}

// An empty declaration set makes every entry unbacked (the fail-closed default:
// with nothing declared, no key is legitimate).
#[test]
fn unbacked_entries_flags_all_when_nothing_declared() {
    let declared = std::collections::HashSet::default();
    assert_eq!(unbacked_entries(&ToyTable, &declared).len(), 2);
}

/// Find the workspace `Function` named `name` (first match).
fn find_fn<'a>(ws: &'a bone_semantic::inference::Workspace, name: &str) -> &'a bone_semantic::hir::Function {
    ws.modules
        .iter()
        .flat_map(|m| m.functions.iter().map(|(_, f)| f))
        .find(|f| f.name.as_str() == name)
        .unwrap_or_else(|| panic!("no fn `{name}`"))
}

// The generic-key answer (plan: "verify EARLY"). A generic intrinsic is NOT
// monomorphized; its stable registry key is the DECLARED-SIGNATURE mangle,
// computed off the one declared `Function` (a FuncRef), so it cannot depend on
// any call site's `T`. `gid` is overloaded (two defs) to force the mangle path:
// `mangle_fn` encodes the generic parameter as its NAME (`T`), never a concrete
// instantiation -- proving the key is call-site-`T`-independent even though the
// two call sites below instantiate `T` at `int` and at `bool`.
const GENERIC: &str = r#"
@intrinsic fn gid<T>(x: T) -> T { return x }
@intrinsic fn gid(a: int, b: int) -> int { return a }

fn use_both() -> int {
    let p = gid(7)
    let q = gid(true)
    if q { return p } else { return 0 }
}
"#;

#[test]
fn generic_intrinsic_key_is_declared_signature_mangle() {
    let ws = ws_of(GENERIC);

    let gid_generic = ws
        .modules
        .iter()
        .flat_map(|m| m.functions.iter().map(|(_, f)| f))
        .find(|f| f.name.as_str() == "gid" && !f.generics.is_empty())
        .expect("generic gid");

    let key = mangle_fn(&ws, gid_generic);
    // The declared-signature mangle: generic param encoded as its NAME (`T`),
    // no `int`/`bool` from either call site.
    assert_eq!(key, "gid_T_ret_T", "generic intrinsic key must be the declared-signature mangle");
    assert!(is_intrinsic(gid_generic));
    assert!(!is_native_required(gid_generic));
    assert!(expand_kind(gid_generic).is_none());
}

// `@struct_expand` is recognized as the `StructPairs` expansion intrinsic
// through the ONE registry recognizer, and a generic `@struct_expand` fn's key
// is likewise its declared mangle (always-mangle: even a unique name mangles).
const EXPAND: &str = r#"
value V2 { x: float, y: float }
enum U { F(float), Vv(V2) }

@struct_expand inline(lua) fn upairs<T>(u: T) -> [(string, U)] = """
    error("phantom")
"""
"#;

#[test]
fn struct_expand_is_an_expansion_intrinsic() {
    let ws = ws_of(EXPAND);
    let upairs = find_fn(&ws, "upairs");
    assert_eq!(expand_kind(upairs), Some(ExpandKind::StructPairs));
    assert!(is_intrinsic(upairs), "an expansion intrinsic is an intrinsic");

    assert_eq!(
        mangle_fn(&ws, upairs),
        "upairs_T_ret_arr_tuple2_string_U",
        "always-mangle: the declared-signature mangle (generic param as `T`)"
    );

    // It is registered as an intrinsic declaration (name, arity=1).
    let declared = declared_intrinsics(&ws);
    assert!(declared.contains(&(ustr::Ustr::from("upairs"), 1)));
}

// Slice 3: the Layer-0 proving set is annotated `@intrinsic` in the stdlib, so
// `declared_intrinsics` sees it -- the declaration side of the contract the toy
// JS table (slice 4) is checked against with `unbacked_entries`.
#[test]
fn stdlib_layer0_proving_set_is_declared_intrinsic() {
    let ws = ws_with_stdlib("fn main() {}");
    let declared = declared_intrinsics(&ws);
    let has = |n: &str, a: usize| {
        assert!(declared.contains(&(ustr::Ustr::from(n), a)), "`{n}`/{a} not declared @intrinsic");
    };
    // Math passthroughs (luajit mapping = inline(lua) body).
    for n in ["sin", "cos", "sqrt", "floor", "ceil"] {
        has(n, 1);
    }
    has("pow", 2);
    has("atan2", 2);
    has("abs", 1); // float overload
    // Math with a Rock fallback body + a backend native mapping.
    has("min", 2);
    has("max", 2);
    has("clamp", 3);
    has("lerp", 3);
    has("smoothstep", 3);
    has("fract", 1);
    // Untyped indexing + NativeBuffer primitives + a StringBuilder op.
    has("table_get", 2);
    has("table_set", 3);
    has("nb_get", 2);
    has("nb_set", 3);
    has("__nb_alloc_u8", 1);
    has("sb_new", 0);
}

/// A stdlib fn by name (first match), or panic.
fn stdlib_fn<'a>(ws: &'a bone_semantic::inference::Workspace, name: &str) -> &'a bone_semantic::hir::Function {
    ws.modules
        .iter()
        .flat_map(|m| m.functions.iter().map(|(_, f)| f))
        .find(|f| f.name.as_str() == name)
        .unwrap_or_else(|| panic!("no stdlib fn `{name}`"))
}

// The native-required subset is marked `@native_required` AND has a luajit
// mapping (an `inline(lua)` body) -- the body-slot invariant `intrinsic_rejection`
// enforces on user code, held by the stdlib itself.
#[test]
fn stdlib_native_required_set_has_luajit_mapping() {
    let ws = ws_with_stdlib("fn main() {}");
    for n in ["table_get", "table_set", "nb_get", "nb_set", "__nb_alloc_u8", "__nb_alloc_i8"] {
        let f = stdlib_fn(&ws, n);
        assert!(is_native_required(f), "`{n}` must be @native_required");
        assert!(is_intrinsic(f), "`{n}` must be @intrinsic");
        assert!(f.inline_body.is_some(), "`{n}` (native-required) must have an inline(lua) luajit mapping");
    }
}
