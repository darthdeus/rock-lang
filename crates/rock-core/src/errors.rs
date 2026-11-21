use crate::prelude::*;
use crate::source_files::SourceFiles;
use crate::utils::{ReportBuilder, ReportBuilderExt, make_report_builder};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CompilerErrorLevel {
    Advice,
    Warning,
    Error,
}

impl CompilerErrorLevel {
    fn report_kind(self) -> ariadne::ReportKind<'static> {
        match self {
            Self::Advice => ariadne::ReportKind::Advice,
            Self::Warning => ariadne::ReportKind::Warning,
            Self::Error => ariadne::ReportKind::Error,
        }
    }

    fn initial(self) -> &'static str {
        match self {
            Self::Advice => "A",
            Self::Warning => "W",
            Self::Error => "E",
        }
    }

    fn one_line_prefix(self) -> colored::ColoredString {
        use colored::Colorize;

        match self {
            Self::Error => "[E00] Error:".red().bold(),
            Self::Warning => "[W00] Warning:".yellow().bold(),
            Self::Advice => "[A00] Advice:".blue().bold(),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum CompilerErrorCode {
    Unknown,
    TokenizerError,
    SyntaxError,
}

#[derive(Clone, Debug)]
pub struct CompilerLabel {
    pub span: Span,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CompilerErrorKind {
    Generic,

    /// Implicit materialization violates the `nobox` language guarantee.
    NoboxGuaranteeViolated,

    TargetLimit,
}

#[derive(Clone, Debug)]
pub struct CompilerError {
    pub location: LineCol,
    pub kind: CompilerErrorKind,
    pub code: CompilerErrorCode,
    pub labels: Vec<CompilerLabel>,
    pub level: CompilerErrorLevel,
    pub message: String,
    /// Independent failures from the same pass, for callers that can return only one error.
    /// Rendered after the primary one; they never nest.
    pub siblings: Vec<CompilerError>,
}

impl CompilerError {
    fn at(location: LineCol, message: String) -> Self {
        CompilerError {
            location,
            kind: CompilerErrorKind::Generic,
            code: CompilerErrorCode::Unknown,
            labels: Vec::new(),
            level: CompilerErrorLevel::Error,
            message: display_diagnostic_names(message),
            siblings: Vec::new(),
        }
    }

    pub fn new_no_location(message: impl Into<String>) -> Self {
        Self::new_with_label(Span::unknown(), message.into())
    }

    /// A span-only error for trial paths (overload scoring, `coerce(..).is_ok()`) that discard
    /// the error unrendered, so the expensive type rendering is skipped.
    pub fn silent(span: Span) -> Self {
        Self::at(span.start(), String::new())
    }

    pub fn with_siblings(mut self, others: impl IntoIterator<Item = CompilerError>) -> Self {
        for mut other in others {
            let nested = std::mem::take(&mut other.siblings);
            self.siblings.push(other);
            self.siblings.extend(nested);
        }
        self
    }

    pub fn new_with_label(span: Span, message: String) -> Self {
        let mut error = Self::at(span.start(), message);
        error.labels.push(CompilerLabel {
            span,
            message: error.message.clone(),
        });
        error
    }

    fn has_no_location(&self) -> bool {
        self.location.file == UNKNOWN_FILE_NAME
    }

    pub fn or_at(mut self, span: Span) -> Self {
        if self.has_no_location() {
            self.location = span.start();
            let message = self.message.clone();
            self.labels.clear();
            self = self.with_label(span, message);
        }
        self
    }

    pub fn with_level(mut self, level: CompilerErrorLevel) -> Self {
        self.level = level;
        self
    }

    pub fn with_code(mut self, code: CompilerErrorCode) -> Self {
        self.code = code;
        self
    }

    pub fn with_label(mut self, span: Span, message: String) -> Self {
        let message = display_diagnostic_names(message);
        self.labels.push(CompilerLabel { span, message });
        self
    }

    pub fn one_line(&self) -> String {
        let location = match self.location.file.as_str() {
            UNKNOWN_FILE_NAME => String::new(),
            _ => format!(
                " at {}:{}:{}",
                self.location.file, self.location.line, self.location.col
            ),
        };

        format!(
            "{} {}{}",
            self.level.one_line_prefix(),
            self.message,
            location
        )
    }

    #[must_use]
    pub fn render(&self, sources: &SourceFiles) -> String {
        render_all(std::slice::from_ref(self), sources)
    }

    fn report(&self) -> ReportBuilder<'static> {
        let file = self.location.file;
        let offset = self.location.offset;
        let mut builder = make_report_builder(
            self.level.report_kind(),
            file.to_string(),
            offset,
            format_args!("{}{:02}", self.level.initial(), self.code as u8),
            &self.message,
        );

        match self.labels.as_slice() {
            [] => builder.add_label(ariadne::Label::new((file.to_string(), offset..offset + 1))),
            labels => {
                for label in labels {
                    builder.add_span_label(&label.span, label.message.clone());
                }
            }
        }
        builder
    }
}

// An ariadne cache copies its source files; share one across the batch.
#[must_use]
pub fn render_all(errs: &[CompilerError], sources: &SourceFiles) -> String {
    let (unframed, framed): (Vec<_>, Vec<_>) = errs.iter().partition(|error| {
        let file = error.location.file.as_str();
        error.has_no_location()
            || (crate::is_stdlib_module_file(file)
                && !sources
                    .files
                    .iter()
                    .any(|source| source.borrow().file.id_path() == file))
    });

    let mut rendered = unframed
        .into_iter()
        .map(CompilerError::one_line)
        .join("\n")
        .into_bytes();
    if !framed.is_empty() {
        let mut cache = ariadne::sources(sources.files.iter().map(|source| {
            let source = source.borrow();
            (source.file.to_id_path(), source.contents.clone())
        }));
        for error in framed {
            if !rendered.is_empty() {
                rendered.push(b'\n');
            }
            error
                .report()
                .finish()
                .write(&mut cache, &mut rendered)
                .unwrap();
        }
    }
    String::from_utf8(rendered).unwrap()
}

impl From<CompilerError> for Vec<CompilerError> {
    fn from(e: CompilerError) -> Self {
        vec![e]
    }
}

impl std::fmt::Display for CompilerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_code::source_file_from_path_and_contents;
    use test_utils::strip_ansi_sgr;

    // Frame glyphs ariadne draws, escaped to keep this file ASCII.
    const OPEN: &str = "\u{256d}\u{2500}";
    const CLOSE: &str = "\u{2500}\u{2500}\u{2500}\u{256f}";
    const BAR: &str = "\u{2502}";

    const MAIN: &str = "fn main() {\n    let x = 1;\n}\n";

    fn files() -> SourceFiles {
        let mut files = SourceFiles::empty();
        for (path, contents) in [
            ("main.rock", MAIN),
            ("<stdlib>/core.rock", "fn core() {}\n"),
        ] {
            files.add_or_update_file(source_file_from_path_and_contents(
                path.to_string(),
                path.to_string(),
                contents.to_string(),
            ));
        }
        files
    }

    fn at(file: &str, line: usize, col: usize, offset: usize) -> Span {
        Span::from_linecol(LineCol {
            file: ustr::ustr(file),
            line,
            col,
            offset,
        })
    }

    fn labelled() -> CompilerError {
        CompilerError::new_with_label(at("main.rock", 1, 8, 17), "bad thing".to_string())
    }

    #[test]
    fn render_all_reports_unregistered_stdlib_errors_first_and_without_a_frame() {
        let stdlib =
            CompilerError::new_with_label(at("<stdlib>/absent.rock", 3, 4, 9), "gone".to_string());
        let rendered = strip_ansi_sgr(&render_all(&[labelled(), stdlib.clone()], &files()));
        assert_eq!(
            rendered,
            format!(
                "{}\n{}",
                strip_ansi_sgr(&stdlib.one_line()),
                strip_ansi_sgr(&labelled().render(&files()))
            )
        );
    }

    // A location-less error (`rock shader <file> <missing_root>`) has no file
    // to frame; ariadne would print "Unable to fetch source" and an empty box.
    #[test]
    fn render_all_reports_a_location_less_error_on_one_line() {
        let nowhere = CompilerError::new_no_location("shader root `nope` not found");
        let rendered = strip_ansi_sgr(&render_all(&[labelled(), nowhere], &files()));
        assert_eq!(
            rendered,
            format!(
                "{}\n{}",
                "[E00] Error: shader root `nope` not found",
                strip_ansi_sgr(&labelled().render(&files()))
            )
        );
    }

    #[test]
    fn labels_are_byte_ranges_of_a_multibyte_source() {
        let source = "fn main() {\n    let s = \"\u{e9}\u{e9}\";\n}\n";
        let mut files = SourceFiles::empty();
        files.add_or_update_file(source_file_from_path_and_contents(
            "mb.rock".to_string(),
            "mb.rock".to_string(),
            source.to_string(),
        ));
        let span = Span::new(ustr::ustr("mb.rock"), (1, 1), (13, 15), (25, 29));
        assert_eq!(&source[25..29], "\u{e9}\u{e9}");
        let error = CompilerError::new_with_label(span, "accented".to_string());
        assert_eq!(
            strip_ansi_sgr(&error.render(&files)),
            [
                "[E00] Error: accented".to_string(),
                format!("   {OPEN}[mb.rock:2:14]"),
                format!("   {BAR}"),
                format!(" 2 {BAR}     let s = \"\u{e9}\u{e9}\";"),
                format!("   {BAR}              \u{2500}\u{252c}  "),
                format!("   {BAR}               \u{2570}\u{2500}\u{2500} accented"),
                CLOSE.to_string(),
                String::new(),
            ]
            .join("\n")
        );
    }

    #[test]
    fn conversions_from_a_single_error_yield_one_element() {
        let error = labelled();
        let as_vec: Vec<CompilerError> = error.clone().into();
        assert_eq!(as_vec.len(), 1);
        assert_eq!(as_vec[0].message, error.message);
        assert_eq!(format!("{error}"), "bad thing");
    }
}
