//! Declaration checks and production consumers of the JS host supply table.

use crate::common::{stdlib_ws, ws_with};
use skull_codegen_js::{JsIntrinsics, emit::js_supply};
use skull_mir::intrinsics::{declared_intrinsics, unbacked_entries};
use ustr::Ustr;

#[test]
fn renamed_host_declaration_leaves_an_unbacked_key() {
    let mut ws = stdlib_ws();
    for module in &mut ws.modules {
        for (_, f) in module.functions.iter_mut() {
            if f.name.as_str() == "phantom" {
                f.name = Ustr::from("renamed_phantom");
            }
        }
    }
    let unbacked = unbacked_entries(&JsIntrinsics(&ws), &declared_intrinsics(&ws));
    assert!(
        unbacked.iter().any(|e| e.name == "phantom" && e.arity == 0),
        "renamed host declaration escaped the intrinsic key check: {unbacked:?}"
    );
}

#[test]
fn same_name_user_declarations_are_not_host_supplies() {
    let ws = ws_with(
        r#"
        fn map_new() -> int { 37 }
        fn tostring(x: int) -> int { x + 5 }
        fn random_seed(x: string) -> string { x }
        inline(lua) fn log_set_level(x: string) = "error('user body')"
        fn root(x: string) -> string { random_seed(x) }
    "#,
    );
    let mid = ws.module_ids[&Ustr::from("<test>")];
    for (_, f) in ws.module(mid).functions.iter() {
        assert!(
            js_supply(&ws, f).is_none(),
            "user `{}` captured by JS supply",
            f.name
        );
    }
    let js = crate::common::emit_closure(&ws, "root").unwrap();
    let out = crate::common::node().arg("-e")
        .arg(format!("{js}\nfor (const input of ['user function', '']) {{ if (root(input) !== input) throw Error('user function changed'); }}"))
        .output().expect("node");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn map_get_key_is_checked_independently_of_native_buffer_get() {
    use bone_core::hir::{TyId, TypeCtor, TypeExpr};
    let mut ws = stdlib_ws();
    let mut map_get = None;
    for (mi, module) in ws.modules.iter().enumerate() {
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() != "get" {
                continue;
            }
            if let Some(pd) = f.param_decls.first() {
                if let TyId::Static(mid, idx) = pd.ty {
                    if matches!(
                        ws.module(mid).ty_arena[idx],
                        TypeExpr::App {
                            ctor: TypeCtor::Map,
                            ..
                        }
                    ) {
                        map_get = Some((mi, fid));
                    }
                }
            }
        }
    }
    let (mi, fid) = map_get.expect("stdlib Map.get declaration");
    let key = skull_mir::mangle::mangle_fn(&ws, &ws.modules[mi].functions[fid]);
    assert!(declared_intrinsics(&ws).contains(&(Ustr::from(&key), 2)));
    ws.modules[mi].functions[fid].name = Ustr::from("renamed_map_get");
    let declared = declared_intrinsics(&ws);
    assert!(
        declared.contains(&(Ustr::from("get"), 2)),
        "NativeBuffer get still declared"
    );
    let unbacked = unbacked_entries(&JsIntrinsics(&ws), &declared);
    assert!(
        unbacked.iter().any(|e| e.name == key && e.arity == 2),
        "Map.get drift was hidden by NativeBuffer get: {unbacked:?}"
    );
}

fn run_root(src: &str, runtime: &str, check: &str) {
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").unwrap();
    let out = crate::common::node()
        .arg("-e")
        .arg(format!("{runtime}\n{js}\n{check}"))
        .output()
        .expect("node");
    assert!(
        out.status.success(),
        "{}\n{js}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn mapped_map_constructor_and_get_preserve_native_key_identity() {
    run_root(
        r#"
        fn root() -> Map<any, string> {
            let m: Map<any, string> = map_new()
            m.insert(1, "numeric")
            m.insert("1", "string")
            assert(m.get(1) != m.get("1"))
            m
        }
    "#,
        &format!(
            "{}{}",
            skull_codegen_js::JS_TABLE_PRIM,
            skull_codegen_js::JS_CALL_HELPER
        ),
        r#"const expected = new Map([[1, "numeric"], ["1", "string"]]);
       const actual = root();
       if (!(actual instanceof Map) || actual.size !== expected.size) throw Error("Map shape");
       for (const [key, value] of expected) if (actual.get(key) !== value) throw Error("key identity");"#,
    );
}

#[test]
fn logging_global_remains_callable_as_a_value() {
    run_root(
        r#"
        fn root(level: int) {
            let set_level: fn(int) = log_set_level
            set_level(level)
        }
    "#,
        &format!(
            "var __G = {{}};\n{}{}",
            skull_codegen_js::JS_CALL_HELPER,
            skull_codegen_js::JS_LOG_RUNTIME
        ),
        "for (const level of [0, 2, 4]) { root(level); if (__G.__rock_log_level !== level) throw Error('callable global'); }",
    );
}

#[test]
fn user_inspection_bodies_bypass_the_host_value_formatter() {
    for (declaration, call) in [
        (
            r#"inline(js) fn inspect(p: UserInspected, cycle_detect: bool = true, precision: int = -1) -> int = "return p.value;""#,
            "inspect(UserInspected { value: input })",
        ),
        (
            r#"inline(js) fn inspect_formatted(p: UserInspected, precision: int) -> int = "return p.value;""#,
            "inspect_formatted(UserInspected { value: input }, 2)",
        ),
    ] {
        let src = format!(
            "value UserInspected {{ value: int }}\n{declaration}\nfn root(input: int) -> int {{ {call} }}"
        );
        run_root(
            &src,
            &format!(
                "{}{}{}",
                skull_codegen_js::JS_CALL_HELPER,
                skull_codegen_js::JS_STRING_RUNTIME,
                skull_codegen_js::JS_INSPECT_RUNTIME
            ),
            "for (const input of [0, 17, -9]) { if (root(input) !== input) throw Error('user inspection body replaced'); }",
        );
    }
}

#[test]
fn weak_cache_retirement_removes_only_the_selected_owner() {
    run_root(
        "fn root(scene: int, arena: int, slot: int) { __weak_cache_retire(scene, arena, slot) }",
        "",
        r#"
root(1, -1, -1);
var __rock_weak_cache_owners = {
  1: { 2: { 3: { a: 1 }, 4: { b: 2 } }, 5: { 6: { c: 3 } } },
  7: { 8: { 9: { d: 4 } } }
};
root(1, 2, 3);
if (__rock_weak_cache_owners[1][2][3] !== undefined ||
    __rock_weak_cache_owners[1][2][4].b !== 2) throw new Error("slot retire");
root(1, 2, 4);
if (__rock_weak_cache_owners[1][2] !== undefined) throw new Error("empty arena");
root(1, 5, -1);
if (__rock_weak_cache_owners[1] !== undefined) throw new Error("empty scene");
root(7, -1, -1);
if (Object.keys(__rock_weak_cache_owners).length !== 0) throw new Error("scene retire");
"#,
    );
}
