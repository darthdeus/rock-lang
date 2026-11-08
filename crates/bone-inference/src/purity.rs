use super::*;
use bone_core::hir_walk::HirChild;

/// True if `e` reaches a `return`/`break`/`continue` (statement or expr form)
/// through block / if / match / loop bodies. Port of the retired checker's
/// `block_contains_control_flow`.
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
        Statement::For { body, .. }
        | Statement::While { body, .. }
        | Statement::WhileLet { body, .. } => expr_contains_cf(c, body),
        _ => false,
    }
}

/// Reject control flow (`return`/`break`/`continue`) inside an `if` that is a
/// tuple-literal element of a destructure (`let (..)` / newtype) RHS: the
/// cond-false path can't materialise a value to destructure. Port of rock's
/// `ControlFlowInSubexpression` rule.
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

/// Does function `f` carry annotation `@name`?
pub(crate) fn fn_has_anno(f: &Function, name: &str) -> bool {
    f.annotations.iter().any(|a| a.is_builtin(name))
}

/// Verify a `@pure` function body: push an error at each construct that is not
/// referentially transparent -- a global read, an assignment / inc-dec, or a
/// call to a callee that is itself neither `@pure` nor `@debug_impure`.
/// Permissive on everything else (value-struct literals, field reads,
/// operators) so a genuinely-pure body passes. This is the genuine-side-effect
/// subset of rock-optimizations/src/purity.rs; rock's conservative "accepted
/// limitations" (heap struct/array literal, match-bind, ...) are deliberately
/// NOT flagged.
/// A `static` initializer runs ONCE, not per call, so it may not read an
/// enclosing param/local or a sibling static -- that would silently snapshot
/// the first reaching call's values. A `Var` whose declaration span is
/// contained in the initializer (a lambda param, an inner `let`) is local to
/// the init and allowed; one declared outside is the forbidden capture.
/// Which hoisted construct is being checked -- they forbid different capture
/// sets. A static INITIALIZER runs once so it may read nothing declared outside
/// itself (params, locals, and sibling statics all snapshot the first reaching
/// call). A nested fn BODY is hoisted to module scope with no enclosing frame,
/// so it may not read enclosing params/locals -- but it MAY reference in-scope
/// `static`s and sibling nested `fn`s, which are themselves module-level.
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

/// A nested fn's body may not capture the enclosing frame. `body` is the body
/// block expr; `decl_span` spans the whole `fn ..(..) { .. }` statement so the
/// fn's OWN params (declared in the signature, before the body block) count as
/// inside -- only bindings declared outside the declaration are captures.
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

/// Reject a binding reference (a `Var` read or a `Callee::Local` call) that
/// escapes the hoisted construct. A binding whose declaration span is contained
/// in `root_span` is local to the construct (a param, an inner `let`, a lambda
/// param) and always allowed.
pub(crate) fn reject_captured_binding(
    c: BodyCtx,
    b: Idx<hir::BindingInfo>,
    root_span: Span,
    site: Span,
    mode: CaptureCtx,
) -> Result<(), CompilerError> {
    let bspan = c.body.binding_arena[b].span;
    let inside = bspan.offset_range.0 >= root_span.offset_range.0
        && bspan.offset_range.1 <= root_span.offset_range.1;
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
            // Calling an enclosing local (a captured lambda) is a capture;
            // calling a sibling nested fn / in-scope static is not.
            Expr::Var(b)
            | Expr::Call {
                callee: Callee::Local(b),
                ..
            } => reject_captured_binding(c, b, root_span, site, mode)?,
            _ => {}
        },
        // A doubly-nested fn's body sits inside `root_span`, so any
        // enclosing-frame read it makes is caught by ITS own reject pass;
        // walking the body here is harmless (every ref is span-inside
        // `root_span`).
        HirChild::Statement(_) => {}
        // A pattern binds and compares; it reads no enclosing local.
        HirChild::Pattern(_) => return Ok(()),
    }
    c.body.try_for_each_child(node, |child| {
        reject_captures(c, child, root_span, site, mode)
    })
}

/// Push an error at every construct in `node` that is not referentially
/// transparent. Lambda and nested fn bodies are rejected whole rather than
/// vetted, so the walk does not enter them.
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
        // A pattern binds and compares; it has no effect of its own.
        HirChild::Pattern(_) => false,
    };
    if descend {
        c.body
            .for_each_child(node, |child| verify_pure(c, ws, resolved, child, errors));
    }
}

/// The errors `e` itself raises; whether its children still need vetting.
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
            // const globals are read-only (assignment/incdec rejected,
            // re-initialized unconditionally on hot reload); reading one
            // is pure. Only mutable globals break purity.
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
        // Array literals allocate a heap table -- identity observable across
        // re-evaluation, so not dup-safe. Tuples are value-like (no heap
        // identity) -- pure.
        Expr::Array(_) => reject(
            "`@pure` function builds an array literal (allocates a heap table, identity observable)"
                .to_string(),
        ),
        // A `value` struct literal is byte-value (pure); a heap struct literal
        // has observable identity.
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
        // A lambda captures and has closure identity; its body isn't vetted
        // for purity here.
        Expr::Lambda { .. } => {
            reject(
                "`@pure` function creates a lambda (closure identity / captured state not vetted)"
                    .to_string(),
            );
            return false;
        }
        // Match (incl. with-binding) and self-recursion are accepted: they are
        // referentially transparent. rock rejects match-bind / self-recursion
        // as scope-walker limitations; bone implements the intended rule.
        _ => {}
    }
    true
}

/// The errors `s` itself raises; whether its children still need vetting.
fn verify_pure_stmt(c: BodyCtx, s: Idx<Statement>, errors: &mut Vec<CompilerError>) -> bool {
    let message = match c.body.statement_arena[s] {
        Statement::Static { .. } => {
            "`@pure` function declares a `static` (writes module state)".to_string()
        }
        // Conservative, like the lambda case: a nested fn's body is not vetted
        // for purity here. Put the helper at top level if it must be `@pure`.
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
