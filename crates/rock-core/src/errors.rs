use crate::prelude::*;
use crate::source_files::SourceFiles;
use crate::utils::{make_report_builder, ReportBuilderExt};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CompilerErrorLevel {
    Advice,
    Warning,
    Error,
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
    MissingMethod,
    MissingField,
    DefaultArgPosition,
    SymbolNotFound,
    DuplicateSymbol {
        name: String,
        first_location: Option<LineCol>,
    },

    MismatchedType {
        expected: TypeId,
        actual: TypeId,
    },

    WrongNumberOfArgs {
        expected: usize,
        actual: usize,
    },

    IndexingNonArrayType {
        actual: TypeId,
    },

    InvalidChainedCompoundAssign,

    IncompatibleNewtypeOperation,

    UnificationError {
        expected: TypeRef,
        actual: TypeRef,
    },

    SubtypeError {
        sup: TypeRef,
        sub: TypeRef,
    },

    InferenceError {
        unresolved_nodes: Vec<(AstNodeId, TypeRef)>,
    },

    DestructuringError,

    AmbiguousMethodCall,

    UnusedVariable {
        name: String,
    },

    UnreachableCode,

    UninitializedVariable {
        name: String,
    },

    ConstraintViolation,

    DuplicatePattern,

    NullAssignmentToNonNullable {
        type_name: String,
    },

    IntDivisionPromotionPrevented,

    CodegenError {
        message: String,
    },

    /// A `nobox` value materialized implicitly. Distinct from a plain codegen
    /// rejection because it is a LANGUAGE GUARANTEE: the stdlib emit path may
    /// downgrade a codegen parity gap to a silent `skipped`, and must never
    /// downgrade this (a dropped body is a nil global at runtime).
    NoboxGuaranteeViolated,

    // Control flow error kinds
    ControlFlowInSubexpression,
    MissingElseBranch,
    InvalidContinue,
    InvalidBreak,
}

#[derive(Clone, Debug)]
pub struct CompilerError {
    pub location: LineCol,
    /// What went wrong, for callers that branch on the failure.
    pub kind: CompilerErrorKind,
    pub code: CompilerErrorCode,
    pub message: String,
    /// Extra marked ranges, rendered in the order they were added.
    pub labels: Vec<CompilerLabel>,
    pub level: CompilerErrorLevel,
}

impl CompilerError {
    pub fn new_no_location(message: impl Into<String>) -> Self {
        Self::new_with_label(Span::unknown(), message.into())
    }

    /// A message-less error carrying only a span, allocating nothing. For
    /// trial/predicate paths (overload scoring, `coerce(..).is_ok()`) that
    /// discard the error without ever rendering it -- skips the expensive
    /// recursive type-render the real failure arms build. The empty message
    /// is never surfaced because these callers consume the `Result` as a bool.
    pub fn silent(span: Span) -> Self {
        Self::new(span.start(), String::new())
    }

    fn new(location: LineCol, message: String) -> Self {
        Self {
            location,
            code: CompilerErrorCode::Unknown,
            message,
            level: CompilerErrorLevel::Error,
            labels: Vec::new(),
            kind: CompilerErrorKind::Generic,
        }
    }

    pub fn panic(self) -> CompilerError {
        self.labels.first().unwrap().span.panic(&self.message);
        self
    }

    pub fn new_with_label(span: Span, message: String) -> Self {
        Self::new(span.start(), message.clone()).with_label(span, message)
    }

    pub fn with_kind(mut self, kind: CompilerErrorKind) -> Self {
        self.kind = kind;
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
        self.labels.push(CompilerLabel { span, message });
        self
    }

    /// Simple rendering without source file access - used for stdlib errors to avoid circular dependencies
    fn render_simple(&self) -> String {
        use colored::Colorize;

        let level_str = match self.level {
            CompilerErrorLevel::Error => "[E00] Error:".red().bold(),
            CompilerErrorLevel::Warning => "[W00] Warning:".yellow().bold(),
            CompilerErrorLevel::Advice => "[A00] Advice:".blue().bold(),
        };

        let location = if self.location.file.as_str() != "<unknown>" {
            format!(" at {}:{}:{}",
                    self.location.file,
                    self.location.line,
                    self.location.col)
        } else {
            String::new()
        };

        format!("{} {}{}", level_str, self.message, location)
    }

    /// Compact one-liner rendering (no ariadne source frame). Used for
    /// non-TTY output of low-severity diagnostics where the source-frame
    /// excerpt is the dominant cost (~830us per diagnostic) and adds
    /// little value to piped/redirected output.
    ///
    /// Unlike `render_simple`, this respects whether colored output is
    /// desired by going through `colored::Colorize` (which itself
    /// auto-detects TTY on stderr/stdout).
    pub fn render_compact(&self) -> String {
        // render_simple already produces a "[Level] message at file:line:col"
        // one-liner; reuse it. The `colored` crate auto-strips ANSI when
        // stderr is not a TTY.
        self.render_simple()
    }

    #[must_use]
    pub fn render(&self, sources: &SourceFiles) -> String {
        render_all(std::slice::from_ref(self), sources)
    }

    /// The ariadne report for this error, without a source cache attached.
    /// Split out so a batch render builds the cache once -- see [`render_all`].
    fn report_builder(&self) -> ariadne::ReportBuilder<'static, (String, std::ops::Range<usize>)> {
        let mut builder = make_report_builder(
            self.level.report_kind(),
            self.location.file.to_string(),
            self.location.offset,
            format!(
                "{}{:02}",
                match self.level {
                    CompilerErrorLevel::Error => "E",
                    CompilerErrorLevel::Warning => "W",
                    CompilerErrorLevel::Advice => "A",
                },
                self.code as u8,
            ),
            self.message.clone(),
        );

        match self.labels.as_slice() {
            [] => {
                let file = self.location.file.to_string();
                let start = self.location.offset;
                builder.add_label(ariadne::Label::new((file, start..start + 1)));
            },
            marked => {
                for label in marked {
                    builder.add_span_label(&label.span, label.message.clone());
                }
            },
        }
        builder
    }
}

/// Render every error in `errs` against ONE ariadne source cache, joined by
/// blank lines.
///
/// Building the cache copies the contents of every file in `sources` (a game
/// project is ~1.5 MB), so a per-error cache costs ~1.6 ms EACH on a big
/// project. That was invisible while rendering only ended a build; the lint
/// pass renders warnings on every successful build, which is what surfaced it.
/// Every multi-diagnostic report should come through here.
#[must_use]
pub fn render_all(errs: &[CompilerError], sources: &SourceFiles) -> String {
    if errs.is_empty() {
        return String::new();
    }
    // Errors in the stdlib render without source context (the stdlib is not in
    // `sources`, and rendering it risks a circular to_string dependency).
    let (stdlib, spanned): (Vec<&CompilerError>, Vec<&CompilerError>) =
        errs.iter().partition(|e| e.location.file.to_string().contains("<stdlib>"));

    let mut out: Vec<String> = stdlib.iter().map(|e| e.render_simple()).collect();
    if !spanned.is_empty() {
        let sources_vec = sources
            .files
            .iter()
            .map(|src| {
                let src = src.borrow();
                (src.file.to_id_path(), src.contents.clone())
            })
            .collect_vec();
        let mut cache = ariadne::sources(sources_vec);
        for e in spanned {
            let mut buf = Vec::new();
            e.report_builder().finish().write(&mut cache, &mut buf).unwrap();
            out.push(String::from_utf8(buf).unwrap());
        }
    }
    out.join("\n")
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

impl CompilerErrorLevel {
    fn report_kind(self) -> ariadne::ReportKind<'static> {
        match self {
            Self::Advice => ariadne::ReportKind::Advice,
            Self::Warning => ariadne::ReportKind::Warning,
            Self::Error => ariadne::ReportKind::Error,
        }
    }
}
