//! Trailing-comment column alignment, run as a post-pass over rendered text.
//!
//! The printer emits each trailing comment as `<code>\0<col>\0//<text>`, where
//! `<col>` is the comment's original source column. This pass groups adjacent
//! trailing-comment lines (same indentation, no intervening blank line) and
//! aligns their `//` to a common column: `max(longest code + 2, widest original
//! column)` -- i.e. it preserves the author's chosen column and only pulls
//! stragglers into line, never narrowing below a two-space gap.
//!
//! Using an emitted sentinel (rather than scanning for `//`) means string
//! literals containing `//` are never mistaken for comments.

/// Marks a trailing comment in rendered output: `\0<origcol>\0//text`.
pub const SENTINEL: char = '\u{0}';

struct Trailing {
    /// Display width of the code before the comment (column the code ends at).
    code_col: usize,
    /// The comment's original source column (author's alignment intent).
    orig_col: usize,
    /// Leading-whitespace width (group key).
    indent: usize,
    /// `A` always align, `P` align only if already aligned, `N` never align.
    mode: char,
}

/// Parse a line's trailing-comment sentinel `\0<mode><col>\0//text`, if present.
fn parse(line: &str) -> Option<(String, Trailing, String)> {
    let first = line.find(SENTINEL)?;
    let rest = &line[first + SENTINEL.len_utf8()..];
    let second = rest.find(SENTINEL)?;
    let meta = &rest[..second];
    let mut chars = meta.chars();
    let mode = chars.next()?;
    let orig_col: usize = chars.as_str().parse().ok()?;
    let comment = rest[second + SENTINEL.len_utf8()..].to_string();
    let code = line[..first].to_string();
    let code_col = code.chars().count();
    let indent = code.len() - code.trim_start().len();
    Some((
        code.clone(),
        Trailing {
            code_col,
            orig_col,
            indent,
            mode,
        },
        comment,
    ))
}

pub fn align(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let parsed: Vec<Option<(String, Trailing, String)>> = lines.iter().map(|l| parse(l)).collect();

    // Group adjacent trailing-comment lines: same indent, broken by a blank
    // line or a line at a different indent.
    let mut col_for = vec![None; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let Some((_, t0, _)) = &parsed[i] else {
            i += 1;
            continue;
        };
        let group_indent = t0.indent;
        let mut j = i;
        let mut members = Vec::new();
        while j < lines.len() {
            let line = lines[j];
            if line.trim().is_empty() {
                break;
            }
            let indent = line.len() - line.trim_start().len();
            if indent != group_indent {
                break;
            }
            if parsed[j].is_some() {
                members.push(j);
            }
            j += 1;
        }
        // Decide the group's column. A lone comment, or a non-aligned group of
        // statements/items, normalizes each to a two-space gap. Comma-block
        // groups (`always`) and already-aligned statement groups align to the
        // author's column (widest original `//`), clamped to longest code + 1.
        let mode = parsed[members[0]].as_ref().unwrap().1.mode;
        let orig_cols: Vec<usize> = members
            .iter()
            .map(|&m| parsed[m].as_ref().unwrap().1.orig_col)
            .collect();
        let already_aligned = members.len() >= 2 && orig_cols.iter().all(|c| *c == orig_cols[0]);
        let do_align = members.len() >= 2
            && match mode {
                'A' => true,
                'P' => already_aligned,
                _ => false, // 'N': never align
            };

        if do_align {
            // Statement groups ('P') keep at least a two-space gap after the
            // longest code; comma-block groups ('A', struct fields / match arms)
            // hug to one space after the longest. The author's column wins when
            // they aligned to something wider.
            let min_gap = if mode == 'A' { 1 } else { 2 };
            let mut c = 0usize;
            for &m in &members {
                let t = &parsed[m].as_ref().unwrap().1;
                c = c.max(t.orig_col).max(t.code_col + min_gap);
            }
            for &m in &members {
                col_for[m] = Some(c);
            }
        } else {
            // Lone / non-aligned comment: preserve the author's gap (its original
            // column), clamped to at least one space after the code.
            for &m in &members {
                let t = &parsed[m].as_ref().unwrap().1;
                col_for[m] = Some(t.orig_col.max(t.code_col + 1));
            }
        }
        i = j.max(i + 1);
    }

    let mut out = Vec::with_capacity(lines.len());
    for (idx, line) in lines.iter().enumerate() {
        match (&parsed[idx], col_for[idx]) {
            (Some((code, t, comment)), Some(col)) => {
                let pad = col.saturating_sub(t.code_col);
                out.push(format!("{code}{}{comment}", " ".repeat(pad)));
            }
            _ => out.push(line.to_string()),
        }
    }
    out.join("\n")
}
