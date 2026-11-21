//! Declaration checks and production consumers of the JS host supply table.

use crate::common::{stdlib_ws, ws_with};
use skull_codegen_js::{JsIntrinsics, emit::js_supply};
use skull_mir::intrinsics::{declared_intrinsics, unbacked_entries};
use ustr::Ustr;

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
    let js = crate::common::emit_closure(&ws_with(src), "root").unwrap();
    crate::common::run_js(
        &[skull_codegen_js::JS_VALUE_INTEGER_CHECK, "\n", runtime],
        &format!("{js}\n{check}"),
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
