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

/// The expression a body binds to its result, if it binds one.
pub(crate) fn result_expr(body: &str, shape: InlineLuaShape) -> Option<&str> {
    match shape {
        InlineLuaShape::Expr => Some(body.trim()),
        InlineLuaShape::UnitStmts => None,
        InlineLuaShape::TailReturn => body
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .and_then(inline_lua_return_rest)
            .filter(|rest| !rest.is_empty()),
    }
}

/// True if `e` is a single call of a named function, which Lua accepts as a
/// statement on its own.
pub(crate) fn is_call_statement(e: &str) -> bool {
    let Some(open) = e.find('(') else {
        return false;
    };
    let callee = &e[..open];
    callee.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && callee
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b':'))
        && one_group(&e[open..])
}

/// Emit the spliced (return-free / tail-return) body, rewriting the tail
/// `return X` to `<lv> = X`, or to the bare call `X` when `lv` is `None`.
/// Param locals are emitted by the caller; this writes only the body. Each
/// line keeps the body's own text (Lua ignores indentation).
pub(crate) fn render_spliced_inline_lua(
    out: &mut String,
    body: &str,
    shape: InlineLuaShape,
    lv: Option<&str>,
) {
    let bind = |out: &mut String, rest: &str| match lv {
        Some(lv) if rest == lv => {}
        Some(lv) => {
            let _ = writeln!(out, "  {lv} = {rest}");
        }
        None => {
            let _ = writeln!(out, "  {rest}");
        }
    };
    match shape {
        InlineLuaShape::Expr => bind(out, body.trim()),
        InlineLuaShape::UnitStmts => {
            for line in body.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = writeln!(out, "  {}", line.trim_end());
            }
        }
        InlineLuaShape::TailReturn => {
            for line in body.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match inline_lua_return_rest(line) {
                    Some("") => {} // bare tail return: nothing to bind (unit)
                    Some(rest) => bind(out, rest),
                    None => {
                        let _ = writeln!(out, "  {}", line.trim_end());
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

/// The byte ranges of the identifiers and keywords in a Lua body, skipping
/// strings, comments and numbers.
fn lua_names(body: &str) -> Vec<(usize, usize)> {
    let b = body.as_bytes();
    // `[`, `=`* and `[` open a long bracket; returns its level and length.
    let long_open = |i: usize| -> Option<(usize, usize)> {
        let level = b[i + 1..].iter().take_while(|&&c| c == b'=').count();
        (b.get(i + 1 + level) == Some(&b'[')).then_some((level, level + 2))
    };
    let long_close = |from: usize, level: usize| -> usize {
        let close = format!("]{}]", "=".repeat(level));
        body[from..]
            .find(&close)
            .map_or(b.len(), |at| from + at + close.len())
    };
    let mut names = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            i += 2;
            match (b.get(i) == Some(&b'[')).then(|| long_open(i)).flatten() {
                Some((level, len)) => i = long_close(i + len, level),
                None => i = body[i..].find('\n').map_or(b.len(), |at| i + at),
            }
        } else if c == b'[' && long_open(i).is_some() {
            let (level, len) = long_open(i).unwrap_or_default();
            i = long_close(i + len, level);
        } else if c == b'"' || c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != c {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            names.push((start, i));
        } else {
            i += 1;
        }
    }
    names
}

/// The identifiers and keywords a Lua body spells.
pub(crate) fn lua_names_of(body: &str) -> Vec<&str> {
    lua_names(body)
        .into_iter()
        .map(|(s, e)| &body[s..e])
        .collect()
}

/// The occurrences of `name` in `body` that refer to a variable: not a field
/// after `.` or `:` (a `..` concatenation still reads it).
fn variable_refs<'a>(
    body: &'a str,
    names: &'a [(usize, usize)],
    name: &'a str,
) -> impl Iterator<Item = (usize, usize)> + 'a {
    names.iter().copied().filter(move |&(s, e)| {
        let before = body[..s].trim_end();
        &body[s..e] == name
            && !(before.ends_with(':') || before.ends_with('.') && !before.ends_with(".."))
    })
}

/// True if `body` only reads parameter `name`: it never assigns, declares or
/// captures it, so a plain local of the caller can stand in for it.
pub(crate) fn reads_param_only(body: &str, name: &str) -> bool {
    let names = lua_names(body);
    let keyword = |w: &str| names.iter().any(|&(s, e)| &body[s..e] == w);
    if keyword("function") || keyword("goto") {
        return false;
    }
    variable_refs(body, &names, name).all(|(s, e)| {
        let before = body[..s].trim_end();
        let after = body[e..].trim_start();
        let assigned = after.starts_with('=') && !after.starts_with("==");
        // `p, q = ...` assigns `p` too; a comma inside a call or constructor
        // does not, so only a statement that opens with a name list counts.
        let line = &before[before.rfind('\n').map_or(0, |at| at + 1)..];
        let listed = after.starts_with(',')
            && !line.contains(['(', '{', '='])
            && !line.trim_start().starts_with("return");
        let declared = ["local", "for"].iter().any(|kw| {
            before
                .strip_suffix(kw)
                .is_some_and(|rest| !rest.ends_with(|c: char| c.is_alphanumeric() || c == '_'))
        }) || line.trim_start().starts_with("for ");
        !(assigned || listed || declared)
    })
}

/// `body` with each variable reference to a parameter in `subs` replaced by
/// its stand-in. A stand-in is a name or a parenthesized expression, so it
/// keeps its meaning in any operand position.
pub(crate) fn substitute_params(body: &str, subs: &[(&str, &str)]) -> String {
    let names = lua_names(body);
    let mut refs: Vec<(usize, usize, &str)> = subs
        .iter()
        .flat_map(|&(name, to)| variable_refs(body, &names, name).map(move |(s, e)| (s, e, to)))
        .collect();
    refs.sort_unstable_by_key(|r| r.0);
    let mut out = String::with_capacity(body.len());
    let mut at = 0;
    for (s, e, to) in refs {
        out.push_str(&body[at..s]);
        out.push_str(to);
        at = e;
    }
    out.push_str(&body[at..]);
    out
}

/// How many times `body` reads parameter `name`, or `None` if a loop may
/// read it more than once per splice.
pub(crate) fn param_reads(body: &str, name: &str) -> Option<usize> {
    let names = lua_names(body);
    if names
        .iter()
        .any(|&(s, e)| matches!(&body[s..e], "for" | "while" | "repeat"))
    {
        return None;
    }
    Some(variable_refs(body, &names, name).count())
}

/// True if `body` reads parameter `name` once, in its first statement, with
/// no call returned, branch taken or assignment made before that read: an
/// expression passed for it reads the same memory there as before the body.
pub(crate) fn reads_first(body: &str, name: &str) -> bool {
    let names = lua_names(body);
    if names.iter().any(|&(s, e)| {
        matches!(
            &body[s..e],
            "for" | "while" | "repeat" | "function" | "goto"
        )
    }) {
        return false;
    }
    let mut refs = variable_refs(body, &names, name);
    let (Some((at, _)), None) = (refs.next(), refs.next()) else {
        return false;
    };
    let start = body.len() - body.trim_start().len();
    if body[start..at].contains(['\n', ')', ';']) {
        return false;
    }
    names
        .iter()
        .filter(|&&(s, _)| start <= s && s < at)
        .all(|&(s, e)| {
            matches!(&body[s..e], "return" | "local" | "if" | "not") || !is_keyword(&body[s..e])
        })
}

/// True if a reference to `name` begins a statement, where a parenthesized
/// stand-in would read as a call of the previous line.
pub(crate) fn opens_statement(body: &str, name: &str) -> bool {
    let names = lua_names(body);
    variable_refs(body, &names, name).any(|(s, _)| {
        let before = body[..s].trim_end_matches([' ', '\t']);
        let word = &before[before
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(0, |at| at + 1)..];
        before.is_empty()
            || before.ends_with(['\n', ';'])
            || matches!(word, "then" | "do" | "else" | "end" | "repeat")
    })
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "and"
            | "break"
            | "do"
            | "else"
            | "elseif"
            | "end"
            | "false"
            | "for"
            | "function"
            | "goto"
            | "if"
            | "in"
            | "local"
            | "nil"
            | "not"
            | "or"
            | "repeat"
            | "return"
            | "then"
            | "true"
            | "until"
            | "while"
    )
}

/// True if `s` is one parenthesized group, so it reads as a single operand
/// wherever it is spelled.
pub(crate) fn one_group(s: &str) -> bool {
    let b = s.as_bytes();
    if b.first() != Some(&b'(') || b.last() != Some(&b')') {
        return false;
    }
    let mut depth = 0usize;
    let mut quote = None;
    let mut i = 0;
    while i < b.len() {
        match (quote, b[i]) {
            (Some(_), b'\\') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, c @ (b'"' | b'\'')) => quote = Some(c),
            (None, b'(') => depth += 1,
            (None, b')') => {
                depth -= 1;
                if depth == 0 && i + 1 < b.len() {
                    return false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    depth == 0
}

/// True if `body` declares or labels anything, so splicing it needs a block
/// to scope its names.
pub(crate) fn declares_names(body: &str) -> bool {
    let names = lua_names(body);
    body.contains("::")
        || names
            .iter()
            .any(|&(s, e)| matches!(&body[s..e], "local" | "for"))
}
