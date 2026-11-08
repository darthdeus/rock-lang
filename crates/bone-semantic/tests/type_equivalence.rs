// Workspace::types_equivalent against the cases rendering gets wrong: types
// that render the same but differ, and the same type built in different
// arenas. Every "distinct" case below asserts the renderings collide, so the
// test shows what a rendered-string compare would conclude.

use bone_core::type_equiv::TyRef;
use bone_semantic::prelude::*;

fn workspace(files: &[(&str, &str)]) -> Workspace {
    let mut modules: Vec<_> = files
        .iter()
        .map(|(file, src)| skull_parser::parse_module_clean(src, Ustr::from(file)).expect("parse"))
        .collect();
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic pass")
}

fn function<'a>(ws: &'a Workspace, file: &str, name: &str) -> (ModuleId, &'a hir::Function) {
    let mid = ws.module_ids[&Ustr::from(file)];
    let (_, f) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .unwrap_or_else(|| panic!("fn `{name}` in `{file}`"));
    (mid, f)
}

fn param(ws: &Workspace, file: &str, name: &str) -> TyRef<'static> {
    TyRef::of_static(function(ws, file, name).1.param_decls[0].ty)
}

fn ret(ws: &Workspace, file: &str, name: &str) -> TyRef<'static> {
    TyRef::of_static(function(ws, file, name).1.return_ty.expect("return type"))
}

fn same_render(ws: &Workspace, a: TyRef<'_>, b: TyRef<'_>) -> bool {
    ws.render(a.body, a.ty) == ws.render(b.body, b.ty)
}

#[test]
fn generic_params_of_different_declarations_are_distinct() {
    let ws = workspace(&[(
        "a",
        "fn f<T>(x: T) -> T { x }\n\
         fn g<T>(x: T) -> T { x }",
    )]);
    let (fp, fr, gp) = (
        param(&ws, "a", "f"),
        ret(&ws, "a", "f"),
        param(&ws, "a", "g"),
    );
    assert!(ws.types_equivalent(fp, fr), "one declaration's T is itself");
    assert!(same_render(&ws, fp, gp));
    assert!(!ws.types_equivalent(fp, gp), "f's T is not g's T");
}

#[test]
fn generic_param_is_distinct_from_a_nominal_of_the_same_name() {
    let ws = workspace(&[(
        "a",
        "struct T { x: int }\n\
         fn nominal(x: T) -> int { x.x }\n\
         fn generic<T>(x: T) -> T { x }",
    )]);
    let (n, g) = (param(&ws, "a", "nominal"), param(&ws, "a", "generic"));
    assert!(same_render(&ws, n, g));
    assert!(!ws.types_equivalent(n, g));
    assert!(!ws.types_equivalent(g, n));
}

#[test]
fn generic_params_in_different_modules_are_distinct() {
    // Each module allocates its own generic ids, so the first generic of each
    // module can carry the same id; only the arena tells them apart.
    let ws = workspace(&[
        ("a", "fn f<T>(x: T) -> T { x }"),
        ("b", "fn g<T>(x: T) -> T { x }"),
    ]);
    let (f, g) = (param(&ws, "a", "f"), param(&ws, "b", "g"));
    assert!(same_render(&ws, f, g));
    assert!(!ws.types_equivalent(f, g));
}

#[test]
fn shared_structural_types_are_equivalent_across_modules() {
    // `V2` is declared once; both modules spell `[V2?]` etc. in their own
    // arenas. Module identity is not type identity.
    let ws = workspace(&[
        (
            "a",
            "struct V2 { x: float }\n\
             fn arr(v: [V2?]) {}\n\
             fn map(m: Map<string, (V2, int)>) {}\n\
             fn fun(f: fn(V2) -> int) {}",
        ),
        (
            "b",
            "fn arr(v: [V2?]) {}\n\
             fn map(m: Map<string, (V2, int)>) {}\n\
             fn fun(f: fn(V2) -> int) {}\n\
             fn arr_plain(v: [V2]) {}\n\
             fn fun_ret(f: fn(V2) -> float) {}",
        ),
    ]);
    for name in ["arr", "map", "fun"] {
        let (a, b) = (param(&ws, "a", name), param(&ws, "b", name));
        assert!(ws.types_equivalent(a, b), "`{name}` param across modules");
        assert!(ws.types_equivalent(b, a), "`{name}` param across modules");
    }
    let a_arr = param(&ws, "a", "arr");
    assert!(!ws.types_equivalent(a_arr, param(&ws, "b", "arr_plain")));
    let a_fun = param(&ws, "a", "fun");
    assert!(!ws.types_equivalent(a_fun, param(&ws, "b", "fun_ret")));
}

#[test]
fn body_types_compare_through_their_inference() {
    // A binding's inferred type lives in the body's arena (or behind a solved
    // inference variable) and still equals the declared static type.
    let ws = workspace(&[(
        "a",
        "struct V2 { x: float }\n\
         fn take(v: [V2]) {}\n\
         fn main() {\n\
             let xs = [V2 { x: 1.0 }]\n\
             take(xs)\n\
         }",
    )]);
    let (mid, _) = function(&ws, "a", "main");
    let main_idx = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "main")
        .map(|(idx, _)| idx)
        .unwrap();
    let bi = &ws.inference(mid).unwrap().per_fn[&hir::BodyOwner::Function(main_idx)];
    let body = &ws.module(mid).bodies[&hir::BodyOwner::Function(main_idx)];
    let (b_idx, _) = body
        .binding_arena
        .iter()
        .find(|(_, b)| b.name.as_str() == "xs")
        .unwrap();
    let xs = TyRef::new(Some(bi), bi.binding_types[b_idx]);
    assert!(ws.types_equivalent(xs, param(&ws, "a", "take")));
    assert!(ws.types_equivalent(param(&ws, "a", "take"), xs));
}
