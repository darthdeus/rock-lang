use bone_core::{hir, hir::BodyOwner, workspace::Workspace};
use bone_lower::{LowerCtx, lower_function_def};
use skull_parser::{ItemKind, parse_module_clean};
use ustr::Ustr;

#[test]
fn overloaded_operators_in_a_place_are_lowered_once() {
    let source = r#"fn f() {
    let rows = [[10, 20]]
    let one = 1
    let zero = 0
    rows[0][-one] += rhs()
    rows[0][one + zero] += rhs()
}
"#;
    let ast = parse_module_clean(source, Ustr::from("compound.rock")).unwrap();
    let ItemKind::Function(function) = &ast.items[0].kind else {
        panic!("expected fn f");
    };

    let mut module = hir::Module::default();
    let mut workspace = Workspace::new();
    workspace.fragment_scopes.insert(
        module.file,
        bone_core::modules::FragmentScope {
            package: bone_core::modules::STD_PACKAGE_ID,
            ..Default::default()
        },
    );
    let function_idx = {
        let mut ctx = LowerCtx::new(&workspace, &mut module, &ast, 0);
        lower_function_def(
            &mut ctx,
            function,
            &[],
            function.span.offset_range().0,
            function.name.name,
        )
        .unwrap()
    };
    let body = module
        .bodies
        .get(&BodyOwner::Function(function_idx))
        .expect("lowered body");

    let unary = body
        .expr_arena
        .iter()
        .filter(|(_, expr)| {
            matches!(expr, hir::Expr::UnaryMinus(value, _) if matches!(body.expr_arena[*value], hir::Expr::Var(_)))
        })
        .count();
    let binary = body
        .expr_arena
        .iter()
        .filter(|(_, expr)| match expr {
            hir::Expr::Call {
                callee: hir::Callee::Unresolved { name, .. },
                args,
                family,
                ..
            } if name.as_str() == "__op_add" && family.key().as_str() == "__op_add" => {
                let mut args = *args;
                args.len() == 2 && args.all(|arg| matches!(body.expr_arena[arg], hir::Expr::Var(_)))
            }
            _ => false,
        })
        .count();

    assert_eq!(
        (unary, binary),
        (1, 1),
        "overloaded unary and binary ops should each be captured once"
    );
}
