use std::fmt::Debug;
use std::sync::LazyLock;

use crate::prelude::*;

pub(crate) const UNKNOWN_FILE_NAME: &str = "<unknown>";

/// `Span::unknown()` runs on every trial-unify path that ends up succeeding, so
/// the sentinel name is interned here once instead of being hashed into the
/// interner on each call.
static UNKNOWN_FILE: LazyLock<ustr::Ustr> = LazyLock::new(|| ustr::ustr(UNKNOWN_FILE_NAME));

/// Compiler-owned package identity for a standalone source. This prefix is
/// outside the identifier grammar, so a manifest name cannot claim it.
pub const STANDALONE_PACKAGE_PREFIX: &str = "@/";

pub fn display_name(key: &str) -> &str {
    key.strip_prefix(STANDALONE_PACKAGE_PREFIX)
        .and_then(|rest| rest.split_once("::"))
        .map_or(key, |(_, name)| name)
}

pub fn artifact_name(key: &str) -> &str {
    key.strip_prefix(STANDALONE_PACKAGE_PREFIX)
        .and_then(|rest| rest.split_once("::"))
        .filter(|(_, name)| !name.contains("::"))
        .map_or(key, |(_, name)| name)
}

pub(crate) fn display_diagnostic_names(mut message: String) -> String {
    while let Some(start) = message.find(STANDALONE_PACKAGE_PREFIX) {
        let Some(end) = message[start..].find("::") else {
            break;
        };
        message.replace_range(start..start + end + 2, "");
    }
    message
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GlobalPolicy {
    #[default]
    Any,
    None,
}

impl GlobalPolicy {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "any" => Some(Self::Any),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    pub const NAMES: [&'static str; 2] = ["any", "none"];
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum IdentityStorage {
    #[default]
    Enforced,
    Exempt,
    ExemptModules(Vec<String>),
}

impl IdentityStorage {
    pub const NAMES: [&'static str; 2] = ["enforced", "exempt"];

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "enforced" => Some(Self::Enforced),
            "exempt" => Some(Self::Exempt),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PackageTag {
    pub id: u32,
    pub self_name: String,
    pub identity_storage: IdentityStorage,
    pub root: String,
}

impl PackageTag {
    pub fn logical_file(&self, file: &str) -> Option<String> {
        let rest = file.strip_prefix(self.root.as_str())?;
        let rest = rest.strip_prefix(['/', '\\'])?;
        Some(format!("{}/{}", self.self_name, rest.replace('\\', "/")))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceFile {
    pub contents: String,
    pub file: FileContents,
    pub package: Option<PackageTag>,
    pub global_policy: GlobalPolicy,
}

pub fn source_file_from_path_and_contents(
    path: String,
    filename: String,
    contents: String,
) -> SourceFile {
    let file = FileContents::Filesystem { path, filename };
    SourceFile {
        file,
        contents,
        package: None,
        global_policy: GlobalPolicy::Any,
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Span {
    pub file: Ustr,
    line_range: (u32, u32),
    /// Zero-based, half-open `[start..end)`, counted in characters: `col_range.1` is never
    /// inclusive (`lsp_range_from_span` depends on it).
    col_range: (u32, u32),
    /// Zero-based, half-open byte range; indexes a `&str` directly. Bytes, not characters, so it
    /// drifts from character positions after any multibyte character.
    offset_range: (u32, u32),
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
        // Hashed at `usize` width so hashes, and the order of anything
        // iterated by them, match what wider spans produced.
        self.file.hash(state);
        self.line_range().hash(state);
        self.col_range().hash(state);
        self.offset_range().hash(state);
    }
}

impl Default for Span {
    fn default() -> Self {
        Self::unknown()
    }
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Span>() == 32);

fn narrow((first, last): (usize, usize)) -> (u32, u32) {
    match (u32::try_from(first), u32::try_from(last)) {
        (Ok(first), Ok(last)) => (first, last),
        _ => panic!(
            "source position {first}..{last} does not fit a span; \
             the tokenizer refuses sources this large"
        ),
    }
}

fn widen((first, last): (u32, u32)) -> (usize, usize) {
    (first as usize, last as usize)
}

impl Span {
    pub fn new(
        file: Ustr,
        line_range: (usize, usize),
        col_range: (usize, usize),
        offset_range: (usize, usize),
    ) -> Self {
        Self {
            file,
            line_range: narrow(line_range),
            col_range: narrow(col_range),
            offset_range: narrow(offset_range),
        }
    }

    #[inline]
    pub fn line_range(&self) -> (usize, usize) {
        widen(self.line_range)
    }

    #[inline]
    pub fn col_range(&self) -> (usize, usize) {
        widen(self.col_range)
    }

    #[inline]
    pub fn offset_range(&self) -> (usize, usize) {
        widen(self.offset_range)
    }

    pub fn set_line_range(&mut self, line_range: (usize, usize)) {
        self.line_range = narrow(line_range);
    }

    pub fn set_col_range(&mut self, col_range: (usize, usize)) {
        self.col_range = narrow(col_range);
    }

    pub fn set_offset_range(&mut self, offset_range: (usize, usize)) {
        self.offset_range = narrow(offset_range);
    }

    pub fn with_line_range(mut self, line_range: (usize, usize)) -> Self {
        self.set_line_range(line_range);
        self
    }

    pub fn with_col_range(mut self, col_range: (usize, usize)) -> Self {
        self.set_col_range(col_range);
        self
    }

    pub fn with_offset_range(mut self, offset_range: (usize, usize)) -> Self {
        self.set_offset_range(offset_range);
        self
    }

    pub fn unknown() -> Self {
        Self::from_linecol(LineCol::unknown())
    }

    pub fn format_site(&self) -> String {
        format!("{}:{}", self.file.as_str(), self.line_range().0 + 1)
    }

    pub fn from_linecol(linecol: LineCol) -> Self {
        Self::new(
            linecol.file,
            (linecol.line, linecol.line),
            (linecol.col, linecol.col),
            (linecol.offset, linecol.offset),
        )
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

    pub fn contains(&self, inner: Span) -> bool {
        self.file == inner.file
            && self.offset_range.0 <= inner.offset_range.0
            && inner.offset_range.1 <= self.offset_range.1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        let (first, last) = self.offset_range();
        last - first
    }

    pub fn start(&self) -> LineCol {
        let (line, col, offset) = (
            self.line_range().0,
            self.col_range().0,
            self.offset_range().0,
        );
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
    pub line: usize,
    pub col: usize,
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
        Span::new(ustr::ustr(FILE), line_range, col_range, offset_range)
    }

    #[test]
    #[should_panic(expected = "does not fit a span")]
    fn a_span_past_u32_positions_is_refused_at_construction() {
        let past = u32::MAX as usize + 1;
        Span::new(ustr::ustr(FILE), (0, 0), (0, 0), (0, past));
    }

    #[test]
    fn the_unknown_span_is_all_zeroes_in_the_sentinel_file() {
        let unknown = Span::unknown();
        assert_eq!(unknown.file.as_str(), "<unknown>");
        assert_eq!(
            (
                unknown.line_range(),
                unknown.col_range(),
                unknown.offset_range()
            ),
            ((0, 0), (0, 0), (0, 0))
        );
        assert_eq!(Span::default(), unknown);
        assert_eq!(LineCol::unknown().file, unknown.file);
        assert!(unknown.is_empty());
        assert_eq!(unknown.len(), 0);
    }

    #[test]
    fn len_and_start_read_the_ranges() {
        let single = span((1, 1), (5, 8), (17, 20));
        assert!(!single.is_empty());
        assert_eq!(single.len(), 3);
        assert_eq!(
            span((2, 4), (3, 9), (20, 41)).start(),
            line_col(FILE, 2, 3, 20)
        );
    }

    #[test]
    fn a_span_round_trips_through_its_start() {
        let point = line_col(FILE, 7, 11, 96);
        let converted = Span::from_linecol(point.clone());
        assert_eq!(converted.line_range(), (7, 7));
        assert_eq!(converted.col_range(), (11, 11));
        assert_eq!(converted.offset_range(), (96, 96));
        assert_eq!(converted.start(), point);
        assert!(converted.is_empty());
    }

    #[test]
    fn merging_keeps_the_first_start_and_the_last_end() {
        let first = span((1, 2), (4, 7), (13, 22));
        let last = span((3, 5), (0, 6), (30, 44));
        let merged = first.merge(&last);
        assert_eq!(merged.line_range(), (1, 5));
        assert_eq!(merged.col_range(), (4, 6));
        assert_eq!(merged.offset_range(), (13, 44));
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
        assert_eq!(file.global_policy, GlobalPolicy::Any);
    }
}
