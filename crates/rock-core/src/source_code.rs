use std::fmt::Debug;
use std::sync::LazyLock;

use crate::prelude::*;

/// File name that stands in for a source position which does not exist.
pub(crate) const UNKNOWN_FILE_NAME: &str = "<unknown>";

/// `Span::unknown()` runs on every trial-unify path that ends up succeeding, so
/// the sentinel name is interned here once instead of being hashed into the
/// interner on each call.
static UNKNOWN_FILE: LazyLock<ustr::Ustr> = LazyLock::new(|| ustr::ustr(UNKNOWN_FILE_NAME));

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

/// A package's identity-storage migration state, declared by its manifest
/// (`[language] identity_storage`). Unmigrated code is exempt: its sites are
/// counted instead of reported. Absent means enforced.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum IdentityStorage {
    #[default]
    Enforced,
    /// The whole package is unmigrated.
    Exempt,
    /// Modules under these `::`-joined module path prefixes are unmigrated.
    /// A prefix covers the module it names and every module below it.
    ExemptModules(Vec<String>),
}

impl IdentityStorage {
    pub const NAMES: [&'static str; 2] = ["enforced", "exempt"];

    /// Parse a declared whole-package state. Unknown values are rejected by
    /// the manifest loader.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "enforced" => Some(Self::Enforced),
            "exempt" => Some(Self::Exempt),
            _ => None,
        }
    }
}

/// The package a file belongs to, minted by the host's PackageGraph.
/// `id` is dense and stable within one graph build; `self_name` is the
/// manifest name with `-` normalized to `_`, case preserved.
/// `identity_storage` is the manifest's declaration, carried with the tag so
/// every file of the package reaches the checker with it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PackageTag {
    pub id: u32,
    pub self_name: String,
    pub identity_storage: IdentityStorage,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceFile {
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
    /// The owning package's mutable-global policy, stamped by the same loader
    /// as `package` and `dialect`.
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
    SourceFile {
        file,
        contents,
        is_script: false,
        is_test: false,
        is_test_util: false,
        used_packages: vec![],
        package: None,
        dialect: Dialect::Legacy,
        global_policy: GlobalPolicy::Any,
    }
}

/// A region of source code.
///
/// Spans are what ties analysis results back to the text the user wrote: error
/// reporting, completion, hover and go-to-definition all start from one.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Span {
    /// Which source file the region belongs to (a path or another id path).
    pub file: Ustr,
    /// First and last line, zero-based and inclusive at both ends, so a
    /// single-line span has `line_range.0 == line_range.1`.
    pub line_range: (usize, usize),
    /// First and one-past-last column, zero-based and half-open `[start..end)`.
    /// `col_range.1` is therefore the column just past the last character of
    /// the span, the same convention `offset_range` and LSP `Range.end` use:
    /// a three-character token opening at column 5 has `col_range == (5, 8)`.
    /// `Tokenizer::end_span` produces it that way and `lsp_range_from_span`
    /// depends on it, so `col_range.1` is never inclusive.
    pub col_range: (usize, usize),
    /// First and one-past-last byte, zero-based and half-open `[start..end)`.
    /// The precise extent: these offsets index a `&str` directly, since
    /// `Tokenizer::byte_offset` advances by `c.len_utf8()` per character.
    /// They are byte counts, not character counts, and drift from character
    /// positions as soon as a multibyte character appears earlier in the file.
    pub offset_range: (usize, usize),
}

impl Debug for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.file == UNKNOWN_FILE_NAME {
            f.write_str("Span(unknown)")
        } else {
            write!(f, "Span({})", self.file)
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
        Self::from_linecol(LineCol::unknown())
    }

    /// `"file:line"` with a one-based line, for `__SITE__` and the like.
    pub fn format_site(&self) -> String {
        format!("{}:{}", self.file.as_str(), self.line_range.0 + 1)
    }

    /// The zero-width span of one position.
    pub fn from_linecol(linecol: LineCol) -> Self {
        Self {
            file: linecol.file,
            line_range: (linecol.line, linecol.line),
            col_range: (linecol.col, linecol.col),
            offset_range: (linecol.offset, linecol.offset),
        }
    }

    /// The smallest span covering both, which must belong to one file.
    pub fn merge(&self, other: &Self) -> Self {
        assert_eq!(self.file, other.file);
        Self {
            file: self.file,
            line_range: (self.line_range.0, other.line_range.1),
            col_range: (self.col_range.0, other.col_range.1),
            offset_range: (self.offset_range.0, other.offset_range.1),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        let (first, last) = self.offset_range;
        last - first
    }

    pub fn start(&self) -> LineCol {
        let (line, col, offset) = (self.line_range.0, self.col_range.0, self.offset_range.0);
        LineCol {
            file: self.file,
            line,
            col,
            offset,
        }
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
    pub file: Ustr,
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
            line: 0,
            col: 0,
            offset: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "main.rock";

    fn line_col(file: &str, line: usize, col: usize, offset: usize) -> LineCol {
        LineCol {
            file: ustr::ustr(file),
            line,
            col,
            offset,
        }
    }

    fn span(
        line_range: (usize, usize),
        col_range: (usize, usize),
        offset_range: (usize, usize),
    ) -> Span {
        Span {
            file: ustr::ustr(FILE),
            line_range,
            col_range,
            offset_range,
        }
    }

    #[test]
    fn the_unknown_span_is_all_zeroes_in_the_sentinel_file() {
        let unknown = Span::unknown();
        assert_eq!(unknown.file.as_str(), "<unknown>");
        assert_eq!(
            (unknown.line_range, unknown.col_range, unknown.offset_range),
            ((0, 0), (0, 0), (0, 0))
        );
    }

    #[test]
    fn the_default_span_is_the_unknown_span() {
        assert_eq!(Span::default(), Span::unknown());
        assert_eq!(LineCol::unknown().file, Span::unknown().file);
        assert_eq!(
            (
                LineCol::unknown().line,
                LineCol::unknown().col,
                LineCol::unknown().offset
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn an_unknown_span_is_empty_and_has_no_length() {
        assert!(Span::unknown().is_empty());
        assert_eq!(Span::unknown().len(), 0);
    }

    #[test]
    fn a_span_of_one_character_is_not_empty() {
        let single = span((1, 1), (5, 8), (17, 20));
        assert!(!single.is_empty());
        assert_eq!(single.len(), 3);
    }

    #[test]
    fn start_reads_each_range_at_its_start() {
        let covered = span((2, 4), (3, 9), (20, 41));
        assert_eq!(covered.start(), line_col(FILE, 2, 3, 20));
    }

    #[test]
    fn a_span_round_trips_through_its_start() {
        let point = line_col(FILE, 7, 11, 96);
        let converted = Span::from_linecol(point.clone());
        assert_eq!(converted.file, point.file);
        assert_eq!(converted.line_range, (7, 7));
        assert_eq!(converted.col_range, (11, 11));
        assert_eq!(converted.offset_range, (96, 96));
        assert_eq!(converted.start(), point);
        assert!(converted.is_empty());
    }

    #[test]
    fn merging_keeps_the_first_start_and_the_last_end() {
        let first = span((1, 2), (4, 7), (13, 22));
        let last = span((3, 5), (0, 6), (30, 44));
        let merged = first.merge(&last);
        assert_eq!(merged.line_range, (1, 5));
        assert_eq!(merged.col_range, (4, 6));
        assert_eq!(merged.offset_range, (13, 44));
        assert_eq!(merged.file, first.file);
    }

    #[test]
    #[should_panic]
    fn merging_spans_of_different_files_is_rejected() {
        let mut other = span((0, 0), (0, 1), (0, 1));
        other.file = ustr::ustr("other.rock");
        span((0, 0), (0, 1), (0, 1)).merge(&other);
    }

    #[test]
    fn format_site_is_one_based_on_the_line() {
        assert_eq!(span((0, 0), (0, 1), (0, 1)).format_site(), "main.rock:1");
        assert_eq!(span((6, 7), (0, 1), (0, 1)).format_site(), "main.rock:7");
        assert_eq!(Span::unknown().format_site(), "<unknown>:1");
    }

    #[test]
    fn debug_names_the_file_but_not_the_ranges() {
        assert_eq!(
            format!("{:?}", span((1, 2), (3, 4), (5, 6))),
            "Span(main.rock)"
        );
        assert_eq!(format!("{:?}", Span::unknown()), "Span(unknown)");
    }

    #[test]
    fn a_source_file_defaults_to_a_library_file_of_its_own_module() {
        let file = source_file_from_path_and_contents(
            "src/main.rock".to_string(),
            "main.rock".to_string(),
            "fn main() {}\n".to_string(),
        );
        assert_eq!(file.file.to_id_path(), "src/main.rock");
        assert_eq!(file.contents, "fn main() {}\n");
        assert!(!file.is_script);
        assert!(!file.is_test);
        assert!(!file.is_test_util);
        assert!(file.used_packages.is_empty());
        assert_eq!(file.dialect, Dialect::Legacy);
        assert_eq!(file.global_policy, GlobalPolicy::Any);
    }
}
