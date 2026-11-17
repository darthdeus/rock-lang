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

/// Static initializers cannot capture locals; nested functions cannot capture
/// enclosing locals but may refer to statics and sibling functions.
#[derive(Copy, Clone)]
pub(crate) enum CaptureCtx {
    StaticInit,
    NestedFnBody,
}

pub(crate) fn reject_static_init_captures(
    c: BodyCtx,
    root: Idx<Expr>,
    site: Span,
) -> Result<(), CompilerError> {
    let root_span = c.body.expr_spans[root];
    reject_captures(
        c,
        HirChild::Expr(root),
        root_span,
        site,
        CaptureCtx::StaticInit,
    )
}

/// Check captures using the full declaration span so the nested function's
/// parameters count as local to its body.
pub(crate) fn reject_nested_fn_captures(
    c: BodyCtx,
    body: Idx<Expr>,
    decl_span: Span,
) -> Result<(), CompilerError> {
    reject_captures(
        c,
        HirChild::Expr(body),
        decl_span,
        decl_span,
        CaptureCtx::NestedFnBody,
    )
}

/// Reject references to enclosing locals; declarations inside `root_span` are
/// local to the hoisted construct.
pub(crate) fn reject_captured_binding(
    c: BodyCtx,
    b: Idx<hir::BindingInfo>,
    root_span: Span,
    site: Span,
    mode: CaptureCtx,
) -> Result<(), CompilerError> {
    let bspan = c.body.binding_arena[b].span;
    let inside = bspan.offset_range().0 >= root_span.offset_range().0
        && bspan.offset_range().1 <= root_span.offset_range().1;
    if inside {
        return Ok(());
    }
    let name = c.body.binding_arena[b].name;
    match mode {
        CaptureCtx::StaticInit => comp_error!(
            site,
            "static initializer may not read local `{}`: the initializer runs once, not per call",
            name
        ),
        CaptureCtx::NestedFnBody => {
            if matches!(
                c.body.binding_arena[b].kind,
                hir::BindingKind::Let | hir::BindingKind::Param
            ) {
                comp_error!(
                    site,
                    "nested function may not read enclosing local `{}`: it is hoisted to module scope with no access to the enclosing frame",
                    name
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn reject_captures(
    c: BodyCtx,
    node: HirChild,
    root_span: Span,
    site: Span,
    mode: CaptureCtx,
) -> Result<(), CompilerError> {
    match node {
        HirChild::Expr(e) => match c.body.expr_arena[e] {
            // Calling an enclosing local is a capture; sibling functions are not.
            Expr::Var(b)
            | Expr::Call {
                callee: Callee::Local(b),
                ..
            } => reject_captured_binding(c, b, root_span, site, mode)?,
            _ => {}
        },
        // Nested function bodies are checked separately and lie inside this
        // declaration span.
        HirChild::Statement(_) => {}
        HirChild::Pattern(_) => return Ok(()),
    }
    c.body.try_for_each_child(node, |child| {
        reject_captures(c, child, root_span, site, mode)
    })
}

/// Report impure constructs in `node`; lambda and nested-function bodies are
/// not traversed.
pub(crate) fn verify_pure(
    c: BodyCtx,
    ws: &Workspace,
    resolved: &FxHashMap<Idx<Expr>, (ModuleId, Idx<Function>)>,
    node: HirChild,
    errors: &mut Vec<CompilerError>,
) {
    let descend = match node {
        HirChild::Expr(e) => verify_pure_expr(c, ws, resolved, e, errors),
        HirChild::Statement(s) => verify_pure_stmt(c, s, errors),
        HirChild::Pattern(_) => false,
    };
    if descend {
        c.body
            .for_each_child(node, |child| verify_pure(c, ws, resolved, child, errors));
    }
}

fn verify_pure_expr(
    c: BodyCtx,
    ws: &Workspace,
    resolved: &FxHashMap<Idx<Expr>, (ModuleId, Idx<Function>)>,
    e: Idx<Expr>,
    errors: &mut Vec<CompilerError>,
) -> bool {
    let span = c.body.expr_spans[e];
    let mut reject = |message: String| errors.push(CompilerError::new_with_label(span, message));
    match c.body.expr_arena[e] {
        Expr::Global(name) => {
            // Const globals are read-only; only mutable global reads break purity.
            if !ws.const_globals.contains(&name) {
                reject(format!(
                    "`@pure` function reads global `{}` (mutable state)",
                    name
                ))
            }
        }
        Expr::Assign { .. } => {
            reject("`@pure` function performs an assignment (mutation)".to_string())
        }
        Expr::CompoundAssign { .. } => reject(
            "`@pure` function performs a compound assignment (mutation)".to_string(),
        ),
        Expr::IncDec { .. } => reject(
            "`@pure` function performs an increment/decrement (mutation)".to_string(),
        ),
        // Function values have no declaration whose purity can be checked.
        Expr::Call {
            callee: Callee::Local(_) | Callee::Value(_),
            ..
        } => reject(
            "`@pure` function calls a function value, whose body is not provably pure".to_string(),
        ),
        Expr::Call { .. } | Expr::SafeNavCall { .. } => {
            if let Some(&(mid, fid)) = resolved.get(&e) {
                let callee_fn = &ws.module(mid).functions[fid];
                if !fn_has_anno(callee_fn, "pure") && !fn_has_anno(callee_fn, "debug_impure") {
                    reject(format!(
                        "`@pure` function calls `{}` which is not provably pure",
                        callee_fn.name
                    ));
                }
            }
        }
        Expr::Index { .. } => reject(
            "`@pure` function indexes a collection (reads potentially-mutable state)"
                .to_string(),
        ),
        // Array identity is observable across evaluations; tuples are values.
        Expr::Array(_) => reject(
            "`@pure` function builds an array literal (allocates a heap table, identity observable)"
                .to_string(),
        ),
        // Heap struct identity is observable; value structs are byte-values.
        Expr::StructLiteral { name, .. } => {
            let is_value = ws
                .struct_by_name
                .get(&name)
                .is_some_and(|sid| ws.module(sid.module).structs[sid.def].is_value);
            if !is_value {
                reject(
                    "`@pure` function constructs a heap struct literal (identity observable); only `value` structs are allowed"
                        .to_string(),
                );
            }
        }
        // Lambda identity and captured state are observable; its body is not vetted.
        Expr::Lambda { .. } => {
            reject(
                "`@pure` function creates a lambda (closure identity / captured state not vetted)"
                    .to_string(),
            );
            return false;
        }
        // Match and recursion do not add impurity by themselves.
        _ => {}
    }
    true
}

fn verify_pure_stmt(c: BodyCtx, s: Idx<Statement>, errors: &mut Vec<CompilerError>) -> bool {
    let message = match c.body.statement_arena[s] {
        Statement::Static { .. } => {
            "`@pure` function declares a `static` (writes module state)".to_string()
        }
        // The nested body is checked separately.
        Statement::Fn { .. } => {
            "`@pure` function declares a nested `fn` (its body is not vetted for purity)"
                .to_string()
        }
        _ => return true,
    };
    errors.push(CompilerError::new_with_label(
        c.body.statement_spans[s],
        message,
    ));
    false
}
