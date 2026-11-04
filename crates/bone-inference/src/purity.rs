use super::*;

/// True if `e` reaches a `return`/`break`/`continue` (statement or expr form)
/// through block / if / match / loop bodies. Port of rock's
/// `block_contains_control_flow`
/// (rock-semantic/src/semantic/control_flow_subexpr_validation.rs).
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
    reject_captures_expr(c, root, root_span, site, CaptureCtx::StaticInit)
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
    reject_captures_expr(c, body, decl_span, decl_span, CaptureCtx::NestedFnBody)
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

pub(crate) fn reject_captures_expr(
    c: BodyCtx,
    e: Idx<Expr>,
    root_span: Span,
    site: Span,
    mode: CaptureCtx,
) -> Result<(), CompilerError> {
    macro_rules! go {
        ($x:expr) => {
            reject_captures_expr(c, $x, root_span, site, mode)?
        };
    }
    match c.body.expr_arena[e] {
        Expr::Var(b) => reject_captured_binding(c, b, root_span, site, mode)?,

        Expr::Unit
        | Expr::Null
        | Expr::Hole(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::Break
        | Expr::Continue
        | Expr::Global(_)
        | Expr::SiblingRef(_)
        | Expr::Return(None)
        | Expr::Yield(None) => {}

        Expr::Not(i, _)
        | Expr::BitNot(i)
        | Expr::UnaryMinus(i, _)
        | Expr::Spread(i)
        | Expr::YieldDelegate(i)
        | Expr::Return(Some(i))
        | Expr::Yield(Some(i))
        | Expr::As { value: i, .. }
        | Expr::TupleIndex { tuple: i, .. }
        | Expr::FieldAccess { base: i, .. }
        | Expr::SafeNav { base: i, .. }
        | Expr::IncDec { target: i, .. } => go!(i),

        Expr::Index { base: a, index: b }
        | Expr::Range { low: a, high: b }
        | Expr::BinOp { lhs: a, rhs: b, .. }
        | Expr::Assign {
            target: a,
            value: b,
        }
        | Expr::CompoundAssign {
            target: a,
            value: b,
            ..
        } => {
            go!(a);
            go!(b);
        }

        Expr::Tuple(es) | Expr::Array(es) => {
            for x in es {
                go!(x);
            }
        }

        Expr::InterpolatedString(parts) => {
            for p in parts {
                if let InterpPart::Value { expr, .. } = c.body.interp_part_arena[p] {
                    go!(expr);
                }
            }
        }

        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            go!(cond);
            go!(then_branch);
            if let Some(eb) = else_branch {
                go!(eb);
            }
        }
        Expr::IfLet {
            value,
            then_branch,
            else_branch,
            ..
        } => {
            go!(value);
            go!(then_branch);
            if let Some(eb) = else_branch {
                go!(eb);
            }
        }

        Expr::Call { callee, args, .. } => {
            match callee {
                Callee::Value(ve) => go!(ve),
                // Calling an enclosing local (a captured lambda) is a capture;
                // calling a sibling nested fn / in-scope static is not.
                Callee::Local(b) => reject_captured_binding(c, b, root_span, site, mode)?,
                Callee::Unresolved { .. } => {}
            }
            for a in args {
                go!(a);
            }
        }
        Expr::SafeNavCall { receiver, args, .. } => {
            go!(receiver);
            for a in args {
                go!(a);
            }
        }
        Expr::SafeNavLet { receiver, body, .. } => {
            go!(receiver);
            go!(body);
        }

        Expr::Lambda { body, .. } => go!(body),

        Expr::Block { statements, tail } => {
            for s in statements {
                reject_captures_stmt(c, s, root_span, site, mode)?;
            }
            if let Some(t) = tail {
                go!(t);
            }
        }

        Expr::Match { scrutinee, arms } => {
            go!(scrutinee);
            for a in arms {
                let arm = c.body.match_arm_arena[a];
                if let Some(g) = arm.guard {
                    go!(g);
                }
                go!(arm.body);
            }
        }

        Expr::StructLiteral { fields, bases, .. } => {
            for b in bases {
                go!(b);
            }
            for fld in fields {
                go!(c.body.struct_lit_field_arena[fld].value);
            }
        }

        Expr::EnumVariant { args, .. } => match args {
            VariantArgs::Unit => {}
            VariantArgs::Tuple(es) => {
                for x in es {
                    go!(x);
                }
            }
            VariantArgs::Record(fs) => {
                for fld in fs {
                    go!(c.body.struct_lit_field_arena[fld].value);
                }
            }
        },
    }
    Ok(())
}

pub(crate) fn reject_captures_stmt(
    c: BodyCtx,
    s: Idx<Statement>,
    root_span: Span,
    site: Span,
    mode: CaptureCtx,
) -> Result<(), CompilerError> {
    macro_rules! go {
        ($x:expr) => {
            reject_captures_expr(c, $x, root_span, site, mode)?
        };
    }
    match c.body.statement_arena[s] {
        Statement::Let { value, .. }
        | Statement::LetTuple { value, .. }
        | Statement::LetNewtype { value, .. }
        | Statement::Expr(value)
        | Statement::Defer(value)
        | Statement::Return(Some(value))
        | Statement::Static { value, .. } => go!(value),
        // A doubly-nested fn's body sits inside `root_span`, so any enclosing-frame
        // read it makes is caught by ITS own reject pass; walking the body here is
        // harmless (every ref is span-inside `root_span`).
        Statement::Fn { body, .. } => go!(body),
        Statement::For { iter, body, .. } => {
            go!(iter);
            go!(body);
        }
        Statement::While { cond, body } => {
            go!(cond);
            go!(body);
        }
        Statement::WhileLet { value, body, .. } => {
            go!(value);
            go!(body);
        }
        Statement::Return(None)
        | Statement::Break
        | Statement::Continue
        | Statement::Comment(_) => {}
    }
    Ok(())
}

pub(crate) fn verify_pure_expr(
    c: BodyCtx,
    ws: &Workspace,
    resolved: &FxHashMap<Idx<Expr>, (ModuleId, Idx<Function>)>,
    e: Idx<Expr>,
    errors: &mut Vec<CompilerError>,
) {
    let span = c.body.expr_spans[e];
    macro_rules! go {
        ($x:expr) => {
            verify_pure_expr(c, ws, resolved, $x, errors)
        };
    }
    match c.body.expr_arena[e] {
        Expr::Global(name) => {
            // const globals are read-only (assignment/incdec rejected,
            // re-initialized unconditionally on hot reload); reading one
            // is pure. Only mutable globals break purity.
            if !ws.const_globals.contains(&name) {
                errors.push(CompilerError::new_with_label(
                    span,
                    format!("`@pure` function reads global `{}` (mutable state)", name),
                ))
            }
        }
        Expr::Assign { target, value } => {
            errors.push(CompilerError::new_with_label(
                span,
                "`@pure` function performs an assignment (mutation)".to_string(),
            ));
            go!(target);
            go!(value);
        }
        Expr::CompoundAssign { target, value, .. } => {
            errors.push(CompilerError::new_with_label(
                span,
                "`@pure` function performs a compound assignment (mutation)".to_string(),
            ));
            go!(target);
            go!(value);
        }
        Expr::IncDec { target, .. } => {
            errors.push(CompilerError::new_with_label(
                span,
                "`@pure` function performs an increment/decrement (mutation)".to_string(),
            ));
            go!(target);
        }
        Expr::Call { callee, args, .. } => {
            if let Some(&(mid, fid)) = resolved.get(&e) {
                let callee_fn = &ws.module(mid).functions[fid];
                if !fn_has_anno(callee_fn, "pure") && !fn_has_anno(callee_fn, "debug_impure") {
                    errors.push(CompilerError::new_with_label(
                        span,
                        format!(
                            "`@pure` function calls `{}` which is not provably pure",
                            callee_fn.name
                        ),
                    ));
                }
            }
            if let Callee::Value(ve) = callee {
                go!(ve);
            }
            for a in args {
                go!(a);
            }
        }
        Expr::Block { statements, tail } => {
            for s in statements {
                verify_pure_stmt(c, ws, resolved, s, errors);
            }
            if let Some(t) = tail {
                go!(t);
            }
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            go!(cond);
            go!(then_branch);
            if let Some(eb) = else_branch {
                go!(eb);
            }
        }
        Expr::IfLet {
            value,
            then_branch,
            else_branch,
            ..
        } => {
            go!(value);
            go!(then_branch);
            if let Some(eb) = else_branch {
                go!(eb);
            }
        }
        Expr::Match { scrutinee, arms } => {
            go!(scrutinee);
            for a in arms {
                let arm = c.body.match_arm_arena[a];
                if let Some(g) = arm.guard {
                    go!(g);
                }
                go!(arm.body);
            }
        }
        Expr::BinOp { lhs, rhs, .. } => {
            go!(lhs);
            go!(rhs);
        }
        Expr::Index { base, index } => {
            errors.push(CompilerError::new_with_label(
                span,
                "`@pure` function indexes a collection (reads potentially-mutable state)"
                    .to_string(),
            ));
            go!(base);
            go!(index);
        }
        Expr::Range { low, high } => {
            go!(low);
            go!(high);
        }
        Expr::Not(i, _)
        | Expr::BitNot(i)
        | Expr::UnaryMinus(i, _)
        | Expr::Spread(i)
        | Expr::YieldDelegate(i)
        | Expr::Return(Some(i))
        | Expr::Yield(Some(i))
        | Expr::As { value: i, .. }
        | Expr::TupleIndex { tuple: i, .. }
        | Expr::FieldAccess { base: i, .. }
        | Expr::SafeNav { base: i, .. } => go!(i),
        // Tuples are value-like (no heap identity) -- pure.
        Expr::Tuple(es) => {
            for x in es {
                go!(x);
            }
        }
        // Array literals allocate a heap table -- identity observable across
        // re-evaluation, so not dup-safe.
        Expr::Array(es) => {
            errors.push(CompilerError::new_with_label(
                span,
                "`@pure` function builds an array literal (allocates a heap table, identity observable)"
                    .to_string(),
            ));
            for x in es {
                go!(x);
            }
        }
        // A `value` struct literal is byte-value (pure); a heap struct literal
        // has observable identity.
        Expr::StructLiteral {
            name,
            fields,
            bases,
        } => {
            let is_value = ws
                .struct_by_name
                .get(&name)
                .is_some_and(|sid| ws.module(sid.module).structs[sid.def].is_value);
            if !is_value {
                errors.push(CompilerError::new_with_label(
                    span,
                    "`@pure` function constructs a heap struct literal (identity observable); only `value` structs are allowed"
                        .to_string(),
                ));
            }
            for b in bases {
                go!(b);
            }
            for fld in fields {
                go!(c.body.struct_lit_field_arena[fld].value);
            }
        }
        Expr::EnumVariant { args, .. } => match args {
            VariantArgs::Unit => {}
            VariantArgs::Tuple(es) => {
                for x in es {
                    go!(x);
                }
            }
            VariantArgs::Record(fs) => {
                for fld in fs {
                    go!(c.body.struct_lit_field_arena[fld].value);
                }
            }
        },
        // A lambda captures and has closure identity; its body isn't vetted
        // for purity here.
        Expr::Lambda { .. } => errors.push(CompilerError::new_with_label(
            span,
            "`@pure` function creates a lambda (closure identity / captured state not vetted)"
                .to_string(),
        )),
        // Match (incl. with-binding) and self-recursion are accepted: they are
        // referentially transparent. rock rejects match-bind / self-recursion
        // as scope-walker limitations; bone implements the intended rule.
        // Remaining leaves (interpolation, safe-nav-let/call) carry no
        // pinned side effect.
        _ => {}
    }
}

pub(crate) fn verify_pure_stmt(
    c: BodyCtx,
    ws: &Workspace,
    resolved: &FxHashMap<Idx<Expr>, (ModuleId, Idx<Function>)>,
    s: Idx<Statement>,
    errors: &mut Vec<CompilerError>,
) {
    macro_rules! go {
        ($x:expr) => {
            verify_pure_expr(c, ws, resolved, $x, errors)
        };
    }
    match c.body.statement_arena[s] {
        Statement::Let { value, .. }
        | Statement::LetTuple { value, .. }
        | Statement::LetNewtype { value, .. }
        | Statement::Expr(value)
        | Statement::Defer(value)
        | Statement::Return(Some(value)) => go!(value),
        Statement::For { iter, body, .. } => {
            go!(iter);
            go!(body);
        }
        Statement::While { cond, body } => {
            go!(cond);
            go!(body);
        }
        Statement::WhileLet { value, body, .. } => {
            go!(value);
            go!(body);
        }
        Statement::Static { .. } => {
            errors.push(CompilerError::new_with_label(
                c.body.statement_spans[s],
                "`@pure` function declares a `static` (writes module state)".to_string(),
            ));
        }
        // Conservative, like the lambda case: a nested fn's body is not vetted
        // for purity here. Put the helper at top level if it must be `@pure`.
        Statement::Fn { .. } => {
            errors.push(CompilerError::new_with_label(
                c.body.statement_spans[s],
                "`@pure` function declares a nested `fn` (its body is not vetted for purity)"
                    .to_string(),
            ));
        }
        Statement::Return(None)
        | Statement::Break
        | Statement::Continue
        | Statement::Comment(_) => {}
    }
}
