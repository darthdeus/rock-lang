/*!

Language Server Protocol types for Rust.

Based on: <https://microsoft.github.io/language-server-protocol/specification>

*/
#![allow(non_upper_case_globals)]
#![forbid(unsafe_code)]
#[macro_use]
mod macros;

use std::collections::HashMap;

use serde_json::Value;

pub use uri::Uri;
mod uri;

/// Declare the named values of an LSP integer enum and serialize it as that
/// integer.
macro_rules! lsp_enum {
    (impl $typ: ident { $( $(#[$attr:meta])* pub const $name: ident : $enum_type: ty = $value: expr; )* }) => {
        impl $typ {
            $(
            $(#[$attr])*
            pub const $name: $enum_type = $value;
            )*
        }

        impl serde::Serialize for $typ {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                self.0.serialize(s)
            }
        }

        impl<'de> serde::Deserialize<'de> for $typ {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Ok(Self(i32::deserialize(d)?))
            }
        }
    }
}

pub mod request;

mod code_action;
pub use code_action::*;

mod completion;
pub use completion::*;

mod document_highlight;
pub use document_highlight::*;

mod document_symbols;
pub use document_symbols::*;

mod hover;
pub use hover::*;

mod inlay_hint;
pub use inlay_hint::*;

mod rename;
pub use rename::*;

mod semantic_tokens;
pub use semantic_tokens::*;

mod signature_help;
pub use signature_help::*;

mod window;
pub use window::*;

/* ----------------- Auxiliary types ----------------- */

#[derive(Debug, Eq, Hash, PartialEq, Clone)]
pub enum NumberOrString {
    Number(i32),
    String(String),
}

impl_untagged_enum!(NumberOrString { Number, String });

impl<'de> serde::Deserialize<'de> for NumberOrString {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) => Ok(NumberOrString::String(s)),
            other => serde_json::from_value(other)
                .map(NumberOrString::Number)
                .map_err(serde::de::Error::custom),
        }
    }
}

lsp_struct! {
    both
    /// Zero-based line and character offset in a text document.
    /// A position is between characters, like an editor's insertion cursor.
    #[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Copy, Clone, Default, Hash)]
    pub struct Position {
        required pub line: u32 = "line",

        /// UTF-16 offset for Rock LSP. Out-of-range columns clamp to the line.
        required pub character: u32 = "character",
    }
}

impl Position {
    pub fn new(line: u32, character: u32) -> Position {
        Position { line, character }
    }
}

lsp_struct! {
    both
    /// Zero-based start and end positions in a text document. The end is exclusive.
    #[derive(Debug, Eq, PartialEq, Copy, Clone, Default, Hash)]
    pub struct Range {
        required pub start: Position = "start",

        /// Exclusive end position.
        required pub end: Position = "end",
    }
}

impl Range {
    pub fn new(start: Position, end: Position) -> Range {
        Range { start, end }
    }
}

lsp_struct! {
    out
    #[derive(Debug, Eq, PartialEq, Clone, Hash)]
    pub struct Location {
        required pub uri: Uri = "uri",
        required pub range: Range = "range",
    }
}

lsp_struct! {
    both
    /// A diagnostic scoped to one resource.
    #[derive(Debug, Eq, PartialEq, Clone, Default)]
    pub struct Diagnostic {
        required pub range: Range = "range",
        optional pub severity: DiagnosticSeverity = "severity",

        /// Rock uses lint names so clients can ask to suppress a finding.
        optional pub code: NumberOrString = "code",
        optional pub source: String = "source",
        required pub message: String = "message",
    }
}

/// Supported diagnostic severity values.
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Clone, Copy)]
pub struct DiagnosticSeverity(i32);
lsp_enum! {
impl DiagnosticSeverity {
    /// Reports an error.
    pub const ERROR: DiagnosticSeverity = DiagnosticSeverity(1);
    /// Reports a warning.
    pub const WARNING: DiagnosticSeverity = DiagnosticSeverity(2);
    /// Reports a hint.
    pub const HINT: DiagnosticSeverity = DiagnosticSeverity(4);
}
}

lsp_struct! {
    out
    #[derive(Debug, PartialEq, Clone, Default)]
    pub struct Command {
        required pub title: String = "title",
        required pub command: String = "command",
        optional pub arguments: Vec<Value> = "arguments",
    }
}

lsp_struct! {
    out
    /// All edits in a set apply to the initial document version. Apply them from
    /// bottom to top; overlapping edits are not supported.
    #[derive(Debug, Eq, PartialEq, Clone, Default)]
    pub struct TextEdit {
        required pub range: Range = "range",
        required pub new_text: String = "newText",
    }
}

lsp_struct! {
    out
    #[derive(Debug, Eq, PartialEq, Clone, Default)]
    pub struct WorkspaceEdit {
        optional pub changes: HashMap<Uri, Vec<TextEdit>> = "changes",
    }
}

impl WorkspaceEdit {
    #[allow(clippy::mutable_key_type)]
    pub fn new(changes: HashMap<Uri, Vec<TextEdit>>) -> WorkspaceEdit {
        WorkspaceEdit {
            changes: Some(changes),
        }
    }
}

lsp_struct! {
    in
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct TextDocumentIdentifier {
        required pub uri: Uri = "uri",
    }
}

lsp_struct! {
    in
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct TextDocumentParams {
        required pub text_document: TextDocumentIdentifier = "textDocument",
    }
}

lsp_struct! {
    in
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct TextDocumentRangeParams {
        required pub text_document: TextDocumentIdentifier = "textDocument",
        required pub range: Range = "range",
    }
}

lsp_struct! {
    in
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct TextDocumentPositionParams {
        required pub text_document: TextDocumentIdentifier = "textDocument",
        required pub position: Position = "position",
    }
}

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub struct SymbolKind(i32);
lsp_enum! {
impl SymbolKind {
    pub const CLASS: SymbolKind = SymbolKind(5);
    pub const ENUM: SymbolKind = SymbolKind(10);
    pub const FUNCTION: SymbolKind = SymbolKind(12);
    pub const VARIABLE: SymbolKind = SymbolKind(13);
    pub const CONSTANT: SymbolKind = SymbolKind(14);
    pub const KEY: SymbolKind = SymbolKind(20);
    pub const ENUM_MEMBER: SymbolKind = SymbolKind(22);
    pub const STRUCT: SymbolKind = SymbolKind(23);
}
}

lsp_struct! {
    out
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct PublishDiagnosticsParams {
        required pub uri: Uri = "uri",
        required pub diagnostics: Vec<Diagnostic> = "diagnostics",
    }
}

lsp_struct! {
    in
    #[derive(Debug, PartialEq, Clone, Default)]
    pub struct ExecuteCommandParams {
        required pub command: String = "command",
        required pub arguments: Vec<Value> = "arguments",
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum MarkupKind {
    Markdown,
}

impl serde::Serialize for MarkupKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            MarkupKind::Markdown => "markdown",
        })
    }
}

lsp_struct! {
    out
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct MarkupContent {
        required pub kind: MarkupKind = "kind",
        required pub value: String = "value",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn assert_json<T: serde::Serialize>(value: &T, expected: &str) {
        assert_eq!(serde_json::to_string(value).unwrap(), expected);
    }

    #[test]
    fn number_or_string() {
        assert_json(&NumberOrString::Number(123), "123");
        assert_json(&NumberOrString::String("abcd".into()), r#""abcd""#);
        let read = |json| serde_json::from_str::<NumberOrString>(json).unwrap();
        assert_eq!(read("123"), NumberOrString::Number(123));
        assert_eq!(read(r#""abcd""#), NumberOrString::String("abcd".into()));
    }

    #[test]
    fn workspace_edit() {
        assert_json(
            &WorkspaceEdit {
                changes: Some(HashMap::new()),
            },
            r#"{"changes":{}}"#,
        );
        assert_json(&WorkspaceEdit { changes: None }, r#"{}"#);
        assert_json(
            &WorkspaceEdit::new(HashMap::from([("file://test".parse().unwrap(), vec![])])),
            r#"{"changes":{"file://test":[]}}"#,
        );
    }
}
