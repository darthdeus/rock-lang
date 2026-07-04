//! A Wadler/Prettier-style document IR for layout.
//!
//! The formatter builds a `Doc` describing structure (text, breakable lines,
//! groups, indentation) and `render` turns it into the final string by deciding,
//! per `group`, whether it fits the target width in *flat* mode or must *break*.
//! Width-driven wrapping is therefore declarative and idempotent by
//! construction -- the renderer never re-measures already-emitted text.
//!
//! This module is layout only. Comments, blank lines and verbatim string/lua
//! bodies are handled by the caller (verbatim text goes in via `Doc::text`).

/// A line break that adapts to its enclosing group's mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// Space when flat, newline when broken.
    Normal,
    /// Nothing when flat, newline when broken.
    Soft,
    /// Always a newline; forces every enclosing group to break.
    Hard,
}

#[derive(Clone, Debug)]
pub enum Doc {
    Nil,
    Text(String),
    Line(LineKind),
    Concat(Vec<Doc>),
    /// Increase indentation by one unit for the contained doc's line breaks.
    Indent(Box<Doc>),
    /// A unit that the renderer lays out flat if it fits, otherwise broken.
    Group(Box<Doc>),
    /// `(broken, flat)` — renders the first doc when the enclosing group breaks,
    /// the second when it stays flat. Used for trailing commas (`,` only when
    /// multi-line).
    #[allow(dead_code)]
    IfBreak(Box<Doc>, Box<Doc>),
    /// First-fit alternatives: render the first option *all* of whose lines fit
    /// the width at the current column; fall back to the last option otherwise.
    /// Unlike `Group` (which is a binary flat/break choice over one layout), this
    /// picks between distinct layouts -- used for the fluid assignment layout
    /// (`lhs = rhs` | `lhs =\n  rhs` | `lhs = rhs(\n  args\n)`).
    Choice(Vec<Doc>),
}

impl Doc {
    pub fn text(s: impl Into<String>) -> Doc {
        Doc::Text(s.into())
    }

    pub fn concat(docs: impl IntoIterator<Item = Doc>) -> Doc {
        Doc::Concat(docs.into_iter().collect())
    }

    pub fn indent(d: Doc) -> Doc {
        Doc::Indent(Box::new(d))
    }

    pub fn group(d: Doc) -> Doc {
        Doc::Group(Box::new(d))
    }

    // Wadler `if_break` primitive. Currently unused -- the formatter preserves
    // the author's source line layout for collections/calls rather than emitting
    // width-conditional trailing commas -- but kept as a renderer building block.
    #[allow(dead_code)]
    pub fn if_break(broken: Doc, flat: Doc) -> Doc {
        Doc::IfBreak(Box::new(broken), Box::new(flat))
    }

    pub fn choice(options: impl IntoIterator<Item = Doc>) -> Doc {
        Doc::Choice(options.into_iter().collect())
    }

    /// Join `docs` with `sep` between each pair.
    pub fn join(sep: Doc, docs: impl IntoIterator<Item = Doc>) -> Doc {
        let mut out = Vec::new();
        for (i, d) in docs.into_iter().enumerate() {
            if i > 0 {
                out.push(sep.clone());
            }
            out.push(d);
        }
        Doc::Concat(out)
    }

    /// True if this doc contains a hard line, which forces breaking regardless
    /// of width.
    fn has_hard_line(&self) -> bool {
        match self {
            Doc::Line(LineKind::Hard) => true,
            Doc::Text(_) | Doc::Nil | Doc::Line(_) => false,
            Doc::Concat(ds) => ds.iter().any(Doc::has_hard_line),
            Doc::Indent(d) | Doc::Group(d) => d.has_hard_line(),
            // The flat branch is what matters when probing whether a group can
            // stay flat; the broken branch never appears in flat mode.
            Doc::IfBreak(_, flat) => flat.has_hard_line(),
            // A choice is only entered in break mode and resolves to one option;
            // for flat-probing it stands in for its first (flattest) option.
            Doc::Choice(opts) => opts.first().is_some_and(Doc::has_hard_line),
        }
    }
}

/// Convenience line-break constructors.
pub fn line() -> Doc {
    Doc::Line(LineKind::Normal)
}
pub fn softline() -> Doc {
    Doc::Line(LineKind::Soft)
}
pub fn hardline() -> Doc {
    Doc::Line(LineKind::Hard)
}
pub fn nil() -> Doc {
    Doc::Nil
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
}

/// Does the remaining work fit in `width` columns on the current line?
///
/// `width` is the columns left before the limit. Only content up to the next
/// (break-mode) newline matters. A hard line in flat content makes it not fit.
fn fits(mut width: isize, mut stack: Vec<(usize, Mode, &Doc)>) -> bool {
    while width >= 0 {
        let Some((indent, mode, doc)) = stack.pop() else {
            return true;
        };
        match doc {
            Doc::Nil => {}
            Doc::Text(s) => width -= s.chars().count() as isize,
            Doc::Line(kind) => match mode {
                Mode::Flat => match kind {
                    LineKind::Normal => width -= 1,
                    LineKind::Soft => {}
                    LineKind::Hard => return false,
                },
                Mode::Break => return true,
            },
            Doc::Concat(ds) => {
                for d in ds.iter().rev() {
                    stack.push((indent, mode, d));
                }
            }
            Doc::Indent(d) => stack.push((indent + 1, mode, d)),
            // An inner group that must break (it contains a hard line) ends the
            // current line at its first break, so probe it in Break mode; a
            // breakless inner group is assumed flat. This lets `head() @ [<multi-
            // line>]` keep the head flat (the array breaks) while still breaking
            // `(params) => long_body` (the breakless body overflows the line).
            Doc::Group(d) => {
                let m = if d.has_hard_line() { Mode::Break } else { Mode::Flat };
                stack.push((indent, m, d));
            }
            Doc::IfBreak(broken, flat) => {
                let chosen = if mode == Mode::Break { broken } else { flat };
                stack.push((indent, mode, chosen));
            }
            // A choice nested inside a flat-probe stands in for its first option.
            Doc::Choice(opts) => {
                if let Some(first) = opts.first() {
                    stack.push((indent, mode, first));
                }
            }
        }
    }
    false
}

/// Whether every line of `doc`, rendered standalone from `start_col` at indent
/// level `indent`, fits within `width`. Groups are measured flat; hard lines
/// reset the column to the indent. Used to pick a `Choice` option.
fn all_lines_fit(
    doc: &Doc,
    indent: usize,
    start_col: usize,
    width: usize,
    indent_unit: usize,
) -> bool {
    let mut col = start_col;
    let mut stack: Vec<(usize, &Doc)> = vec![(indent, doc)];
    while let Some((ind, d)) = stack.pop() {
        match d {
            Doc::Nil => {}
            Doc::Text(s) => {
                for (i, seg) in s.split('\n').enumerate() {
                    if i > 0 {
                        col = ind * indent_unit;
                    }
                    col += seg.chars().count();
                    if col > width {
                        return false;
                    }
                }
            }
            Doc::Line(LineKind::Hard) => col = ind * indent_unit,
            Doc::Line(LineKind::Normal) => col += 1,
            Doc::Line(LineKind::Soft) => {}
            Doc::Concat(ds) => {
                for sub in ds.iter().rev() {
                    stack.push((ind, sub));
                }
            }
            Doc::Indent(sub) => stack.push((ind + 1, sub)),
            // Measure groups and choices in their flattest form.
            Doc::Group(sub) => stack.push((ind, sub)),
            Doc::IfBreak(_, flat) => stack.push((ind, flat)),
            Doc::Choice(opts) => {
                if let Some(first) = opts.first() {
                    stack.push((ind, first));
                }
            }
        }
        if col > width {
            return false;
        }
    }
    true
}

/// Render `doc` to a string. `width` is the target max line length;
/// `indent_unit` is spaces per indentation level.
pub fn render(doc: &Doc, width: usize, indent_unit: usize) -> String {
    let mut out = String::new();
    let mut col: usize = 0;
    // (indent level, mode, doc). Process LIFO.
    let mut stack: Vec<(usize, Mode, &Doc)> = vec![(0, Mode::Break, doc)];

    while let Some((indent, mode, d)) = stack.pop() {
        match d {
            Doc::Nil => {}
            Doc::Text(s) => {
                out.push_str(s);
                // Text may contain verbatim newlines (e.g. multiline strings);
                // track the column from the last line so width stays accurate.
                match s.rfind('\n') {
                    Some(pos) => col = s[pos + 1..].chars().count(),
                    None => col += s.chars().count(),
                }
            }
            Doc::Line(kind) => match mode {
                Mode::Flat => match kind {
                    LineKind::Normal => {
                        out.push(' ');
                        col += 1;
                    }
                    LineKind::Soft => {}
                    // A hard line breaks even in flat mode (groups with a hard
                    // line never get picked as flat, but be safe).
                    LineKind::Hard => {
                        out.push('\n');
                        let pad = indent * indent_unit;
                        out.extend(std::iter::repeat_n(' ', pad));
                        col = pad;
                    }
                },
                Mode::Break => {
                    // Trim trailing spaces on the line we are leaving.
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push('\n');
                    let pad = indent * indent_unit;
                    out.extend(std::iter::repeat_n(' ', pad));
                    col = pad;
                }
            },
            Doc::Concat(ds) => {
                for sub in ds.iter().rev() {
                    stack.push((indent, mode, sub));
                }
            }
            Doc::Indent(sub) => stack.push((indent + 1, mode, sub)),
            Doc::Group(sub) => {
                // A group lays out flat iff the group AND the rest of the
                // current line (the continuation already on the stack, in their
                // decided modes) fit before the next break. Measuring the
                // continuation is what makes `(params) => long_body` break its
                // params: the params alone fit, but params-plus-body do not.
                let mut probe = stack.clone();
                probe.push((indent, Mode::Flat, sub));
                let flat_fits =
                    !sub.has_hard_line() && fits(width as isize - col as isize, probe);
                let m = if flat_fits { Mode::Flat } else { Mode::Break };
                stack.push((indent, m, sub));
            }
            Doc::IfBreak(broken, flat) => {
                let chosen = if mode == Mode::Break { broken } else { flat };
                stack.push((indent, mode, chosen.as_ref()));
            }
            Doc::Choice(opts) => {
                // Pick the first option whose every line fits; else the last.
                let chosen = opts
                    .iter()
                    .take(opts.len().saturating_sub(1))
                    .find(|opt| all_lines_fit(opt, indent, col, width, indent_unit))
                    .or_else(|| opts.last());
                if let Some(c) = chosen {
                    stack.push((indent, Mode::Break, c));
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(d: &Doc, w: usize) -> String {
        render(d, w, 4)
    }

    #[test]
    fn text_and_concat() {
        let d = Doc::concat([Doc::text("a"), Doc::text("b"), Doc::text("c")]);
        assert_eq!(r(&d, 80), "abc");
    }

    #[test]
    fn group_flat_when_fits() {
        // [ a, b, c ] on one line
        let d = Doc::group(Doc::concat([
            Doc::text("["),
            Doc::indent(Doc::concat([softline(), Doc::join(
                Doc::concat([Doc::text(","), line()]),
                [Doc::text("a"), Doc::text("b"), Doc::text("c")],
            )])),
            softline(),
            Doc::text("]"),
        ]));
        assert_eq!(r(&d, 80), "[a, b, c]");
    }

    #[test]
    fn group_breaks_when_too_wide() {
        let d = Doc::group(Doc::concat([
            Doc::text("["),
            Doc::indent(Doc::concat([
                softline(),
                Doc::join(
                    Doc::concat([Doc::text(","), line()]),
                    [Doc::text("aaaa"), Doc::text("bbbb"), Doc::text("cccc")],
                ),
            ])),
            softline(),
            Doc::text("]"),
        ]));
        assert_eq!(r(&d, 8), "[\n    aaaa,\n    bbbb,\n    cccc\n]");
    }

    #[test]
    fn hard_line_forces_break() {
        let d = Doc::group(Doc::concat([Doc::text("a"), hardline(), Doc::text("b")]));
        assert_eq!(r(&d, 80), "a\nb");
    }

    #[test]
    fn nested_indent() {
        let d = Doc::concat([
            Doc::text("fn f() {"),
            Doc::indent(Doc::concat([hardline(), Doc::text("return 1")])),
            hardline(),
            Doc::text("}"),
        ]);
        assert_eq!(r(&d, 80), "fn f() {\n    return 1\n}");
    }

    // The fluid assignment layout: flat | break-after-op | hug.
    fn assign_choice(lhs: &str, rhs: Doc) -> Doc {
        let flat = Doc::concat([Doc::text(lhs), Doc::text(" = "), rhs.clone()]);
        let after = Doc::concat([
            Doc::text(lhs),
            Doc::text(" ="),
            Doc::indent(Doc::concat([hardline(), rhs.clone()])),
        ]);
        let hug = Doc::concat([Doc::text(lhs), Doc::text(" = "), rhs]);
        Doc::choice([flat, after, hug])
    }

    // A multi-arg call group: `f(a, b, c)` flat, breaking one-per-line.
    fn call(name: &str, args: &[&str]) -> Doc {
        Doc::group(Doc::concat([
            Doc::text(name),
            Doc::text("("),
            Doc::indent(Doc::concat([
                softline(),
                Doc::join(
                    Doc::concat([Doc::text(","), line()]),
                    args.iter().map(|a| Doc::text(*a)),
                ),
            ])),
            softline(),
            Doc::text(")"),
        ]))
    }

    #[test]
    fn choice_picks_all_flat_when_it_fits() {
        let d = assign_choice("x", call("f", &["a", "b"]));
        assert_eq!(r(&d, 80), "x = f(a, b)");
    }

    #[test]
    fn choice_breaks_after_op_when_rhs_fits_on_its_own_line() {
        // `xxxx = f(aaaa, bbbb)` is 20 wide; at width 18 the one-liner fails but
        // the RHS (`f(aaaa, bbbb)`, 13 wide) fits at indent 4 (17) -> break after `=`.
        let d = assign_choice("xxxx", call("f", &["aaaa", "bbbb"]));
        assert_eq!(r(&d, 18), "xxxx =\n    f(aaaa, bbbb)");
    }

    #[test]
    fn choice_hugs_when_rhs_too_wide_even_alone() {
        // The RHS doesn't fit even on its own indented line -> hug and break args.
        let d = assign_choice("xxxx", call("f", &["aaaaaaaa", "bbbbbbbb"]));
        assert_eq!(r(&d, 16), "xxxx = f(\n    aaaaaaaa,\n    bbbbbbbb\n)");
    }

    #[test]
    fn trailing_space_trimmed_on_break() {
        // A normal line in break mode should not leave a trailing space.
        let d = Doc::concat([Doc::text("a"), line(), Doc::text("b")]);
        // Force break by wrapping in a group that won't fit.
        let d = Doc::group(d);
        assert_eq!(r(&d, 1), "a\nb");
    }
}
