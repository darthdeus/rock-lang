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

/// Position in a text document expressed as zero-based line and character offset.
/// A position is between two characters like an 'insert' cursor in a editor.
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Copy, Clone, Default, Hash)]
pub struct Position {
    /// Line position in a document (zero-based).
    pub line: u32,
    /// Character offset on a line in a document (zero-based). The meaning of this
    /// offset is determined by the negotiated `PositionEncodingKind`.
    ///
    /// If the character value is greater than the line length it defaults back
    /// to the line length.
    pub character: u32,
}

impl_lsp_struct!(
    both Position,
    required {
        line: u32 = "line",
        character: u32 = "character",
    },
    optional {}
);

impl Position {
    pub fn new(line: u32, character: u32) -> Position {
        Position { line, character }
    }
}

/// A range in a text document expressed as (zero-based) start and end positions.
/// A range is comparable to a selection in an editor. Therefore the end position is exclusive.
#[derive(Debug, Eq, PartialEq, Copy, Clone, Default, Hash)]
pub struct Range {
    /// The range's start position.
    pub start: Position,
    /// The range's end position.
    pub end: Position,
}

impl_lsp_struct!(
    both Range,
    required {
        start: Position = "start",
        end: Position = "end",
    },
    optional {}
);

impl Range {
    pub fn new(start: Position, end: Position) -> Range {
        Range { start, end }
    }
}

/// Represents a location inside a resource, such as a line inside a text file.
#[derive(Debug, Eq, PartialEq, Clone, Hash)]
pub struct Location {
    pub uri: Uri,
    pub range: Range,
}

impl_lsp_struct!(
    out Location,
    required {
        uri: Uri = "uri",
        range: Range = "range",
    },
    optional {}
);

/// Represents a diagnostic, such as a compiler error or warning.
/// Diagnostic objects are only valid in the scope of a resource.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct Diagnostic {
    /// The range at which the message applies.
    pub range: Range,

    /// The diagnostic's severity. Can be omitted. If omitted it is up to the
    /// client to interpret diagnostics as error, warning, info or hint.
    pub severity: Option<DiagnosticSeverity>,

    /// The diagnostic's code. Can be omitted.
    pub code: Option<NumberOrString>,

    /// A human-readable string describing the source of this
    /// diagnostic, e.g. 'typescript' or 'super lint'.
    pub source: Option<String>,

    /// The diagnostic's message.
    pub message: String,
}

impl_lsp_struct!(
    both Diagnostic,
    required {
        range: Range = "range",
        message: String = "message"
    },
    optional {
        severity: DiagnosticSeverity = "severity",
        code: NumberOrString = "code",
        source: String = "source",
    }
);

/// The protocol currently supports the following diagnostic severities:
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

/// Represents a reference to a command. Provides a title which will be used to represent a command in the UI.
/// Commands are identified by a string identifier. The recommended way to handle commands is to implement
/// their execution on the server side if the client and server provides the corresponding capabilities.
/// Alternatively the tool extension code could handle the command.
/// The protocol currently doesn’t specify a set of well-known commands.
#[derive(Debug, PartialEq, Clone, Default)]
pub struct Command {
    /// Title of the command, like `save`.
    pub title: String,
    /// The identifier of the actual command handler.
    pub command: String,
    /// Arguments that the command handler should be
    /// invoked with.
    pub arguments: Option<Vec<Value>>,
}

impl_lsp_struct!(out Command,
    required {
        title: String = "title",
        command: String = "command",
    },
    optional {
        arguments: Vec<Value> = "arguments",
    }
);

/// A textual edit applicable to a text document.
///
/// If n `TextEdit`s are applied to a text document all text edits describe changes to the initial document version.
/// Execution wise text edits should applied from the bottom to the top of the text document. Overlapping text edits
/// are not supported.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct TextEdit {
    /// The range of the text document to be manipulated. To insert
    /// text into a document create a range where start === end.
    pub range: Range,
    /// The string to be inserted. For delete operations use an
    /// empty string.
    pub new_text: String,
}

impl_lsp_struct!(
    out TextEdit,
    required {
        range: Range = "range",
        new_text: String = "newText",
    },
    optional {}
);

/// A workspace edit represents changes to many resources managed in the workspace.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceEdit {
    /// Holds changes to existing resources.
    pub changes: Option<HashMap<Uri, Vec<TextEdit>>>, //    changes?: { [uri: string]: TextEdit[]; };
}

impl_lsp_struct!(out WorkspaceEdit,
    required {},
    optional {
        changes: HashMap<Uri, Vec<TextEdit>> = "changes",
    }
);

impl WorkspaceEdit {
    #[allow(clippy::mutable_key_type)]
    pub fn new(changes: HashMap<Uri, Vec<TextEdit>>) -> WorkspaceEdit {
        WorkspaceEdit {
            changes: Some(changes),
        }
    }
}

/// Text documents are identified using a URI. On the protocol level, URIs are passed as strings.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TextDocumentIdentifier {
    /// The text document's URI.
    pub uri: Uri,
}

impl_lsp_struct!(
    in TextDocumentIdentifier,
    required { uri: Uri = "uri" },
    optional {}
);

/// Params of a request about a whole text document.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TextDocumentParams {
    pub text_document: TextDocumentIdentifier,
}

impl_lsp_struct!(
    in TextDocumentParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument"
    },
    optional {}
);

/// Params of a request about a range of a text document.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TextDocumentRangeParams {
    pub text_document: TextDocumentIdentifier,
    pub range: Range,
}

impl_lsp_struct!(
    in TextDocumentRangeParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        range: Range = "range",
    },
    optional {}
);

/// A parameter literal used in requests to pass a text document and a position inside that document.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TextDocumentPositionParams {
    /// The text document.
    pub text_document: TextDocumentIdentifier,

    /// The position inside the text document.
    pub position: Position,
}

impl_lsp_struct!(
    in TextDocumentPositionParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        position: Position = "position",
    },
    optional {}
);

/// A symbol kind.
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

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct PublishDiagnosticsParams {
    /// The URI for which diagnostic information is reported.
    pub uri: Uri,

    /// An array of diagnostic information items.
    pub diagnostics: Vec<Diagnostic>,
}

impl_lsp_struct!(out PublishDiagnosticsParams,
    required {
        uri: Uri = "uri",
        diagnostics: Vec<Diagnostic> = "diagnostics",
    },
    optional {}
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum Documentation {
    String(String),
    MarkupContent(MarkupContent),
}

impl_untagged_enum!(Documentation {
    MarkupContent,
    String,
});

#[derive(Debug, PartialEq, Clone, Default)]
pub struct ExecuteCommandParams {
    /// The identifier of the actual command handler.
    pub command: String,
    /// Arguments that the command should be invoked with.
    pub arguments: Vec<Value>,
}

impl_lsp_struct!(in ExecuteCommandParams,
    required {
        command: String = "command",
        arguments: Vec<Value> = "arguments",
    },
    optional {}
);

/// Describes the content type that a client supports in various
/// result literals like `Hover`, `ParameterInfo` or `CompletionItem`.
///
/// Please note that `MarkupKinds` must not start with a `$`. This kinds
/// are reserved for internal usage.
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum MarkupKind {
    /// Plain text is supported as a content format
    PlainText,
    /// Markdown is supported as a content format
    Markdown,
}

impl serde::Serialize for MarkupKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            MarkupKind::PlainText => "plaintext",
            MarkupKind::Markdown => "markdown",
        })
    }
}

/// A `MarkupContent` literal represents a string value which content can be represented in different formats.
/// Currently `plaintext` and `markdown` are supported formats. A `MarkupContent` is usually used in
/// documentation properties of result literals like `CompletionItem` or `SignatureInformation`.
/// If the format is `markdown` the content should follow the [GitHub Flavored Markdown Specification](https://github.github.com/gfm/).
///
/// Here is an example how such a string can be constructed using JavaScript / TypeScript:
///
/// ```ignore
/// let markdown: MarkupContent = {
///     kind: MarkupKind::Markdown,
///     value: [
///         "# Header",
///         "Some text",
///         "```typescript",
///         "someCode();",
///         "```"
///     ]
///     .join("\n"),
/// };
/// ```
///
/// Please *Note* that clients might sanitize the return markdown. A client could decide to
/// remove HTML from the markdown to avoid script execution.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct MarkupContent {
    pub kind: MarkupKind,
    pub value: String,
}

impl_lsp_struct!(
    out MarkupContent,
    required {
        kind: MarkupKind = "kind",
        value: String = "value",
    },
    optional {}
);

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
