/// Marks a trailing comment in rendered output: `\0<origcol>\0//text`.
/// Trailing comments are marked with a sentinel rather than found by scanning for `//`,
/// so string literals containing `//` are never mistaken for comments.
pub const SENTINEL: char = '\u{0}';

struct Trailing<'a> {
    code: &'a str,
    comment: &'a str,
    code_col: usize,
    orig_col: usize,
    /// `A` always align, `P` align only if already aligned, `N` never align.
    mode: char,
}

impl<'a> Trailing<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        let (code, rest) = line.split_once(SENTINEL)?;
        let (meta, comment) = rest.split_once(SENTINEL)?;
        let mut chars = meta.chars();
        let mode = chars.next()?;
        let orig_col = chars.as_str().parse().ok()?;
        Some(Self {
            code,
            comment,
            code_col: code.chars().count(),
            orig_col,
            mode,
        })
    }

    fn column(&self, min_gap: usize) -> usize {
        self.orig_col.max(self.code_col + min_gap)
    }
}

/// Decide the group's column. A lone comment, or a non-aligned group of
/// statements/items, normalizes each to a two-space gap. Comma-block
/// groups (`always`) and already-aligned statement groups align to the
/// author's column (widest original `//`), clamped to longest code + 1.
fn group_column(group: &[(&str, Option<Trailing<'_>>)]) -> Option<usize> {
    let mut comments = group.iter().filter_map(|(_, comment)| comment.as_ref());
    let first = comments.next()?;
    comments.clone().next()?;
    let min_gap = match first.mode {
        'A' => 1,
        'P' if comments.clone().all(|c| c.orig_col == first.orig_col) => 2,
        _ => return None,
    };
    std::iter::once(first)
        .chain(comments)
        .map(|comment| comment.column(min_gap))
        .max()
}

pub fn align(text: &str) -> String {
    let lines: Vec<_> = text
        .split('\n')
        .map(|line| (line, Trailing::parse(line)))
        .collect();
    let indent = |line: &str| line.len() - line.trim_start().len();
    let groups = lines.chunk_by(|(a, _), (b, _)| {
        !a.trim().is_empty() && !b.trim().is_empty() && indent(a) == indent(b)
    });
    let mut out = String::with_capacity(text.len());
    for group in groups {
        let shared_col = group_column(group);
        for (line, comment) in group {
            if let Some(t) = comment {
                let col = shared_col.unwrap_or_else(|| t.column(1));
                out.push_str(t.code);
                out.extend(std::iter::repeat_n(' ', col - t.code_col));
                out.push_str(t.comment);
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
    }
    out.pop();
    out
}
