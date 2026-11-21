use crate::prelude::*;

pub(crate) type ReportBuilder<'a> = ariadne::ReportBuilder<'a, (String, std::ops::Range<usize>)>;

/// Ariadne must index bytes to match spans after multibyte characters.
/// `ROCK_FORCE_COLOR` and `NO_COLOR` override terminal color detection.
pub(crate) fn make_report_builder<'a>(
    kind: ariadne::ReportKind<'a>,
    file: String,
    offset: usize,
    code: impl std::fmt::Display,
    text: &str,
) -> ReportBuilder<'a> {
    let config = ariadne::Config::default()
        .with_index_type(ariadne::IndexType::Byte)
        .with_color(stderr_is_tty());
    ariadne::Report::build(kind, file, offset)
        .with_config(config)
        .with_code(code)
        .with_message(text)
}

fn stderr_is_tty() -> bool {
    use std::io::IsTerminal;
    use std::sync::OnceLock;
    static PROBED: OnceLock<bool> = OnceLock::new();
    *PROBED.get_or_init(|| {
        if std::env::var_os("ROCK_FORCE_COLOR").is_some() {
            return true;
        }
        if std::env::var_os("NO_COLOR").is_some() {
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
        let (start, end) = span.offset_range();
        self.add_label(
            ariadne::Label::new((span.file.to_string(), start..end)).with_message(label),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_utils::strip_ansi_sgr;

    #[test]
    fn a_report_without_labels_renders_only_its_header() {
        let report = make_report_builder(
            ariadne::ReportKind::Error,
            "u.rock".to_string(),
            1,
            "E03",
            "no labels",
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
