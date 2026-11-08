//! Line directives mapping emitted Lua lines back to Rock spans.

use super::*;

/// A statement's start offset in a function's emitted text, and the span of
/// the instruction it renders.
pub(crate) type LineMark = (usize, Span);

/// The line LuaJIT reads after [`LINE_DIRECTIVE_RESET`] until the assembler
/// resolves it ([`resolve_line_directive_resets`]).
pub const LINE_DIRECTIVE_RESET: &str = "# 0 \"=[generated]\"";

/// Number the lines of one emitted definition with Rock source lines.
///
/// LuaJIT 2.1 (since commit c41d727d) reads `# <line> ["<file>"]` at column 0
/// as "the next line is `<line>` of `<file>`". A prototype keeps only the last
/// file it saw, so the definition is numbered against ONE file, the one most of
/// its marks come from; instructions inlined from other files keep the line of
/// the statement before them. Every line from the header on carries the line
/// of the latest mark at or before it, so the nil kills, jumps and loop heads
/// that follow a statement report that statement. Label lines produce no
/// bytecode and get no directive. Text before the header (hoisted constants)
/// belongs to the enclosing chunk and is left alone, and the definition ends
/// with [`LINE_DIRECTIVE_RESET`] so code after it is not numbered as Rock.
///
/// Returns the new text and each insertion as (offset in `lua`, bytes), in
/// ascending order, or `None` when no mark names a source file.
pub(crate) fn write_line_directives(
    lua: &str,
    header_offset: usize,
    marks: &[LineMark],
) -> Option<(String, Vec<(usize, usize)>)> {
    let unknown = Span::unknown().file;
    let known = |span: &Span| span.file != unknown;
    let mut counts: Vec<(ustr::Ustr, usize)> = Vec::new();
    for (_, span) in marks.iter().filter(|(_, span)| known(span)) {
        match counts.iter_mut().find(|(file, _)| *file == span.file) {
            Some((_, count)) => *count += 1,
            None => counts.push((span.file, 1)),
        }
    }
    // `max_by_key` keeps the last maximum; reversing makes a tie go to the
    // file seen first.
    let (file, _) = counts
        .iter()
        .rev()
        .max_by_key(|(_, count)| *count)
        .copied()?;
    let own: Vec<(usize, u32)> = marks
        .iter()
        .filter(|(_, span)| span.file == file)
        .map(|(offset, span)| (*offset, span.line_range.0 as u32 + 1))
        .collect();

    let header_line = lua[..header_offset].rfind('\n').map_or(0, |at| at + 1);
    let mut out = String::with_capacity(lua.len() + lua.len() / 4);
    out.push_str(&lua[..header_line]);
    let mut inserted = Vec::new();
    let mut next_mark = 0;
    let mut target = own[0].1;
    // The line LuaJIT gives the next line, once a directive has set it.
    let mut current: Option<u32> = None;
    let mut start = header_line;
    while start < lua.len() {
        let end = lua[start..]
            .find('\n')
            .map_or(lua.len(), |at| start + at + 1);
        while next_mark < own.len() && own[next_mark].0 < end {
            target = own[next_mark].1;
            next_mark += 1;
        }
        let line = &lua[start..end];
        if current != Some(target) && !line.trim_start().starts_with("::") {
            let before = out.len();
            let _ = write!(out, "# {target}");
            if current.is_none() {
                out.push(' ');
                if file.as_str().starts_with(['=', '@', ' ']) {
                    write_lua_string(&mut out, &format!("@{file}"));
                } else {
                    write_lua_string(&mut out, file.as_str());
                }
            }
            out.push('\n');
            inserted.push((start, out.len() - before));
            current = Some(target);
        }
        out.push_str(line);
        current = current.map(|line| line + 1);
        start = end;
    }
    let before = out.len();
    out.push_str(LINE_DIRECTIVE_RESET);
    out.push('\n');
    inserted.push((lua.len(), out.len() - before));
    Some((out, inserted))
}

/// Replace every [`LINE_DIRECTIVE_RESET`] in an assembled chunk with a
/// directive that numbers the following lines by their position in the chunk,
/// under the file name `chunk`. Code outside Rock definitions then reports
/// where it sits in the generated file.
pub fn resolve_line_directive_resets(lua: &str, chunk: &str) -> String {
    let mut out = String::with_capacity(lua.len());
    for (index, line) in lua.split_inclusive('\n').enumerate() {
        if line.trim_end_matches('\n') == LINE_DIRECTIVE_RESET {
            // `index` is zero-based, so the next line is `index + 2`.
            let _ = write!(out, "# {} ", index + 2);
            write_lua_string(&mut out, chunk);
            out.push('\n');
        } else {
            out.push_str(line);
        }
    }
    out
}
