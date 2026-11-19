use crate::ast::*;
use rock_core::Span;
use ustr::Ustr;

pub fn desugar(ast: &mut Ast) {
    synth_newtype_operators(ast);
    synth_json_default_factories(ast);
}

fn synth_newtype_operators(ast: &mut Ast) {
    let int_specs: &[(&str, &str, bool)] = &[
        ("__op_add", "a + b", true),
        ("__op_sub", "a - b", true),
        ("__op_mul", "a * b", true),
        // int newtypes get `./` (floor) and NOT `/` (consistent with core
        // ints post-float-div-migration; a newtype wanting float `/` unwraps
        // first). Float newtypes keep `/` in float_specs below.
        ("__op_idiv", "math.floor(a / b)", true),
        ("__op_mod", "a % b", true),
        ("__op_lt", "a < b", false),
        ("__op_le", "a <= b", false),
        ("__op_gt", "a > b", false),
        ("__op_ge", "a >= b", false),
    ];
    let float_specs: &[(&str, &str, bool)] = &[
        ("__op_add", "a + b", true),
        ("__op_sub", "a - b", true),
        ("__op_mul", "a * b", true),
        ("__op_div", "a / b", true),
        ("__op_mod", "a % b", true),
        ("__op_lt", "a < b", false),
        ("__op_le", "a <= b", false),
        ("__op_gt", "a > b", false),
        ("__op_ge", "a >= b", false),
    ];
    let uint_specs: &[(&str, &str, bool)] = &[
        ("__op_add", "(a + b) % 4294967296", true),
        ("__op_sub", "(a - b) % 4294967296", true),
        (
            "__op_mul",
            "local al = a % 65536\nlocal ah = math.floor(a / 65536)\nlocal bl = b % 65536\nlocal bh = math.floor(b / 65536)\nreturn (al * bl + ((al * bh + ah * bl) % 65536) * 65536) % 4294967296",
            true,
        ),
        ("__op_idiv", "math.floor(a / b)", true),
        ("__op_mod", "a % b", true),
        ("__op_lt", "a < b", false),
        ("__op_le", "a <= b", false),
        ("__op_gt", "a > b", false),
        ("__op_ge", "a >= b", false),
    ];
    let string_specs: &[(&str, &str, bool)] = &[("__op_add", "a .. b", true)];

    let mut targets: Vec<(Ustr, Span, &[(&str, &str, bool)])> = Vec::new();
    for item in &ast.items {
        let ItemKind::Newtype(nt) = &item.kind else {
            continue;
        };
        if !nt.generics.is_empty() {
            continue;
        }
        let specs = match ast.types[nt.wrapped].kind {
            TypeExprKind::Int => int_specs,
            TypeExprKind::Uint => uint_specs,
            TypeExprKind::Float | TypeExprKind::F64 => float_specs,
            TypeExprKind::String => string_specs,
            _ => continue,
        };
        targets.push((nt.name.name, nt.span, specs));
    }

    for (nt_name, sp, specs) in targets {
        for &(stem, inner, ret_self) in specs {
            let pa_ty = ast.ty(
                sp,
                TypeExprKind::Ident(Ident {
                    span: sp,
                    name: nt_name,
                }),
                false,
            );
            let pb_ty = ast.ty(
                sp,
                TypeExprKind::Ident(Ident {
                    span: sp,
                    name: nt_name,
                }),
                false,
            );
            let return_ty = if ret_self {
                ast.ty(
                    sp,
                    TypeExprKind::Ident(Ident {
                        span: sp,
                        name: nt_name,
                    }),
                    false,
                )
            } else {
                ast.ty(sp, TypeExprKind::Bool, false)
            };
            let mk_param = |ty, nm: &str| Param {
                span: sp,
                name: Ident {
                    span: sp,
                    name: Ustr::from(nm),
                },
                ty,
                default: None,
                is_variadic: false,
                annotations: Vec::new(),
            };
            let mk_anno = |nm: &str| Annotation {
                span: sp,
                name: Ident {
                    span: sp,
                    name: Ustr::from(nm),
                },
                args: Vec::new(),
            };
            ast.items.push(Item {
                annotations: vec![mk_anno("pure"), mk_anno("inline")],
                vis: None,
                span: sp,
                kind: ItemKind::Function(FunctionDef {
                    span: sp,
                    keyword_span: None,
                    name: Ident {
                        span: sp,
                        name: Ustr::from(stem),
                    },
                    generics: Vec::new(),
                    params: vec![mk_param(pa_ty, "a"), mk_param(pb_ty, "b")],
                    params_span: sp,
                    return_ty: Some(return_ty),
                    body: FunctionBody::Inline(
                        FnBackend::Lua,
                        Ustr::from(format!("return {inner}").as_str()),
                    ),
                    is_macro: false,
                    is_coroutine: false,
                    is_synthetic: true,
                    json_field_for: None,
                    macro_items: Vec::new(),
                    receiver_surface: None,
                }),
            });
        }
    }
}

fn synth_json_default_factories(ast: &mut Ast) {
    if !ast.items.iter().any(|item| {
        matches!(&item.kind, ItemKind::Struct(s)
        if s.fields.iter().any(|field| field.annotations.iter().any(|a|
            matches!(a.name.name.as_str(), "json_default" | "json_skip" | "json_omit_if"))))
    }) {
        return;
    }
    let existing: Vec<_> = ast
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Function(f) => f.json_field_for.clone(),
            _ => None,
        })
        .collect();
    let items = std::mem::take(&mut ast.items);
    for item in items {
        let mut factories = Vec::new();
        if let ItemKind::Struct(s) = &item.kind {
            for field in &s.fields {
                let marked = field.annotations.iter().any(|a| {
                    matches!(
                        a.name.name.as_str(),
                        "json_default" | "json_skip" | "json_omit_if"
                    )
                });
                if !marked || !s.generics.is_empty() {
                    continue;
                }
                let Some(default) = field.default else {
                    continue;
                };
                let predicate = field
                    .annotations
                    .iter()
                    .find(|a| a.name.name.as_str() == "json_omit_if")
                    .and_then(|a| a.args.first())
                    .map(|a| a.value);
                for operation in [JsonFieldOperation::Default, JsonFieldOperation::Omit] {
                    if operation == JsonFieldOperation::Omit && predicate.is_none() {
                        continue;
                    }
                    if existing.iter().any(|(owner, name, op)| {
                        owner == &s.name && *name == field.name.name && *op == operation
                    }) {
                        continue;
                    }
                    factories.push(json_field_factory(
                        ast, s, field, default, predicate, operation,
                    ));
                }
            }
        }
        ast.items.push(item);
        ast.items.extend(factories);
    }
}

fn json_field_factory(
    ast: &mut Ast,
    owner: &StructDef,
    field: &Field,
    default: Idx<ExprData>,
    predicate: Option<Idx<ExprData>>,
    operation: JsonFieldOperation,
) -> Item {
    let sp = field.name.span;
    let local = Ident {
        span: sp,
        name: Ustr::from("value"),
    };
    let any = ast.ty(sp, TypeExprKind::Any, false);
    let field_ty = if matches!(ast.types[field.ty].kind, TypeExprKind::F32) {
        ast.ty(sp, TypeExprKind::Float, false)
    } else {
        field.ty
    };
    let (params, statements, return_ty, stem) = match operation {
        JsonFieldOperation::Default => {
            let bind = ast.stmt(
                sp,
                StatementKind::Let(Let {
                    span: sp,
                    keyword_span: None,
                    pattern: Pattern::Ident(local.clone()),
                    ty: Some(field_ty),
                    value: Some(default),
                    else_block: None,
                }),
            );
            let value = ast.expr(sp, ExprKind::Ident(local));
            let ret = ast.stmt(
                sp,
                StatementKind::Return {
                    keyword_span: None,
                    value: Some(value),
                },
            );
            (Vec::new(), vec![bind, ret], any, "default")
        }
        JsonFieldOperation::Omit => {
            let param = Param {
                span: sp,
                name: local.clone(),
                ty: any,
                default: None,
                is_variadic: false,
                annotations: Vec::new(),
            };
            let value = ast.expr(sp, ExprKind::Ident(local));
            let cast = ast.expr(
                sp,
                ExprKind::Ident(Ident {
                    span: sp,
                    name: Ustr::from("cast"),
                }),
            );
            let typed = ast.expr(
                sp,
                ExprKind::FunctionCall {
                    func: cast,
                    generic_args: vec![field_ty],
                    args: vec![FunctionCallArg::positional(value)],
                    syntax: CallSyntax::Normal,
                },
            );
            let call = ast.expr(
                sp,
                ExprKind::FunctionCall {
                    func: predicate.expect("omission factory requires a predicate"),
                    generic_args: Vec::new(),
                    args: vec![FunctionCallArg::positional(typed)],
                    syntax: CallSyntax::Normal,
                },
            );
            let ret = ast.stmt(
                sp,
                StatementKind::Return {
                    keyword_span: None,
                    value: Some(call),
                },
            );
            let boolean = ast.ty(sp, TypeExprKind::Bool, false);
            (vec![param], vec![ret], boolean, "omit")
        }
    };
    let block = ast.block(sp, None, statements);
    Item {
        annotations: Vec::new(),
        vis: None,
        span: sp,
        kind: ItemKind::Function(FunctionDef {
            span: sp,
            keyword_span: None,
            name: Ident {
                span: sp,
                name: Ustr::from(&format!(
                    "__json_{stem}_{}_{}_{}",
                    owner.name.name.len(),
                    owner.name.name,
                    field.name.name
                )),
            },
            generics: Vec::new(),
            params,
            params_span: sp,
            return_ty: Some(return_ty),
            body: FunctionBody::Rock(block),
            is_macro: false,
            is_coroutine: false,
            is_synthetic: true,
            json_field_for: Some((owner.name.clone(), field.name.name, operation)),
            macro_items: Vec::new(),
            receiver_surface: None,
        }),
    }
}
