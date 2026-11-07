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
                let m = if d.has_hard_line() {
                    Mode::Break
                } else {
                    Mode::Flat
                };
                stack.push((indent, m, d));
            }
        }
    }
    false
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
                let flat_fits = !sub.has_hard_line() && fits(width as isize - col as isize, probe);
                let m = if flat_fits { Mode::Flat } else { Mode::Break };
                stack.push((indent, m, sub));
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
            Doc::indent(Doc::concat([
                softline(),
                Doc::join(
                    Doc::concat([Doc::text(","), line()]),
                    [Doc::text("a"), Doc::text("b"), Doc::text("c")],
                ),
            ])),
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

    #[test]
    fn trailing_space_trimmed_on_break() {
        // A normal line in break mode should not leave a trailing space.
        let d = Doc::concat([Doc::text("a"), line(), Doc::text("b")]);
        // Force break by wrapping in a group that won't fit.
        let d = Doc::group(d);
        assert_eq!(r(&d, 1), "a\nb");
    }
}
