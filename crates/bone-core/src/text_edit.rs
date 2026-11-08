//! The coordinate change one incremental text edit makes, and the one policy
//! for moving stored spans past it.
//!
//! Every cache that keeps spans across a body edit (the session's HIR, the lint
//! cache, the instance graph) consumes the same delta and shifts with
//! [`TextEditDelta::shift_span`], so they cannot disagree about where a kept
//! node now sits.

use rock_core::{LineCol, Span};
use ustr::Ustr;

/// Text in `file` at byte `at` and later moved by `byte_delta` bytes and
/// `line_delta` lines; text before `at` is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextEditDelta {
    pub file: Ustr,
    pub at: usize,
    pub byte_delta: isize,
    pub line_delta: isize,
}

impl TextEditDelta {
    /// Whether a span in this edit's file starts at or after the edit point.
    pub fn moves(&self, span: &Span) -> bool {
        span.file == self.file && span.offset_range.0 >= self.at
    }

    /// A stored byte offset, moved iff it sits at or after the edit point.
    pub fn shift_offset(&self, offset: usize) -> usize {
        if offset >= self.at {
            add(offset, self.byte_delta)
        } else {
            offset
        }
    }

    /// Move a span that [`moves`](Self::moves) by the byte and line deltas.
    /// Returns whether it moved.
    ///
    /// `col_range` is deliberately kept. Incremental edits are confined to one
    /// body, and every kept node after it belongs to a later top-level item,
    /// which starts on a later physical line (items are newline-separated), so
    /// its column is unchanged even when the edit changed the length of a line.
    /// Arithmetic saturates: a stale span must not wrap to a huge offset.
    pub fn shift_span(&self, span: &mut Span) -> bool {
        if !self.moves(span) {
            return false;
        }
        span.offset_range = (
            add(span.offset_range.0, self.byte_delta),
            add(span.offset_range.1, self.byte_delta),
        );
        span.line_range = (
            add(span.line_range.0, self.line_delta),
            add(span.line_range.1, self.line_delta),
        );
        true
    }

    /// [`shift_span`](Self::shift_span) for a point: offset and line move,
    /// the column is kept for the same reason.
    pub fn shift_line_col(&self, at: &mut LineCol) -> bool {
        if at.file != self.file || at.offset < self.at {
            return false;
        }
        at.offset = add(at.offset, self.byte_delta);
        at.line = add(at.line, self.line_delta);
        true
    }
}

fn add(value: usize, delta: isize) -> usize {
    value.saturating_add_signed(delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(file: &str, line: usize, col: (usize, usize), off: (usize, usize)) -> Span {
        Span {
            file: Ustr::from(file),
            line_range: (line, line),
            col_range: col,
            offset_range: off,
        }
    }

    #[test]
    fn same_line_count_edit_keeps_columns_of_later_spans() {
        let edit = TextEditDelta {
            file: Ustr::from("a.rock"),
            at: 10,
            byte_delta: 6,
            line_delta: 0,
        };
        let mut s = span("a.rock", 5, (8, 19), (60, 71));
        assert!(edit.shift_span(&mut s));
        assert_eq!(s.offset_range, (66, 77));
        assert_eq!(s.line_range, (5, 5));
        assert_eq!(s.col_range, (8, 19));
    }

    #[test]
    fn spans_before_the_edit_or_in_other_files_stay() {
        let edit = TextEditDelta {
            file: Ustr::from("a.rock"),
            at: 10,
            byte_delta: -4,
            line_delta: -1,
        };
        let mut before = span("a.rock", 0, (0, 3), (2, 5));
        let mut other = span("b.rock", 9, (0, 3), (90, 93));
        assert!(!edit.shift_span(&mut before));
        assert!(!edit.shift_span(&mut other));
        assert_eq!(before.offset_range, (2, 5));
        assert_eq!(other.offset_range, (90, 93));
    }

    #[test]
    fn shifting_saturates_instead_of_wrapping() {
        let edit = TextEditDelta {
            file: Ustr::from("a.rock"),
            at: 0,
            byte_delta: -100,
            line_delta: -100,
        };
        let mut s = span("a.rock", 1, (0, 1), (5, 6));
        edit.shift_span(&mut s);
        assert_eq!(s.offset_range, (0, 0));
        assert_eq!(s.line_range, (0, 0));
    }
}
