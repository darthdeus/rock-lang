//! Line directives mapping emitted Lua lines back to Rock spans.

use super::*;

/// A statement's start offset in a function's emitted text, and the span of
/// the instruction it renders.
pub(crate) type LineMark = (usize, Span);

/// The line LuaJIT reads after [`LINE_DIRECTIVE_RESET`] until the assembler
/// resolves it ([`resolve_line_directive_resets`]).
pub const LINE_DIRECTIVE_RESET: &str = "# 0 \"=[generated]\"";

/// Number a definition using the source file that supplies most of its marks.
/// LuaJIT remembers only the last file in a prototype. Leave hoisted code
/// outside the header unnumbered and reset the chunk source after the body.
/// Report insertions as offsets in the original Lua and their byte lengths.
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
        .map(|(offset, span)| (*offset, span.line_range().0 as u32 + 1))
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
