use super::*;
use bone_core::hir_walk::HirChild;

/// Whether control flow appears in a block, branch, match, or loop body.
pub(crate) fn expr_contains_cf(c: BodyCtx, e: Idx<Expr>) -> bool {
    match c.body.expr_arena[e] {
        Expr::Return(_) | Expr::Break | Expr::Continue => true,
        Expr::Block { statements, tail } => {
            statements.into_iter().any(|s| stmt_contains_cf(c, s))
                || tail.is_some_and(|t| expr_contains_cf(c, t))
        }
        Expr::If {
            then_branch,
            else_branch,
            ..
        } => {
            expr_contains_cf(c, then_branch) || else_branch.is_some_and(|b| expr_contains_cf(c, b))
        }
        Expr::Match { arms, .. } => arms
            .into_iter()
            .any(|a| expr_contains_cf(c, c.body.match_arm_arena[a].body)),
        _ => false,
    }
}

pub(crate) fn stmt_contains_cf(c: BodyCtx, s: Idx<Statement>) -> bool {
    match c.body.statement_arena[s] {
        Statement::Return(_) | Statement::Break | Statement::Continue => true,
        Statement::Expr(e) => expr_contains_cf(c, e),
        Statement::For { body, .. } | Statement::While { body, .. } => expr_contains_cf(c, body),
        _ => false,
    }
}

/// Reject control flow inside an `if` used as a destructuring tuple element;
/// its false path cannot produce a value for the destructure.
pub(crate) fn reject_cf_in_destructure_rhs(
    c: BodyCtx,
    value: Idx<Expr>,
) -> Result<(), CompilerError> {
    if let Expr::Tuple(elems) = c.body.expr_arena[value] {
        for elem in elems {
            if matches!(c.body.expr_arena[elem], Expr::If { .. }) && expr_contains_cf(c, elem) {
                return Err(CompilerError::new_with_label(
                    c.body.expr_spans[elem],
                    "control flow (return/break/continue) is not allowed in an `if` used as a \
                     tuple element of a destructuring assignment"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn fn_has_anno(f: &Function, name: &str) -> bool {
    f.annotations.iter().any(|a| a.is_builtin(name))
}

pub(crate) fn reject_captures(
    c: BodyCtx,
    root: Idx<Expr>,
    root_span: Span,
    site: Span,
    static_init: bool,
) -> Result<(), CompilerError> {
    fn visit(
        c: BodyCtx,
        node: HirChild,
        root_span: Span,
        site: Span,
        static_init: bool,
    ) -> Result<(), CompilerError> {
        if let HirChild::Expr(expr) = node
            && let Expr::Var(binding)
            | Expr::Call {
                callee: Callee::Local(binding),
                ..
            } = c.body.expr_arena[expr]
        {
            let binding = &c.body.binding_arena[binding];
            let (start, end) = binding.span.offset_range();
            let (root_start, root_end) = root_span.offset_range();
            if !(root_start <= start && end <= root_end) {
                let message = if static_init {
                    Some(format!(
                        "static initializer may not read local `{}`: the initializer runs once, not per call",
                        binding.name
                    ))
                } else if matches!(binding.kind, BindingKind::Let | BindingKind::Param) {
                    Some(format!(
                        "nested function may not read enclosing local `{}`: it is hoisted to module scope with no access to the enclosing frame",
                        binding.name
                    ))
                } else {
                    None
                };
                if let Some(message) = message {
                    return Err(CompilerError::new_with_label(site, message));
                }
            }
        }
        if matches!(node, HirChild::Pattern(_)) {
            return Ok(());
        }
        c.body
            .try_for_each_child(node, |child| visit(c, child, root_span, site, static_init))
    }
    visit(c, HirChild::Expr(root), root_span, site, static_init)
}

/// Report impure constructs in evaluation order, excluding nested function bodies.
pub(crate) fn verify_pure(
    c: BodyCtx,
    ws: &Workspace,
    resolved: &FxHashMap<Idx<Expr>, (ModuleId, Idx<Function>)>,
    node: HirChild,
    errors: &mut Vec<CompilerError>,
) {
    let (span, message, descend) = match node {
        HirChild::Pattern(_) => return,
        HirChild::Statement(stmt) => {
            let message = match c.body.statement_arena[stmt] {
                Statement::Static { .. } => {
                    Some("`@pure` function declares a `static` (writes module state)")
                }
                Statement::Fn { .. } => Some(
                    "`@pure` function declares a nested `fn` (its body is not vetted for purity)",
                ),
                _ => None,
            };
            (
                c.body.statement_spans[stmt],
                message.map(String::from),
                message.is_none(),
            )
        }
        HirChild::Expr(expr) => {
            let mut descend = true;
            let message = match c.body.expr_arena[expr] {
                Expr::Global(name) if !ws.const_globals.contains(&name) =>
                    Some(format!("`@pure` function reads global `{name}` (mutable state)")),
                Expr::Assign { .. } => Some("`@pure` function performs an assignment (mutation)".into()),
                Expr::CompoundAssign { .. } => Some("`@pure` function performs a compound assignment (mutation)".into()),
                Expr::IncDec { .. } => Some("`@pure` function performs an increment/decrement (mutation)".into()),
                Expr::Call { callee: Callee::Local(_) | Callee::Value(_), .. } =>
                    Some("`@pure` function calls a function value, whose body is not provably pure".into()),
                Expr::Call { .. } | Expr::SafeNavCall { .. } => resolved.get(&expr).and_then(|&(mid, fid)| {
                    let function = &ws.module(mid).functions[fid];
                    (!fn_has_anno(function, "pure") && !fn_has_anno(function, "debug_impure"))
                        .then(|| format!("`@pure` function calls `{}` which is not provably pure", function.name))
                }),
                Expr::Index { .. } => Some("`@pure` function indexes a collection (reads potentially-mutable state)".into()),
                Expr::Array(_) => Some("`@pure` function builds an array literal (allocates a heap table, identity observable)".into()),
                Expr::StructLiteral { name, .. } if !ws.struct_by_name.get(&name)
                    .is_some_and(|sid| ws.module(sid.module).structs[sid.def].is_value) =>
                    Some("`@pure` function constructs a heap struct literal (identity observable); only `value` structs are allowed".into()),
                Expr::Lambda { .. } => {
                    descend = false;
                    Some("`@pure` function creates a lambda (closure identity / captured state not vetted)".into())
                }
                _ => None,
            };
            (c.body.expr_spans[expr], message, descend)
        }
    };
    if let Some(message) = message {
        errors.push(CompilerError::new_with_label(span, message));
    }
    if descend {
        c.body
            .for_each_child(node, |child| verify_pure(c, ws, resolved, child, errors));
    }
}
