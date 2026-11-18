//! Convert between compiler source coordinates and LSP locations.

use super::{BoneProject, editor_path};
use crate::FileUriExt;
use crate::query::TextOf;
use bone_semantic::prelude::Span;
use lsp_types::{Location, Position, Range, Uri};
use std::borrow::Cow;
use std::collections::HashMap;
use ustr::Ustr;

/// 0-based LSP `(line, character)` -> 0-based **byte** offset into `src`.
/// `character` counts UTF-16 code units, the protocol's default encoding. A
/// column inside a surrogate pair snaps to the character's start; past the
/// line's end it clamps there (before the line break), per the spec.
pub fn offset_at_line_col(src: &str, line: u32, character: u32) -> usize {
    let mut offset = 0usize;
    for (i, line_str) in src.split_inclusive('\n').enumerate() {
        if i == line as usize {
            let text = line_str.strip_suffix('\n').unwrap_or(line_str);
            let text = text.strip_suffix('\r').unwrap_or(text);
            let mut units = 0u32;
            for (byte, ch) in text.char_indices() {
                units += ch.len_utf16() as u32;
                if units > character {
                    return offset + byte;
                }
            }
            return offset + text.len();
        }
        offset += line_str.len();
    }
    src.len()
}

/// Byte offset -> 0-based LSP [`Position`], `character` in UTF-16 code units
/// like [`offset_at_line_col`].
pub fn offset_to_position(src: &str, offset: usize) -> Position {
    let mut line = 0u32;
    let mut col = 0u32;
    let mut cur = 0usize;
    for ch in src.chars() {
        if cur >= offset {
            break;
        }
        cur += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf16() as u32;
        }
    }
    Position::new(line, col)
}

/// The LSP character of byte `offset` on its own line, in UTF-16 code units.
pub(super) fn utf16_col_at(src: &str, offset: usize) -> u32 {
    let Some(before) = src.get(..offset) else {
        return 0;
    };
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    before[line_start..].encode_utf16().count() as u32
}

/// A compiler `(line, col)`, where `col` counts Unicode scalars, as an LSP
/// [`Position`] in UTF-16 code units. Without the file's text the column is
/// passed through, which is exact for any line without astral characters.
fn compiler_position(text: Option<&str>, line: usize, col: usize) -> Position {
    let units = text.and_then(|t| t.split('\n').nth(line)).map(|l| {
        let prefix = l.chars().take(col);
        let (scalars, units) =
            prefix.fold((0, 0u32), |(n, u), c| (n + 1, u + c.len_utf16() as u32));
        units + (col - scalars) as u32
    });
    Position::new(line as u32, units.unwrap_or(col as u32))
}

/// Index line starts once for batches of span-to-position conversions.
pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(src: &str) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        LineIndex { line_starts }
    }

    /// LSP [`Range`] for a byte span, both ends via [`LineIndex::position`].
    pub fn range(&self, src: &str, start: usize, stop: usize) -> Range {
        Range::new(self.position(src, start), self.position(src, stop))
    }

    /// 0-based byte `offset` -> 0-based LSP `(line, character)`. `character` counts
    /// UTF-16 code units within the line, matching [`offset_to_position`].
    pub fn position(&self, src: &str, offset: usize) -> Position {
        let offset = offset.min(src.len());
        // Last line start <= offset (partition_point gives the count of starts
        // that are <=, so -1 is that line's index; line_starts[0]=0 so >=1).
        let line = self.line_starts.partition_point(|&s| s <= offset) - 1;
        let line_start = self.line_starts[line];
        let mut pos = offset_to_position(&src[line_start..], offset - line_start);
        pos.line = line as u32;
        pos
    }
}

pub(super) fn byte_range(src: &str, (start, end): (usize, usize)) -> Range {
    Range::new(offset_to_position(src, start), offset_to_position(src, end))
}

pub(super) fn byte_offset(src: &str, pos: Position) -> usize {
    offset_at_line_col(src, pos.line, pos.character)
}

pub(super) fn byte_offsets(src: &str, range: Range) -> (usize, usize) {
    (byte_offset(src, range.start), byte_offset(src, range.end))
}

/// A compiler span's `(line, col)` range as an LSP range.
pub(super) fn span_range(s: &Span, text_of: TextOf<'_, '_>) -> Range {
    let text = text_of(s.file.as_str());
    Range::new(
        compiler_position(text, s.line_range().0, s.col_range().0),
        compiler_position(text, s.line_range().1, s.col_range().1),
    )
}

/// The one-column range at a label-less error's location.
pub(super) fn location_range(
    loc: &rock_core::source_code::LineCol,
    text_of: TextOf<'_, '_>,
) -> Range {
    let text = text_of(loc.file.as_str());
    Range::new(
        compiler_position(text, loc.line, loc.col),
        compiler_position(text, loc.line, loc.col + 1),
    )
}

pub(super) fn editor_uri(file: &str) -> Result<Uri, String> {
    Uri::from_file_path(&editor_path(file)).map_err(|error| error.to_string())
}

pub(super) fn source_location(
    project: &BoneProject,
    file: &str,
    request: &'static str,
    fallback: &'static str,
) -> Option<Uri> {
    match editor_uri(file) {
        Ok(uri) => Some(uri),
        Err(error) => {
            project
                .fallbacks
                .report(request, file, "construct URI", &error, fallback);
            None
        }
    }
}

pub(super) fn location(
    project: &BoneProject,
    span: &Span,
    indexes: Option<&mut HashMap<Ustr, LineIndex>>,
) -> Option<Location> {
    let file = span.file.as_str();
    let uri = source_location(project, file, "span location", "omit location")?;
    let text = match project.texts.get(file) {
        Some(text) => Cow::Borrowed(text.as_str()),
        None => match fs_err::read_to_string(file) {
            Ok(text) => Cow::Owned(text),
            Err(error) => {
                project.fallbacks.report(
                    "span location",
                    file,
                    "read source",
                    &error.to_string(),
                    "omit location",
                );
                return None;
            }
        },
    };
    let (start, end) = span.offset_range();
    let range = match indexes {
        Some(indexes) => indexes
            .entry(span.file)
            .or_insert_with(|| LineIndex::new(&text))
            .range(&text, start, end),
        None => byte_range(&text, (start, end)),
    };
    Some(Location { uri, range })
}
