use crate::prelude::*;

type ReportBuilder<'a> = ariadne::ReportBuilder<'a, (String, std::ops::Range<usize>)>;

/// Build a report whose labels address the source by byte offset.
///
/// `Span.offset_range` holds byte offsets (`Tokenizer::advance` adds
/// `c.len_utf8()` per character), while ariadne's default index type counts
/// characters and would place a label past a multibyte character in the wrong
/// column; the index type is therefore fixed to bytes. Colour is settled here
/// as well, because styling the source frame is a large share of ariadne's
/// per-diagnostic cost and a piped stderr renders none of it. `ROCK_FORCE_COLOR`
/// and `ROCK_NO_COLOR` / `NO_COLOR` override the terminal probe.
pub fn make_report_builder(
    kind: ariadne::ReportKind<'_>,
    file: String,
    offset: usize,
    code: String,
    text: String,
) -> ReportBuilder<'_> {
    let config = ariadne::Config::default()
        .with_index_type(ariadne::IndexType::Byte)
        .with_color(stderr_is_tty());
    ariadne::Report::build(kind, file, offset)
        .with_config(config)
        .with_code(code)
        .with_message(text)
}

/// Whether diagnostic colour is on, probed from stderr once per process.
/// `ROCK_FORCE_COLOR` turns it on, while `ROCK_NO_COLOR` and `NO_COLOR` (the
/// no-color.org convention) turn it off without consulting the terminal.
pub fn stderr_is_tty() -> bool {
    use std::io::IsTerminal;
    use std::sync::OnceLock;
    static PROBED: OnceLock<bool> = OnceLock::new();
    *PROBED.get_or_init(|| {
        if std::env::var_os("ROCK_FORCE_COLOR").is_some() {
            return true;
        }
        if std::env::var_os("ROCK_NO_COLOR").is_some() || std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        std::io::stderr().is_terminal()
    })
}

pub trait ReportBuilderExt {
    fn add_span_label(&mut self, span: &Span, label: String);
}

impl ReportBuilderExt for ReportBuilder<'_> {
    fn add_span_label(&mut self, span: &Span, label: String) {
        let range = span.offset_range.0..span.offset_range.1;
        self.add_label(ariadne::Label::new((span.file.to_string(), range)).with_message(label));
    }
}

#[derive(Copy, Clone, Debug)]
pub enum LineColError {
    ColumnOutOfRange,
    LineOutOfRange,
}

/// Byte offset of `col` characters into line `line` of `s`.
///
/// Lines are the ones `str::lines` yields, so a trailing line break ends the
/// last line rather than starting an empty one, and `col` is checked against
/// the byte length of the line before being counted in characters. A `\r\n`
/// break advances the offset by one byte rather than two, so a later line's
/// base offset sits on the `\n` of a CRLF break.
pub fn line_col_to_offset(s: &str, line: usize, col: usize) -> Result<usize, LineColError> {
    let mut offset = 0usize;

    for (index, text) in s.lines().enumerate() {
        if index != line {
            offset += text.len() + 1;
            continue;
        }
        if col > text.len() {
            return Err(LineColError::ColumnOutOfRange);
        }
        return Ok(offset + text.chars().take(col).map(char::len_utf8).sum::<usize>());
    }

    Err(LineColError::LineOutOfRange)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_utils::strip_ansi_sgr;

    const OPEN: &str = "\u{256d}\u{2500}";
    const CLOSE: &str = "\u{2500}\u{2500}\u{2500}\u{256f}";
    const BAR: &str = "\u{2502}";

    fn joined(lines: &[String]) -> String {
        lines.join("\n")
    }

    fn span(offset_range: (usize, usize)) -> Span {
        Span {
            file: ustr::ustr("u.rock"),
            line_range: (0, 0),
            col_range: offset_range,
            offset_range,
        }
    }

    #[test]
    fn a_built_report_carries_the_code_message_and_span() {
        let mut report = make_report_builder(
            ariadne::ReportKind::Warning,
            "u.rock".to_string(),
            0,
            "W07".to_string(),
            "header text".to_string(),
        );
        report.add_span_label(&span((0, 3)), "label text".to_string());

        let mut written = Vec::new();
        report
            .finish()
            .write(
                ("u.rock".to_string(), ariadne::Source::from("abc\n")),
                &mut written,
            )
            .unwrap();

        assert_eq!(
            strip_ansi_sgr(&String::from_utf8(written).unwrap()),
            joined(&[
                "[W07] Warning: header text".to_string(),
                format!("   {OPEN}[u.rock:1:1]"),
                format!("   {BAR}"),
                format!(" 1 {BAR} abc"),
                format!("   {BAR} \u{2500}\u{252c}\u{2500}  "),
                format!("   {BAR}  \u{2570}\u{2500}\u{2500}\u{2500} label text"),
                CLOSE.to_string(),
                String::new(),
            ])
        );
    }

    #[test]
    fn a_report_without_labels_renders_only_its_header() {
        let report = make_report_builder(
            ariadne::ReportKind::Error,
            "u.rock".to_string(),
            1,
            "E03".to_string(),
            "no labels".to_string(),
        );

        let mut written = Vec::new();
        report
            .finish()
            .write(
                ("u.rock".to_string(), ariadne::Source::from("abc\n")),
                &mut written,
            )
            .unwrap();

        assert_eq!(
            strip_ansi_sgr(&String::from_utf8(written).unwrap()),
            "[E03] Error: no labels\n"
        );
    }
}
