use crate::prelude::*;

type ReportBuilder<'a> = ariadne::ReportBuilder<'a, (String, std::ops::Range<usize>)>;

pub fn make_report_builder(
    kind: ariadne::ReportKind<'_>,
    file: String,
    offset: usize,
    code: String,
    message: String,
) -> ReportBuilder<'_> {
    // `Span.offset_range` is byte offsets (Tokenizer::advance increments
    // by `c.len_utf8()`). Ariadne's default IndexType is Char, which
    // would interpret our bytes as chars and skew labels past multibyte
    // UTF-8 characters; force Byte. Color is also gated here: ANSI
    // styling of source-frames is a meaningful share of ariadne's
    // per-diagnostic cost, and piped stderr doesn't render colors
    // anyway. Override with `ROCK_FORCE_COLOR=1` /
    // `ROCK_NO_COLOR=1` / `NO_COLOR=1`.
    let config = ariadne::Config::default()
        .with_index_type(ariadne::IndexType::Byte)
        .with_color(stderr_is_tty());
    ariadne::Report::build(kind, file, offset)
        .with_config(config)
        .with_code(code)
        .with_message(message)
}

/// Cached "is stderr a TTY?" probe. The result is computed once per
/// process; flip with `ROCK_FORCE_COLOR=1` (force on) or
/// `ROCK_NO_COLOR=1` / `NO_COLOR=1` (force off, NO_COLOR follows the
/// no-color.org convention).
pub fn stderr_is_tty() -> bool {
    use std::io::IsTerminal;
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        if std::env::var_os("ROCK_FORCE_COLOR").is_some() {
            return true;
        }
        if std::env::var_os("ROCK_NO_COLOR").is_some()
            || std::env::var_os("NO_COLOR").is_some()
        {
            return false;
        }
        std::io::stderr().is_terminal()
    })
}

pub trait ReportBuilderExt {
    fn add_span_label(&mut self, span: &Span, message: String);
}

impl ReportBuilderExt for ReportBuilder<'_> {
    fn add_span_label(&mut self, span: &Span, message: String) {
        let range = span.offset_range.0..span.offset_range.1;
        let label = ariadne::Label::new((span.file.to_string(), range));
        self.add_label(label.with_message(message));
    }
}

#[derive(Copy, Clone, Debug)]
pub enum LineColError {
    ColumnOutOfRange,
    LineOutOfRange,
}

pub fn line_col_to_offset(s: &str, line: usize, col: usize) -> Result<usize, LineColError> {
    let mut offset = 0;

    for (i, line_str) in s.lines().enumerate() {
        if i == line {
            if col > line_str.len() {
                return Err(LineColError::ColumnOutOfRange);
            }

            offset += line_str
                .chars()
                .take(col)
                .map(|c| c.len_utf8())
                .sum::<usize>();

            return Ok(offset);
        } else {
            offset += line_str.len() + 1;
        }
    }

    Err(LineColError::LineOutOfRange)
}

