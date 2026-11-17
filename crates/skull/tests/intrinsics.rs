use crate::common::{name_matches, ws_of, ws_with_stdlib};

use skull_codegen_lua::mangle_fn;
use skull_mir::intrinsics::{
    Emission, ExpandKind, IntrinsicEntry, IntrinsicTable, declared_intrinsics, expand_kind,
    is_intrinsic, is_native_required, unbacked_entries,
};

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
            IntrinsicEntry {
                name: "sin",
                arity: 1,
            },
            IntrinsicEntry {
                name: "drifted",
                arity: 2,
            },
        ]
    }
}

#[test]
fn unbacked_entries_flags_only_the_drifted_key() {
    let declared = [(ustr::Ustr::from("sin"), 1usize)].into_iter().collect();
    let un = unbacked_entries(&ToyTable, &declared);
    assert_eq!(un.len(), 1);
    assert_eq!(un[0].name, "drifted");
    assert_eq!(un[0].arity, 2);
}

#[test]
fn unbacked_entries_flags_all_when_nothing_declared() {
    let declared = std::collections::HashSet::default();
    assert_eq!(unbacked_entries(&ToyTable, &declared).len(), 2);
}

fn find_fn<'a>(
    ws: &'a bone_semantic::inference::Workspace,
    name: &str,
) -> &'a bone_semantic::hir::Function {
    ws.modules
        .iter()
        .flat_map(|m| m.functions.iter().map(|(_, f)| f))
        .find(|f| name_matches(f.name, name))
        .unwrap_or_else(|| panic!("no fn `{name}`"))
}

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
        .find(|f| name_matches(f.name, "gid") && !f.generics.is_empty())
        .expect("generic gid");

    let key = mangle_fn(&ws, gid_generic);
    // The declared-signature mangle: generic param encoded as its NAME (`T`),
    // no `int`/`bool` from either call site.
    assert_eq!(key, "gid_T");
    assert!(is_intrinsic(gid_generic));
    assert!(!is_native_required(gid_generic));
    assert!(expand_kind(gid_generic).is_none());
}

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
    assert!(is_intrinsic(upairs));

    assert_eq!(mangle_fn(&ws, upairs), "upairs_T");

    // It is registered as an intrinsic declaration (name, arity=1).
    let declared = declared_intrinsics(&ws);
    assert!(
        declared
            .iter()
            .any(|(name, arity)| name_matches(*name, "upairs") && *arity == 1)
    );
}

#[test]
fn stdlib_layer0_proving_set_is_declared_intrinsic() {
    let ws = ws_with_stdlib("fn main() {}");
    let declared = declared_intrinsics(&ws);
    let has = |n: &str, a: usize| {
        assert!(
            declared
                .iter()
                .any(|(name, arity)| name_matches(*name, n) && *arity == a)
        );
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

fn stdlib_fn<'a>(
    ws: &'a bone_semantic::inference::Workspace,
    name: &str,
) -> &'a bone_semantic::hir::Function {
    ws.modules
        .iter()
        .flat_map(|m| m.functions.iter().map(|(_, f)| f))
        .find(|f| name_matches(f.name, name))
        .unwrap_or_else(|| panic!("no stdlib fn `{name}`"))
}

#[test]
fn stdlib_native_required_set_has_luajit_mapping() {
    let ws = ws_with_stdlib("fn main() {}");
    for n in [
        "table_get",
        "table_set",
        "nb_get",
        "nb_set",
        "__nb_alloc_u8",
        "__nb_alloc_i8",
    ] {
        let f = stdlib_fn(&ws, n);
        assert!(is_native_required(f));
        assert!(is_intrinsic(f));
        assert!(f.inline_body.is_some());
    }
}
