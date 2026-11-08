use crate::prelude::*;
use crate::source_files::SourceFiles;
use crate::utils::{ReportBuilderExt, make_report_builder};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CompilerErrorLevel {
    Advice,
    Warning,
    Error,
}

impl CompilerErrorLevel {
    /// How ariadne labels a report of this level.
    fn report_kind(self) -> ariadne::ReportKind<'static> {
        match self {
            Self::Advice => ariadne::ReportKind::Advice,
            Self::Warning => ariadne::ReportKind::Warning,
            Self::Error => ariadne::ReportKind::Error,
        }
    }

    /// First letter of the numeric code shown after the level word.
    fn initial(self) -> &'static str {
        match self {
            Self::Advice => "A",
            Self::Warning => "W",
            Self::Error => "E",
        }
    }

    /// The coloured `[E00] Error:` prefix of a one-line rendering. The code in
    /// it is always `00`: the one-liner reports the level, not the code.
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

    /// A `nobox` value materialized implicitly. Distinct from a plain codegen
    /// rejection because it is a LANGUAGE GUARANTEE: the stdlib emit path may
    /// downgrade a codegen parity gap to a silent `skipped`, and must never
    /// downgrade this (a dropped body is a nil global at runtime).
    NoboxGuaranteeViolated,
}

#[derive(Clone, Debug)]
pub struct CompilerError {
    pub location: LineCol,
    /// What went wrong, for callers that branch on the failure.
    pub kind: CompilerErrorKind,
    pub code: CompilerErrorCode,
    /// Extra marked ranges, rendered in the order they were added.
    pub labels: Vec<CompilerLabel>,
    pub level: CompilerErrorLevel,
    pub message: String,
    /// Independent failures from the same pass, carried so that a caller able
    /// to return only one error still reports all of them. Body emission runs
    /// in parallel and rejects every body before returning any of them, so
    /// without this a caller would see one rejection and read the program as
    /// having one problem. A renderer emits them after the primary one, and
    /// they never nest: a sibling carries no siblings of its own.
    pub siblings: Vec<CompilerError>,
}

impl CompilerError {
    fn at(location: LineCol, message: String) -> Self {
        let code = CompilerErrorCode::Unknown;
        let level = CompilerErrorLevel::Error;
        CompilerError {
            location,
            kind: CompilerErrorKind::Generic,
            code,
            labels: Vec::new(),
            level,
            message,
            siblings: Vec::new(),
        }
    }

    pub fn new_no_location(message: impl Into<String>) -> Self {
        Self::new_with_label(Span::unknown(), message.into())
    }

    /// A span-only error with no message, for trial paths -- overload scoring,
    /// `coerce(..).is_ok()` -- that only test whether the call succeeded. They
    /// throw the error away unrendered and would otherwise pay for the
    /// recursive type rendering the real failure arms build.
    pub fn silent(span: Span) -> Self {
        Self::at(span.start(), String::new())
    }

    /// Take over further independent errors from the same pass. Nested
    /// siblings are lifted out of whatever carried them, so the flat shape
    /// holds no matter how the inputs were assembled.
    pub fn with_siblings(mut self, others: impl IntoIterator<Item = CompilerError>) -> Self {
        for mut other in others {
            let nested = std::mem::take(&mut other.siblings);
            self.siblings.push(other);
            self.siblings.extend(nested);
        }
        self
    }

    pub fn panic(self) -> CompilerError {
        self.labels.first().unwrap().span.panic(&self.message);
        self
    }

    pub fn new_with_label(span: Span, message: String) -> Self {
        Self::at(span.start(), message.clone()).with_label(span, message)
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
        self.labels.push(CompilerLabel { span, message });
        self
    }

    /// A `[E00] Error: message at file:line:col` line and nothing else. Line
    /// and column are the raw (zero-based) ones stored on the location.
    fn one_line(&self) -> String {
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

    /// A one-line rendering with no source frame, for low-severity diagnostics
    /// piped to a file: the frame excerpt costs most of the per-diagnostic
    /// time and is worth little where nobody looks at it. Goes through
    /// `colored`, which strips its escapes when stderr is not a terminal.
    pub fn render_compact(&self) -> String {
        self.one_line()
    }

    /// This error's ariadne report with no source cache attached, so a batch
    /// render can share one cache across every report it emits.
    fn report(&self) -> ariadne::ReportBuilder<'static, (String, std::ops::Range<usize>)> {
        let file = self.location.file;
        let offset = self.location.offset;
        let mut builder = make_report_builder(
            self.level.report_kind(),
            file.to_string(),
            offset,
            format!("{}{:02}", self.level.initial(), self.code as u8),
            self.message.clone(),
        );

        match self.labels.as_slice() {
            // No marked range: point at the single byte the error sits on.
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

/// Render `errs` against one shared ariadne source cache, separated by blank
/// lines.
///
/// Building a cache copies the text of every file in `sources` (~1.5 MB for a
/// whole project), so one cache per error costs about 1.6 ms each there. The
/// lint pass renders warnings after every successful build, so it must render
/// all of them against one cache; any multi-diagnostic render belongs here.
///
/// An error in a stdlib file whose contents the caller never supplied has no
/// source to frame and falls back to the one-line form. A caller that does
/// supply the stdlib (the CLI does) gets the same source report as for a user
/// file.
#[must_use]
pub fn render_all(errs: &[CompilerError], sources: &SourceFiles) -> String {
    if errs.is_empty() {
        return String::new();
    }

    let has_source = |error: &CompilerError| {
        sources
            .files
            .iter()
            .any(|source| source.borrow().file.to_id_path() == error.location.file.as_str())
    };
    let frameless = |error: &CompilerError| {
        error.location.file.as_str().contains("<stdlib>") && !has_source(error)
    };

    let (unframed, framed): (Vec<&CompilerError>, Vec<&CompilerError>) =
        errs.iter().partition(|error| frameless(error));

    let mut rendered: Vec<String> = unframed.iter().map(|error| error.one_line()).collect();
    if !framed.is_empty() {
        let contents = sources
            .files
            .iter()
            .map(|source| {
                let source = source.borrow();
                (source.file.to_id_path(), source.contents.clone())
            })
            .collect::<Vec<_>>();
        let mut cache = ariadne::sources(contents);
        for error in framed {
            let mut written = Vec::new();
            error
                .report()
                .finish()
                .write(&mut cache, &mut written)
                .unwrap();
            rendered.push(String::from_utf8(written).unwrap());
        }
    }
    rendered.join("\n")
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

pub trait ErrorExt<T> {
    fn to_compiler_error(self, span: Span) -> Result<T, CompilerError>;
}

impl<T> ErrorExt<T> for Result<T>
where
    T: std::fmt::Debug,
{
    fn to_compiler_error(self, span: Span) -> Result<T, CompilerError> {
        self.map_err(|e| CompilerError::new_with_label(span, format!("{:?}", e)))
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
    const TICK: &str = "\u{2570}\u{2500}";

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

    /// The frame body, i.e. everything after the `[Exx] Level: message` line.
    fn body(rendered: &str) -> &str {
        let start = rendered.find('\n').expect("header line") + 1;
        &rendered[start..]
    }

    /// A report as ariadne emits it: one line per entry, the last one empty so
    /// the joined text ends in a newline.
    fn joined(lines: &[String]) -> String {
        lines.join("\n")
    }

    #[test]
    fn render_draws_the_labelled_frame() {
        let rendered = strip_ansi_sgr(&labelled().render(&files()));
        assert_eq!(
            rendered,
            joined(&[
                "[E00] Error: bad thing".to_string(),
                format!("   {OPEN}[main.rock:2:6]"),
                format!("   {BAR}"),
                format!(" 2 {BAR}     let x = 1;"),
                format!("   {BAR}      {BAR} "),
                format!("   {BAR}      {TICK} bad thing"),
                CLOSE.to_string(),
                String::new(),
            ])
        );
    }

    #[test]
    fn render_points_at_the_location_without_any_label() {
        let error = CompilerError::at(at("main.rock", 1, 8, 17).start(), "no label".to_string());
        let rendered = strip_ansi_sgr(&error.render(&files()));
        assert_eq!(
            rendered,
            joined(&[
                "[E00] Error: no label".to_string(),
                format!("   {OPEN}[main.rock:2:6]"),
                format!("   {BAR}"),
                format!(" 2 {BAR}     let x = 1;"),
                CLOSE.to_string(),
                String::new(),
            ])
        );
    }

    #[test]
    fn render_keeps_every_label_in_the_order_it_was_added() {
        let error = labelled().with_label(at("main.rock", 1, 8, 17), "second".to_string());
        let rendered = strip_ansi_sgr(&error.render(&files()));
        let second = rendered.find("second").expect("second label");
        let first = rendered.find("bad thing").expect("first label");
        assert!(first < second, "{rendered}");
        assert_eq!(
            rendered,
            joined(&[
                "[E00] Error: bad thing".to_string(),
                format!("   {OPEN}[main.rock:2:6]"),
                format!("   {BAR}"),
                format!(" 2 {BAR}     let x = 1;"),
                format!("   {BAR}      {BAR} "),
                format!("   {BAR}      {TICK} bad thing"),
                format!("   {BAR}      {BAR} "),
                format!("   {BAR}      {TICK} second"),
                CLOSE.to_string(),
                String::new(),
            ])
        );
    }

    #[test]
    fn render_prefixes_the_level_and_a_two_digit_code() {
        let error = labelled();
        let rendered = strip_ansi_sgr(&error.render(&files()));
        assert_eq!(rendered.lines().next(), Some("[E00] Error: bad thing"));
        let rendered = strip_ansi_sgr(
            &error
                .clone()
                .with_code(CompilerErrorCode::SyntaxError)
                .render(&files()),
        );
        assert_eq!(rendered.lines().next(), Some("[E02] Error: bad thing"));
        let rendered = strip_ansi_sgr(
            &error
                .clone()
                .with_level(CompilerErrorLevel::Warning)
                .render(&files()),
        );
        assert_eq!(rendered.lines().next(), Some("[W00] Warning: bad thing"));
        let rendered = strip_ansi_sgr(
            &error
                .with_level(CompilerErrorLevel::Advice)
                .render(&files()),
        );
        assert_eq!(rendered.lines().next(), Some("[A00] Advice: bad thing"));
    }

    #[test]
    fn render_compact_uses_the_level_only_and_always_the_00_code() {
        let error = labelled();
        assert_eq!(
            strip_ansi_sgr(&error.render_compact()),
            "[E00] Error: bad thing at main.rock:1:8"
        );
        assert_eq!(
            strip_ansi_sgr(
                &error
                    .clone()
                    .with_level(CompilerErrorLevel::Warning)
                    .render_compact()
            ),
            "[W00] Warning: bad thing at main.rock:1:8"
        );
        assert_eq!(
            strip_ansi_sgr(
                &error
                    .clone()
                    .with_level(CompilerErrorLevel::Advice)
                    .render_compact()
            ),
            "[A00] Advice: bad thing at main.rock:1:8"
        );
        assert_eq!(
            strip_ansi_sgr(
                &error
                    .clone()
                    .with_code(CompilerErrorCode::TokenizerError)
                    .render_compact()
            ),
            "[E00] Error: bad thing at main.rock:1:8"
        );
        assert_eq!(
            strip_ansi_sgr(&CompilerError::new_no_location("nowhere".to_string()).render_compact()),
            "[E00] Error: nowhere"
        );
    }

    #[test]
    fn render_changes_only_the_header_with_the_level() {
        let error = labelled();
        let base = strip_ansi_sgr(&error.render(&files()));
        for level in [CompilerErrorLevel::Warning, CompilerErrorLevel::Advice] {
            let rendered = strip_ansi_sgr(&error.clone().with_level(level).render(&files()));
            assert_eq!(body(&rendered), body(&base));
        }
        let rendered = strip_ansi_sgr(
            &error
                .with_level(CompilerErrorLevel::Warning)
                .render(&files()),
        );
        assert_eq!(rendered.lines().next(), Some("[W00] Warning: bad thing"));
    }

    #[test]
    fn render_all_of_nothing_is_empty() {
        assert_eq!(render_all(&[], &files()), "");
    }

    #[test]
    fn render_all_separates_frames_by_one_blank_line() {
        let first = labelled();
        let second = CompilerError::new_with_label(at("main.rock", 2, 0, 29), "second".to_string());
        let rendered = strip_ansi_sgr(&render_all(&[first.clone(), second.clone()], &files()));
        assert_eq!(
            rendered,
            format!(
                "{}\n{}",
                strip_ansi_sgr(&first.render(&files())),
                strip_ansi_sgr(&second.render(&files()))
            )
        );
        assert_eq!(
            body(&rendered)
                .matches(&format!("{CLOSE}\n\n[E00]"))
                .count(),
            1
        );
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
                strip_ansi_sgr(&stdlib.render_compact()),
                strip_ansi_sgr(&labelled().render(&files()))
            )
        );
    }

    #[test]
    fn render_all_draws_a_frame_for_a_registered_stdlib_file() {
        let stdlib =
            CompilerError::new_with_label(at("<stdlib>/core.rock", 0, 3, 3), "stdlib".to_string());
        let rendered = strip_ansi_sgr(&render_all(&[stdlib], &files()));
        assert_eq!(
            rendered,
            joined(&[
                "[E00] Error: stdlib".to_string(),
                format!("   {OPEN}[<stdlib>/core.rock:1:4]"),
                format!("   {BAR}"),
                format!(" 1 {BAR} fn core() {{}}"),
                format!("   {BAR}    {BAR} "),
                format!("   {BAR}    {TICK} stdlib"),
                CLOSE.to_string(),
                String::new(),
            ])
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
        let span = Span {
            file: ustr::ustr("mb.rock"),
            line_range: (1, 1),
            col_range: (13, 15),
            offset_range: (25, 29),
        };
        assert_eq!(&source[25..29], "\u{e9}\u{e9}");
        let error = CompilerError::new_with_label(span, "accented".to_string());
        assert_eq!(
            strip_ansi_sgr(&error.render(&files)),
            joined(&[
                "[E00] Error: accented".to_string(),
                format!("   {OPEN}[mb.rock:2:14]"),
                format!("   {BAR}"),
                format!(" 2 {BAR}     let s = \"\u{e9}\u{e9}\";"),
                format!("   {BAR}              \u{2500}\u{252c}  "),
                format!("   {BAR}               \u{2570}\u{2500}\u{2500} accented"),
                CLOSE.to_string(),
                String::new(),
            ])
        );
    }

    #[test]
    fn siblings_are_flattened_and_stay_out_of_the_report() {
        let primary = labelled();
        let sibling =
            CompilerError::new_with_label(at("main.rock", 2, 0, 29), "sibling".to_string());
        let nested = CompilerError::new_with_label(at("main.rock", 2, 0, 29), "nested".to_string())
            .with_siblings(vec![sibling.clone()]);
        let error = primary.clone().with_siblings(vec![sibling, nested]);
        assert_eq!(
            error
                .siblings
                .iter()
                .map(|s| s.message.as_str())
                .collect::<Vec<_>>(),
            ["sibling", "nested", "sibling"]
        );
        assert!(error.siblings.iter().all(|s| s.siblings.is_empty()));
        assert_eq!(
            strip_ansi_sgr(&error.render(&files())),
            strip_ansi_sgr(&primary.render(&files()))
        );
        assert_eq!(
            strip_ansi_sgr(&error.render_compact()),
            strip_ansi_sgr(&primary.render_compact())
        );
    }

    #[test]
    fn silent_errors_carry_a_span_and_an_empty_message() {
        let span = at("main.rock", 1, 8, 17);
        let error = CompilerError::silent(span);
        assert_eq!(error.location, span.start());
        assert_eq!(error.message, "");
        assert!(error.labels.is_empty());
        assert_eq!(error.level, CompilerErrorLevel::Error);
    }

    fn fallible() -> anyhow::Result<u8> {
        Err(anyhow::anyhow!("boom"))
    }

    #[test]
    fn to_compiler_error_wraps_the_debug_rendering() {
        let error = fallible()
            .to_compiler_error(at("main.rock", 0, 0, 0))
            .unwrap_err();
        assert_eq!(error.message, "boom");
        assert_eq!(error.labels.len(), 1);
        assert_eq!(error.labels[0].message, "boom");
        assert_eq!(error.location.offset, 0);
    }

    #[test]
    fn conversions_from_a_single_error_yield_one_element() {
        let error = labelled();
        let as_vec: Vec<CompilerError> = error.clone().into();
        assert_eq!(as_vec.len(), 1);
        assert_eq!(as_vec[0].message, error.message);
        assert_eq!(format!("{error}"), "bad thing");
    }

    #[test]
    fn panic_prints_the_first_label_before_aborting() {
        let result = std::panic::catch_unwind(|| labelled().panic());
        assert!(result.is_err());
    }
}
