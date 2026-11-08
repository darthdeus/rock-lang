//! Shapes of inline(lua) bodies and their splicing.

use super::*;

/// Whether a value-producing inst is ALWAYS safe to tree-fold into its single use
/// site, regardless of what executes between def and use: a pure computation over
/// value operands only (operands are SSA — immutable once computed) with no read
/// of mutable state (cell/global/heap) and no side effect. Excludes `Load`/
/// `Global`/`Field`/`Index`/`Len`/`Call` (order-sensitive memory reads / effects)
/// and closure records — those keep their register.
/// The splice-able shape of an `inline(lua)` body (for `@inline` splicing).
#[derive(Clone, Copy)]
pub(crate) enum InlineLuaShape {
    /// A single bare expression (no `return`, no statement keyword) -> the body
    /// IS the rvalue (`<result> = <body>`).
    Expr,
    /// Statements ending in a single TAIL `return X` -> emit the leading
    /// statements verbatim, rewrite the tail to `<result> = X`.
    TailReturn,
    /// Return-free statements (a unit body, e.g. `buf.ptr[i] = v`) -> verbatim,
    /// no result bound.
    UnitStmts,
}

/// `Some(rest)` if `line` (trimmed) is a Lua `return` statement, where `rest` is
/// the returned expression (`""` for a bare `return`). Word-boundary aware so
/// `returned`/`return_x` don't match. `None` otherwise.
fn inline_lua_return_rest(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("return")?;
    match rest.chars().next() {
        None => Some(""),
        Some(c) if !c.is_alphanumeric() && c != '_' => Some(rest.trim()),
        _ => None,
    }
}

/// Classify an `inline(lua)` body for splicing, or `None` if it is not
/// splice-safe (early or multiple `return`s, whose line-by-line rewrite to
/// `<result> = X` would not preserve the control flow -- fall back to a call).
pub(crate) fn classify_inline_lua_body(body: &str) -> Option<InlineLuaShape> {
    let t = body.trim();
    if t.is_empty() {
        return Some(InlineLuaShape::UnitStmts);
    }
    let nonempty: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
    let returns = nonempty
        .iter()
        .filter(|l| inline_lua_return_rest(l).is_some())
        .count();
    if returns == 0 {
        return Some(if is_expr_inline_lua_body(t) {
            InlineLuaShape::Expr
        } else {
            InlineLuaShape::UnitStmts
        });
    }
    // Exactly one return, and it is the LAST non-empty line -> tail return.
    let last_is_return = nonempty
        .last()
        .is_some_and(|l| inline_lua_return_rest(l).is_some());
    if returns == 1 && last_is_return {
        Some(InlineLuaShape::TailReturn)
    } else {
        None
    }
}

/// Emit the spliced (return-free / tail-return) body, rewriting the tail
/// `return X` to `<lv> = X`. Param locals are emitted by the caller; this writes
/// only the body. Each line keeps the body's own text (Lua ignores indentation).
pub(crate) fn render_spliced_inline_lua(
    out: &mut String,
    body: &str,
    shape: InlineLuaShape,
    lv: &str,
) {
    match shape {
        InlineLuaShape::Expr => {
            let _ = writeln!(out, "    {lv} = {}", body.trim());
        }
        InlineLuaShape::UnitStmts => {
            for line in body.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = writeln!(out, "    {}", line.trim_end());
            }
        }
        InlineLuaShape::TailReturn => {
            for line in body.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match inline_lua_return_rest(line) {
                    Some("") => {} // bare tail return: nothing to bind (unit)
                    Some(rest) => {
                        let _ = writeln!(out, "    {lv} = {rest}");
                    }
                    None => {
                        let _ = writeln!(out, "    {}", line.trim_end());
                    }
                }
            }
        }
    }
}

/// Is this (return-free) `inline(lua)` body a single bare expression (so it can
/// be the RHS of `lv = …`), vs a statement / multi-statement unit body? Any
/// statement-keyword start, a top-level `=` assignment, `;`, or a second
/// non-blank line => stmts.
fn is_expr_inline_lua_body(s: &str) -> bool {
    let t = s.trim();
    let mut lines = t.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false; // multi-line => statements
    }
    let f = first.trim_start();
    for kw in [
        "local ", "if ", "for ", "while ", "do ", "return", "::", "repeat", "break", "goto ",
    ] {
        if f.starts_with(kw) {
            return false;
        }
    }
    if f.contains(';') {
        return false;
    }
    // A top-level (paren-depth 0) `=` that is not ==/~=/<=/>= is an assignment.
    let b = f.as_bytes();
    let mut depth: i32 = 0;
    for i in 0..b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'=' if depth == 0 => {
                let prev = if i > 0 { b[i - 1] } else { 0 };
                let next = if i + 1 < b.len() { b[i + 1] } else { 0 };
                let is_cmp = next == b'=' || matches!(prev, b'=' | b'~' | b'<' | b'>' | b'!');
                if !is_cmp {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}
