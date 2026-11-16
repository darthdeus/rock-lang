/*!

Language Server Protocol types for Rust.

Based on: <https://microsoft.github.io/language-server-protocol/specification>

*/
#![allow(non_upper_case_globals)]
#![forbid(unsafe_code)]
#[macro_use]
mod macros;

use std::{collections::HashMap, fmt::Debug};

use serde_json::Value;

/// Decode an optional serde field while preserving errors for invalid values.
/// A missing or explicit `null` field becomes `None`.
pub(crate) fn deserialize_optional<T, E>(value: Option<Value>) -> Result<Option<T>, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    match value {
        Some(value) => serde_json::from_value::<Option<T>>(value).map_err(E::custom),
        None => Ok(None),
    }
}

pub use uri::Uri;
mod uri;

// Large enough to contain any enumeration name defined in this crate
type PascalCaseBuf = [u8; 32];
const fn fmt_pascal_case_const(name: &str) -> (PascalCaseBuf, usize) {
    let mut buf = [0; 32];
    let mut buf_i = 0;
    let mut name_i = 0;
    let name = name.as_bytes();
    while name_i < name.len() {
        let first = name[name_i];
        name_i += 1;

        buf[buf_i] = first;
        buf_i += 1;

        while name_i < name.len() {
            let rest = name[name_i];
            name_i += 1;
            if rest == b'_' {
                break;
            }

            buf[buf_i] = rest.to_ascii_lowercase();
            buf_i += 1;
        }
    }
    (buf, buf_i)
}

fn fmt_pascal_case(f: &mut std::fmt::Formatter<'_>, name: &str) -> std::fmt::Result {
    for word in name.split('_') {
        let mut chars = word.chars();
        let first = chars.next().unwrap();
        write!(f, "{}", first)?;
        for rest in chars {
            write!(f, "{}", rest.to_lowercase())?;
        }
    }
    Ok(())
}

macro_rules! lsp_enum {
    (impl $typ: ident { $( $(#[$attr:meta])* pub const $name: ident : $enum_type: ty = $value: expr; )* }) => {
        impl $typ {
            $(
            $(#[$attr])*
            pub const $name: $enum_type = $value;
            )*
        }

        impl std::fmt::Debug for $typ {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match *self {
                    $(
                    Self::$name => crate::fmt_pascal_case(f, stringify!($name)),
                    )*
                    _ => write!(f, "{}({})", stringify!($typ), self.0),
                }
            }
        }

        impl std::convert::TryFrom<&str> for $typ {
            type Error = &'static str;
            fn try_from(value: &str) -> Result<Self, Self::Error> {
                match () {
                    $(
                        _ if {
                            const X: (crate::PascalCaseBuf, usize) = crate::fmt_pascal_case_const(stringify!($name));
                            let (buf, len) = X;
                            &buf[..len] == value.as_bytes()
                        } => Ok(Self::$name),
                    )*
                    _ => Err("unknown enum variant"),
                }
            }
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

pub mod notification;
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

impl_untagged_enum!(NumberOrString {
    Number(i32),
    String(String),
});

/* ----------------- Cancel support ----------------- */

/* ----------------- Basic JSON Structures ----------------- */

/// The LSP any type
///
/// @since 3.17.0
pub(crate) type LSPAny = serde_json::Value;

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
    Position,
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
    Range,
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
    Location,
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

    /// A data entry field that is preserved between a `textDocument/publishDiagnostics`
    /// notification and `textDocument/codeAction` request.
    ///
    /// @since 3.16.0
    pub data: Option<serde_json::Value>,
}

impl_lsp_struct!(
    Diagnostic,
    required {
        range: Range = "range",
        message: String = "message"
    },
    optional {
        severity: DiagnosticSeverity = "severity",
        code: NumberOrString = "code",
        source: String = "source",
        data: serde_json::Value = "data",
    }
);

/// The protocol currently supports the following diagnostic severities:
#[derive(Eq, PartialEq, Ord, PartialOrd, Clone, Copy)]
pub struct DiagnosticSeverity(i32);
lsp_enum! {
impl DiagnosticSeverity {
    /// Reports an error.
    pub const ERROR: DiagnosticSeverity = DiagnosticSeverity(1);
    /// Reports a warning.
    pub const WARNING: DiagnosticSeverity = DiagnosticSeverity(2);
    /// Reports an information.
    pub const INFORMATION: DiagnosticSeverity = DiagnosticSeverity(3);
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

impl_lsp_struct!(Command,
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
    TextEdit,
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

impl_lsp_struct!(WorkspaceEdit,
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
    TextDocumentIdentifier,
    required { uri: Uri = "uri" },
    optional {}
);

/// Params of a request about a whole text document.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TextDocumentParams {
    pub text_document: TextDocumentIdentifier,
}

impl_lsp_struct!(
    TextDocumentParams,
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
    TextDocumentRangeParams,
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
    TextDocumentPositionParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        position: Position = "position",
    },
    optional {}
);

/// A symbol kind.
#[derive(Eq, PartialEq, Copy, Clone)]
pub struct SymbolKind(i32);
lsp_enum! {
impl SymbolKind {
    pub const FILE: SymbolKind = SymbolKind(1);
    pub const MODULE: SymbolKind = SymbolKind(2);
    pub const NAMESPACE: SymbolKind = SymbolKind(3);
    pub const PACKAGE: SymbolKind = SymbolKind(4);
    pub const CLASS: SymbolKind = SymbolKind(5);
    pub const METHOD: SymbolKind = SymbolKind(6);
    pub const PROPERTY: SymbolKind = SymbolKind(7);
    pub const FIELD: SymbolKind = SymbolKind(8);
    pub const CONSTRUCTOR: SymbolKind = SymbolKind(9);
    pub const ENUM: SymbolKind = SymbolKind(10);
    pub const INTERFACE: SymbolKind = SymbolKind(11);
    pub const FUNCTION: SymbolKind = SymbolKind(12);
    pub const VARIABLE: SymbolKind = SymbolKind(13);
    pub const CONSTANT: SymbolKind = SymbolKind(14);
    pub const STRING: SymbolKind = SymbolKind(15);
    pub const NUMBER: SymbolKind = SymbolKind(16);
    pub const BOOLEAN: SymbolKind = SymbolKind(17);
    pub const ARRAY: SymbolKind = SymbolKind(18);
    pub const OBJECT: SymbolKind = SymbolKind(19);
    pub const KEY: SymbolKind = SymbolKind(20);
    pub const NULL: SymbolKind = SymbolKind(21);
    pub const ENUM_MEMBER: SymbolKind = SymbolKind(22);
    pub const STRUCT: SymbolKind = SymbolKind(23);
    pub const EVENT: SymbolKind = SymbolKind(24);
    pub const OPERATOR: SymbolKind = SymbolKind(25);
    pub const TYPE_PARAMETER: SymbolKind = SymbolKind(26);
}
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct PublishDiagnosticsParams {
    /// The URI for which diagnostic information is reported.
    pub uri: Uri,

    /// An array of diagnostic information items.
    pub diagnostics: Vec<Diagnostic>,

    /// Optional the version number of the document the diagnostics are published for.
    pub version: Option<i32>,
}

impl_lsp_struct!(PublishDiagnosticsParams,
    required {
        uri: Uri = "uri",
        diagnostics: Vec<Diagnostic> = "diagnostics",
    },
    optional { version: i32 = "version" }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum Documentation {
    String(String),
    MarkupContent(MarkupContent),
}

impl_untagged_enum!(Documentation {
    MarkupContent(MarkupContent),
    String(String),
});

/// MarkedString can be used to render human readable text. It is either a
/// markdown string or a code-block that provides a language and a code snippet.
/// The language identifier is semantically equal to the optional language
/// identifier in fenced code blocks in GitHub issues.
///
/// The pair of a language and a value is an equivalent to markdown:
///
/// ```LANGUAGE
/// VALUE
/// ```
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum MarkedString {
    String(String),
    LanguageString(LanguageString),
}

impl_untagged_enum!(MarkedString {
    LanguageString(LanguageString),
    String(String),
});

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct LanguageString {
    pub language: String,
    pub value: String,
}

impl_lsp_struct!(
    LanguageString,
    required {
        language: String = "language",
        value: String = "value",
    },
    optional {}
);

#[derive(Debug, PartialEq, Clone, Default)]
pub struct ExecuteCommandParams {
    /// The identifier of the actual command handler.
    pub command: String,
    /// Arguments that the command should be invoked with.
    pub arguments: Vec<Value>,
}

impl_lsp_struct!(ExecuteCommandParams,
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

impl_str_enum!(MarkupKind { PlainText = "plaintext", Markdown = "markdown" });

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
    MarkupContent,
    required {
        kind: MarkupKind = "kind",
        value: String = "value",
    },
    optional {}
);

/// Symbol tags are extra annotations that tweak the rendering of a symbol.
///
/// @since 3.16.0
#[derive(Eq, PartialEq, Clone)]
pub struct SymbolTag(i32);
lsp_enum! {
impl SymbolTag {
    /// Render a symbol as obsolete, usually using a strike-out.
    pub const DEPRECATED: SymbolTag = SymbolTag(1);
}
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    pub(crate) fn test_serialization<SER>(ms: &SER, expected: &str)
    where
        SER: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json_str = serde_json::to_string(ms).unwrap();
        assert_eq!(&json_str, expected);
        let deserialized: SER = serde_json::from_str(&json_str).unwrap();
        assert_eq!(&deserialized, ms);
    }

    pub(crate) fn test_deserialization<T>(json: &str, expected: &T)
    where
        T: for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let value = serde_json::from_str::<T>(json).unwrap();
        assert_eq!(&value, expected);
    }

    #[test]
    fn number_or_string() {
        test_serialization(&NumberOrString::Number(123), r#"123"#);

        test_serialization(&NumberOrString::String("abcd".into()), r#""abcd""#);
    }

    #[test]
    fn marked_string() {
        test_serialization(&MarkedString::String("xxx".into()), r#""xxx""#);

        test_serialization(
            &MarkedString::LanguageString(LanguageString {
                language: "lang".into(),
                value: "code".into(),
            }),
            r#"{"language":"lang","value":"code"}"#,
        );
    }

    #[test]
    fn language_string() {
        test_serialization(
            &LanguageString {
                language: "LL".into(),
                value: "VV".into(),
            },
            r#"{"language":"LL","value":"VV"}"#,
        );
    }

    #[test]
    fn workspace_edit() {
        test_serialization(
            &WorkspaceEdit {
                changes: Some(vec![].into_iter().collect()),
            },
            r#"{"changes":{}}"#,
        );

        test_serialization(&WorkspaceEdit { changes: None }, r#"{}"#);

        test_serialization(
            &WorkspaceEdit {
                changes: Some(
                    vec![("file://test".parse().unwrap(), vec![])]
                        .into_iter()
                        .collect(),
                ),
            },
            r#"{"changes":{"file://test":[]}}"#,
        );
    }
}
