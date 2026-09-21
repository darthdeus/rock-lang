use std::fmt::Debug;
use std::sync::LazyLock;

use crate::prelude::*;

/// The sentinel file name for spans/linecols with no real source location.
/// Interned once: `Span::unknown()` is called on every error-free trial-unify
/// path, and `"<unknown>".into()` re-hashes the ustr interner each time.
static UNKNOWN_FILE: LazyLock<ustr::Ustr> = LazyLock::new(|| ustr::ustr("<unknown>"));

/// Source dialect of one file. Legacy is today's grammar and flat lookup;
/// `V1` opts a package into the modules-v1 grammar (`::` paths, `mod`/`use`,
/// visibility). Selected per package by its stick.toml
/// (`[language] modules = "v1"`), never per file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Dialect {
    #[default]
    Legacy,
    V1,
}

/// The memory class of a compilation unit: which store a `global` declared in
/// it belongs to when the declaration says nothing itself
/// (`docs/state-ownership-plan.md` R2). Selected per package by its stick.toml
/// (`[language] memory = "platform"`) and per module by a `@memory(<class>)`
/// file header; a per-declaration `@state` still wins over both.
///
/// `Game` is today's default for every unannotated global: captured in the
/// WorldImage, hashed for desync, fresh per Session. `Platform` is host-owned
/// state that survives a restore and is excluded from the hash. `Debug` is
/// tools and diagnostics, handed forward or reset at a worker boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MemoryClass {
    #[default]
    Game,
    Platform,
    Debug,
}

impl MemoryClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Game => "game",
            Self::Platform => "platform",
            Self::Debug => "debug",
        }
    }

    /// Parse a declared class name. `None` is an unknown spelling; every caller
    /// rejects rather than falling back to `Game`, so a typo cannot silently
    /// move a package's state into the world plan.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "game" => Some(Self::Game),
            "platform" => Some(Self::Platform),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }

    pub const NAMES: [&'static str; 3] = ["game", "platform", "debug"];
}

/// Whether a package may declare mutable global storage. The default keeps
/// today's language behavior; `none` is the package boundary used by a
/// simulation library whose state must be reachable from its explicit root.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GlobalPolicy {
    #[default]
    Any,
    None,
}

impl GlobalPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::None => "none",
        }
    }

    /// Parse a declared policy. Unknown values are rejected by the manifest
    /// loader instead of silently restoring the permissive default.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "any" => Some(Self::Any),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    pub const NAMES: [&'static str; 2] = ["any", "none"];
}

/// The package a file belongs to, minted by the host's PackageGraph.
/// `id` is dense and stable within one graph build; `self_name` is the
/// manifest name with `-` normalized to `_`, case preserved.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PackageTag {
    pub id: u32,
    pub self_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceFile {
    pub module_id: ModuleId,
    pub contents: String,
    pub file: FileContents,
    pub is_script: bool,
    pub is_test: bool,
    pub is_test_util: bool,
    pub used_packages: Vec<String>,
    /// Owning package and source dialect, stamped by the loader that resolved
    /// the package graph. `None`/`Legacy` for files loaded outside a package
    /// (standalone scripts, editor synthetic sources).
    pub package: Option<PackageTag>,
    pub dialect: Dialect,
    /// The owning package's declared memory class, stamped by the same loader
    /// that stamps `package`/`dialect`. `Game` for files loaded outside a
    /// package, which keeps a standalone script on today's default.
    pub memory_class: MemoryClass,
    /// The owning package's mutable-global policy, stamped by the same loader
    /// as `package`, `dialect` and `memory_class`.
    pub global_policy: GlobalPolicy,
}

/// Build a `SourceFile` from an on-disk path + already-read contents. The
/// canonical constructor; skull-parser + the rock-cli prelude re-export it.
pub fn source_file_from_path_and_contents(
    path: String,
    filename: String,
    contents: String,
) -> SourceFile {
    let file = FileContents::Filesystem { path, filename };
    let module_id = ModuleId::new(file.to_id_path().into());
    SourceFile {
        module_id,
        file,
        contents,
        is_script: false,
        is_test: false,
        is_test_util: false,
        used_packages: vec![],
        package: None,
        dialect: Dialect::Legacy,
        memory_class: MemoryClass::Game,
        global_policy: GlobalPolicy::Any,
    }
}

/// Represents a region of source code.
/// Spans are fundamental for error reporting, language server features (like completion, hover, go-to-definition),
/// and mapping analysis results back to the original code.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Span {
    /// The identifier for the source file (e.g., path or URI).
    pub file: ustr::Ustr,
    /// The start and end line numbers (0-based, inclusive on both ends —
    /// a single-line token has `line_range.0 == line_range.1`).
    pub line_range: (usize, usize),
    /// The start and end column numbers, 0-based, half-open `[start..end)`.
    /// `col_range.1` is the column *after* the last character of the span,
    /// matching `offset_range`'s semantics and LSP `Range.end`. A token
    /// "abc" starting at col 5 has `col_range = (5, 8)`. This is what
    /// `Tokenizer::end_span` produces and what `lsp_range_from_span`
    /// relies on; do not treat `col_range.1` as inclusive.
    pub col_range: (usize, usize),
    /// The start and end byte offsets (0-based, half-open range `[start..end)`).
    /// This is the most precise representation of the span's extent: it indexes
    /// `&str` directly (`Tokenizer::byte_offset` advances by `c.len_utf8()` per
    /// char). Do NOT treat it as a char index -- that drifts past any earlier
    /// multibyte char. See `docs/span-position-units.md`.
    pub offset_range: (usize, usize),
}

impl Debug for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.file == "<unknown>" {
            f.write_str("Span(unknown)")
        } else {
            f.write_str(&format!("Span({})", self.file))
        }
    }
}

impl Hash for Span {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.file.hash(state);
        self.line_range.hash(state);
        self.col_range.hash(state);
        self.offset_range.hash(state);
    }
}

impl Default for Span {
    fn default() -> Self {
        Self::unknown()
    }
}

impl Span {
    pub fn unknown() -> Self {
        Self {
            file: *UNKNOWN_FILE,
            // file: "<unknown>".into(),  // slow path: flip to reproduce the
            // ustr-interner lock contention (docs/bone-profiling.md).
            line_range: (0, 0),
            col_range: (0, 0),
            offset_range: (0, 0),
        }
    }

    /// Format as `"file:line"` for `__SITE__` and similar source-position
    /// literals. 1-based line for user display.
    pub fn format_site(&self) -> String {
        format!("{}:{}", self.file.as_str(), self.line_range.0 + 1)
    }

    pub fn from_linecol(linecol: LineCol) -> Self {
        Self {
            file: linecol.file,
            line_range: (linecol.line, linecol.line),
            col_range: (linecol.col, linecol.col),
            offset_range: (linecol.offset, linecol.offset),
        }
    }

    pub fn panic(&self, message: impl AsRef<str>) -> Self {
        self.print(message.as_ref());
        panic!("{}", message.as_ref());
    }

    pub fn merge(&self, other: &Self) -> Self {
        assert_eq!(self.file, other.file);
        Self {
            file: self.file,
            line_range: (self.line_range.0, other.line_range.1),
            col_range: (self.col_range.0, other.col_range.1),
            offset_range: (self.offset_range.0, other.offset_range.1),
        }
    }

    pub fn or(self, other: &Self) -> Self {
        if self.file == "<unknown>" {
            *other
        } else {
            self
        }
    }

    pub fn is_empty(&self) -> bool {
        self.offset_range.0 == self.offset_range.1
    }

    pub fn len(&self) -> usize {
        let (first, last) = self.offset_range;
        last - first
    }

    pub fn start(&self) -> LineCol {
        let line = self.line_range.0;
        let col = self.col_range.0;
        let offset = self.offset_range.0;
        LineCol { file: self.file, line, col, offset }
    }

    pub fn end(&self) -> LineCol {
        let line = self.line_range.1;
        let col = self.col_range.1;
        let offset = self.offset_range.1;
        LineCol { file: self.file, line, col, offset }
    }

    pub fn contains_loc(&self, loc: &LineCol) -> bool {
        let file_match = loc.file == self.file;
        let loc_match = loc.offset >= self.offset_range.0 && loc.offset < self.offset_range.1;

        // if !file_match && loc_match {
        //     info!("File mismatch: {:?} != {:?}", loc.file, self.file);
        // }

        file_match && loc_match
    }

    /// Source text for ariadne's frame in the `print`/`write` debug
    /// helpers. Empty for the `unknown` sentinel file or if the file
    /// can't be read -- ariadne then renders without a source frame,
    /// which is the right degraded behavior for a debug-print path
    /// (panicking in a diagnostic printer would be worse).
    fn report_source(&self) -> String {
        if self.file == ustr::ustr("unknown") {
            String::new()
        } else {
            fs_err::read_to_string(self.file.as_str()).unwrap_or_else(|_| String::new())
        }
    }

    pub fn print(&self, message: impl AsRef<str>) {
        let message = message.as_ref().to_string();

        let mut report = make_report_builder(
            ariadne::ReportKind::Advice,
            self.file.to_string(),
            self.offset_range.0,
            "".to_string(),
            message.clone(),
        );

        report.add_span_label(self, message);

        let source = self.report_source();

        report
            .finish()
            .eprint((
                self.file.to_string(),
                ariadne::Source::from(source),
            ))
            .unwrap();
    }

    pub fn write(&self, message: impl AsRef<str>, w: impl std::io::Write) {
        let message = message.as_ref().to_string();

        let mut report = make_report_builder(
            ariadne::ReportKind::Advice,
            self.file.to_string(),
            self.offset_range.0,
            "".to_string(),
            message.clone(),
        );

        report.add_span_label(self, message);

        let source = self.report_source();

        report
            .finish()
            .write(
                (
                    self.file.to_string(),
                    ariadne::Source::from(source),
                ),
                w,
            )
            .unwrap();
    }

}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FileContents {
    Filesystem { path: String, filename: String },
    Code { id_path: String, contents: String },
}

impl FileContents {
    pub fn to_id_path(&self) -> String {
        match self {
            FileContents::Filesystem { path, .. } => path.clone(),
            FileContents::Code { id_path, .. } => id_path.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineCol {
    pub file: ustr::Ustr,
    /// Zero-based line number.
    pub line: usize,
    /// Zero-based column, counted in characters from the start of the line.
    pub col: usize,
    /// Byte offset from the start of the file.
    pub offset: usize,
}

impl LineCol {
    pub fn unknown() -> Self {
        Self {
            file: *UNKNOWN_FILE,
            // file: "<unknown>".into(),
            line: 0,
            col: 0,
            offset: 0,
        }
    }
}
