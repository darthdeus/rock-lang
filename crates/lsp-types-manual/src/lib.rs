/*!

Language Server Protocol types for Rust.

Based on: <https://microsoft.github.io/language-server-protocol/specification>

*/
#![allow(non_upper_case_globals)]
#![forbid(unsafe_code)]
#[macro_use]
extern crate bitflags;
#[macro_use]
mod macros;

use std::{collections::HashMap, fmt::Debug};

use serde::{de, de::Error, Deserialize};
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

mod color;
pub use color::*;

mod completion;
pub use completion::*;

mod document_highlight;
pub use document_highlight::*;

mod document_symbols;
pub use document_symbols::*;

mod formatting;
pub use formatting::*;

mod hover;
pub use hover::*;

mod inlay_hint;
pub use inlay_hint::*;

mod progress;
pub use progress::*;

mod references;
pub use references::*;

mod rename;
pub use rename::*;

mod semantic_tokens;
pub use semantic_tokens::*;

mod signature_help;
pub use signature_help::*;

mod window;
pub use window::*;

mod workspace_folders;
pub use workspace_folders::*;

mod workspace_symbols;
pub use workspace_symbols::*;

mod trace;
pub use trace::*;

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

impl Location {
    pub fn new(uri: Uri, range: Range) -> Location {
        Location { uri, range }
    }
}

/// Represents a link between a source and a target location.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct LocationLink {
    /// Span of the origin of this link.
    ///
    /// Used as the underlined span for mouse interaction. Defaults to the word range at
    /// the mouse position.
    pub origin_selection_range: Option<Range>,

    /// The target resource identifier of this link.
    pub target_uri: Uri,

    /// The full target range of this link.
    pub target_range: Range,

    /// The span of this link.
    pub target_selection_range: Range,
}

impl_lsp_struct!(
    LocationLink,
    required {
        target_uri: Uri = "targetUri",
        target_range: Range = "targetRange",
        target_selection_range: Range = "targetSelectionRange",
    },
    optional {
        origin_selection_range: Range = "originSelectionRange",
    }
);

/// A type indicating how positions are encoded,
/// specifically what column offsets mean.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Hash, PartialOrd, Clone)]
pub struct PositionEncodingKind(std::borrow::Cow<'static, str>);

impl PositionEncodingKind {
    /// Character offsets count UTF-8 code units.
    pub const UTF8: PositionEncodingKind = PositionEncodingKind::new("utf-8");

    /// Character offsets count UTF-16 code units.
    ///
    /// This is the default and must always be supported
    /// by servers
    pub const UTF16: PositionEncodingKind = PositionEncodingKind::new("utf-16");

    /// Character offsets count UTF-32 code units.
    ///
    /// Implementation note: these are the same as Unicode code points,
    /// so this `PositionEncodingKind` may also be used for an
    /// encoding-agnostic representation of character offsets.
    pub const UTF32: PositionEncodingKind = PositionEncodingKind::new("utf-32");

    pub const fn new(tag: &'static str) -> Self {
        PositionEncodingKind(std::borrow::Cow::Borrowed(tag))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for PositionEncodingKind {
    fn from(from: String) -> Self {
        PositionEncodingKind(std::borrow::Cow::from(from))
    }
}

impl From<&'static str> for PositionEncodingKind {
    fn from(from: &'static str) -> Self {
        PositionEncodingKind::new(from)
    }
}

impl serde::Serialize for PositionEncodingKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PositionEncodingKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(PositionEncodingKind(std::borrow::Cow::Owned(
            String::deserialize(d)?,
        )))
    }
}

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

    /// An optional property to describe the error code.
    ///
    /// @since 3.16.0
    pub code_description: Option<CodeDescription>,

    /// A human-readable string describing the source of this
    /// diagnostic, e.g. 'typescript' or 'super lint'.
    pub source: Option<String>,

    /// The diagnostic's message.
    pub message: String,

    /// An array of related diagnostic information, e.g. when symbol-names within
    /// a scope collide all definitions can be marked via this property.
    pub related_information: Option<Vec<DiagnosticRelatedInformation>>,

    /// Additional metadata about the diagnostic.
    pub tags: Option<Vec<DiagnosticTag>>,

    /// A data entry field that is preserved between a `textDocument/publishDiagnostics`
    /// notification and `textDocument/codeAction` request.
    ///
    /// @since 3.16.0
    pub data: Option<serde_json::Value>,
}

impl_lsp_struct!(Diagnostic,
    required { range: Range = "range", message: String = "message" },
    optional {
        severity: DiagnosticSeverity = "severity",
        code: NumberOrString = "code",
        code_description: CodeDescription = "codeDescription",
        source: String = "source",
        related_information: Vec<DiagnosticRelatedInformation> = "relatedInformation",
        tags: Vec<DiagnosticTag> = "tags",
        data: serde_json::Value = "data",
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct CodeDescription {
    pub href: Uri,
}

impl_lsp_struct!(
    CodeDescription,
    required { href: Uri = "href" },
    optional {}
);

impl Diagnostic {
    pub fn new(
        range: Range,
        severity: Option<DiagnosticSeverity>,
        code: Option<NumberOrString>,
        source: Option<String>,
        message: String,
        related_information: Option<Vec<DiagnosticRelatedInformation>>,
        tags: Option<Vec<DiagnosticTag>>,
    ) -> Diagnostic {
        Diagnostic {
            range,
            severity,
            code,
            source,
            message,
            related_information,
            tags,
            ..Diagnostic::default()
        }
    }
}

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

/// Represents a related message and source code location for a diagnostic. This
/// should be used to point to code locations that cause or related to a
/// diagnostics, e.g when duplicating a symbol in a scope.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DiagnosticRelatedInformation {
    /// The location of this related diagnostic information.
    pub location: Location,

    /// The message of this related diagnostic information.
    pub message: String,
}

impl_lsp_struct!(
    DiagnosticRelatedInformation,
    required {
        location: Location = "location",
        message: String = "message",
    },
    optional {}
);

/// The diagnostic tags.
#[derive(Eq, PartialEq, Clone)]
pub struct DiagnosticTag(i32);
lsp_enum! {
impl DiagnosticTag {
    /// Unused or unnecessary code.
    /// Clients are allowed to render diagnostics with this tag faded out instead of having
    /// an error squiggle.
    pub const UNNECESSARY: DiagnosticTag = DiagnosticTag(1);

    /// Deprecated or obsolete code.
    /// Clients are allowed to rendered diagnostics with this tag strike through.
    pub const DEPRECATED: DiagnosticTag = DiagnosticTag(2);
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

impl Command {
    pub fn new(title: String, command: String, arguments: Option<Vec<Value>>) -> Command {
        Command {
            title,
            command,
            arguments,
        }
    }
}

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

impl TextEdit {
    pub fn new(range: Range, new_text: String) -> TextEdit {
        TextEdit { range, new_text }
    }
}

/// An identifier referring to a change annotation managed by a workspace
/// edit.
///
/// @since 3.16.0
pub(crate) type ChangeAnnotationIdentifier = String;

/// A special text edit with an additional change annotation.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct AnnotatedTextEdit {
    /// The actual annotation
    pub annotation_id: ChangeAnnotationIdentifier,

    pub text_edit: TextEdit,
}

impl_lsp_struct!(
    AnnotatedTextEdit,
    required {
        annotation_id: ChangeAnnotationIdentifier = "annotationId"
    },
    optional {},
    flatten {
        text_edit: TextEdit
    }
);

/// Describes textual changes on a single text document. The text document is referred to as a
/// `OptionalVersionedTextDocumentIdentifier` to allow clients to check the text document version before an
/// edit is applied. A `TextDocumentEdit` describes all changes on a version Si and after they are
/// applied move the document to version Si+1. So the creator of a `TextDocumentEdit` doesn't need to
/// sort the array or do any kind of ordering. However the edits must be non overlapping.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TextDocumentEdit {
    /// The text document to change.
    pub text_document: OptionalVersionedTextDocumentIdentifier,

    /// The edits to be applied.
    ///
    /// @since 3.16.0 - support for AnnotatedTextEdit. This is guarded by the
    /// client capability `workspace.workspaceEdit.changeAnnotationSupport`
    pub edits: Vec<OneOf<TextEdit, AnnotatedTextEdit>>,
}

impl_lsp_struct!(TextDocumentEdit,
    required {
        text_document: OptionalVersionedTextDocumentIdentifier = "textDocument",
        edits: Vec<OneOf<TextEdit, AnnotatedTextEdit>> = "edits",
    },
    optional {}
);

/// Additional information that describes document changes.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct ChangeAnnotation {
    /// A human-readable string describing the actual change. The string
    /// is rendered prominent in the user interface.
    pub label: String,

    /// A flag which indicates that user confirmation is needed
    /// before applying the change.
    pub needs_confirmation: Option<bool>,

    /// A human-readable string which is rendered less prominent in
    /// the user interface.
    pub description: Option<String>,
}

impl_lsp_struct!(
    ChangeAnnotation,
    required {
        label: String = "label"
    },
    optional {
        needs_confirmation: bool = "needsConfirmation",
        description: String = "description",
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct ChangeAnnotationWorkspaceEditClientCapabilities {
    /// Whether the client groups edits with equal labels into tree nodes,
    /// for instance all edits labelled with "Changes in Strings" would
    /// be a tree node.
    pub groups_on_label: Option<bool>,
}

impl_lsp_struct!(
    ChangeAnnotationWorkspaceEditClientCapabilities,
    required {},
    optional {
        groups_on_label: bool = "groupsOnLabel"
    }
);

/// Options to create a file.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct CreateFileOptions {
    /// Overwrite existing file. Overwrite wins over `ignoreIfExists`
    pub overwrite: Option<bool>,
    /// Ignore if exists.
    pub ignore_if_exists: Option<bool>,
}

impl_lsp_struct!(
    CreateFileOptions,
    required {},
    optional {
        overwrite: bool = "overwrite",
        ignore_if_exists: bool = "ignoreIfExists",
    }
);

/// Create file operation
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct CreateFile {
    /// The resource to create.
    pub uri: Uri,
    /// Additional options
    pub options: Option<CreateFileOptions>,

    /// An optional annotation identifier describing the operation.
    ///
    /// @since 3.16.0
    pub annotation_id: Option<ChangeAnnotationIdentifier>,
}

impl_lsp_struct!(
    CreateFile,
    required { uri: Uri = "uri" },
    optional {
        options: CreateFileOptions = "options",
        annotation_id: ChangeAnnotationIdentifier = "annotationId",
    }
);

/// Rename file options
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct RenameFileOptions {
    /// Overwrite target if existing. Overwrite wins over `ignoreIfExists`
    pub overwrite: Option<bool>,
    /// Ignores if target exists.
    pub ignore_if_exists: Option<bool>,
}

impl_lsp_struct!(
    RenameFileOptions,
    required {},
    optional {
        overwrite: bool = "overwrite",
        ignore_if_exists: bool = "ignoreIfExists",
    }
);

/// Rename file operation
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct RenameFile {
    /// The old (existing) location.
    pub old_uri: Uri,
    /// The new location.
    pub new_uri: Uri,
    /// Rename options.
    pub options: Option<RenameFileOptions>,

    /// An optional annotation identifier describing the operation.
    ///
    /// @since 3.16.0
    pub annotation_id: Option<ChangeAnnotationIdentifier>,
}

impl_lsp_struct!(
    RenameFile,
    required {
        old_uri: Uri = "oldUri",
        new_uri: Uri = "newUri",
    },
    optional {
        options: RenameFileOptions = "options",
        annotation_id: ChangeAnnotationIdentifier = "annotationId",
    }
);

/// Delete file options
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DeleteFileOptions {
    /// Delete the content recursively if a folder is denoted.
    pub recursive: Option<bool>,
    /// Ignore the operation if the file doesn't exist.
    pub ignore_if_not_exists: Option<bool>,

    /// An optional annotation identifier describing the operation.
    ///
    /// @since 3.16.0
    pub annotation_id: Option<ChangeAnnotationIdentifier>,
}

impl_lsp_struct!(
    DeleteFileOptions,
    required {},
    optional {
        recursive: bool = "recursive",
        ignore_if_not_exists: bool = "ignoreIfNotExists",
        annotation_id: ChangeAnnotationIdentifier = "annotationId",
    }
);

/// Delete file operation
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DeleteFile {
    /// The file to delete.
    pub uri: Uri,
    /// Delete options.
    pub options: Option<DeleteFileOptions>,
}

impl_lsp_struct!(
    DeleteFile,
    required { uri: Uri = "uri" },
    optional {
        options: DeleteFileOptions = "options"
    }
);

/// A workspace edit represents changes to many resources managed in the workspace.
/// The edit should either provide `changes` or `documentChanges`.
/// If the client can handle versioned document edits and if `documentChanges` are present,
/// the latter are preferred over `changes`.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceEdit {
    /// Holds changes to existing resources.
    pub changes: Option<HashMap<Uri, Vec<TextEdit>>>, //    changes?: { [uri: string]: TextEdit[]; };

    /// Depending on the client capability `workspace.workspaceEdit.resourceOperations` document changes
    /// are either an array of `TextDocumentEdit`s to express changes to n different text documents
    /// where each text document edit addresses a specific version of a text document. Or it can contain
    /// above `TextDocumentEdit`s mixed with create, rename and delete file / folder operations.
    ///
    /// Whether a client supports versioned document edits is expressed via
    /// `workspace.workspaceEdit.documentChanges` client capability.
    ///
    /// If a client neither supports `documentChanges` nor `workspace.workspaceEdit.resourceOperations` then
    /// only plain `TextEdit`s using the `changes` property are supported.
    pub document_changes: Option<DocumentChanges>,

    /// A map of change annotations that can be referenced in
    /// `AnnotatedTextEdit`s or create, rename and delete file / folder
    /// operations.
    ///
    /// Whether clients honor this property depends on the client capability
    /// `workspace.changeAnnotationSupport`.
    ///
    /// @since 3.16.0
    pub change_annotations: Option<HashMap<ChangeAnnotationIdentifier, ChangeAnnotation>>,
}

impl_lsp_struct!(WorkspaceEdit,
    required {},
    optional {
        changes: HashMap<Uri, Vec<TextEdit>> = "changes",
        document_changes: DocumentChanges = "documentChanges",
        change_annotations: HashMap<ChangeAnnotationIdentifier, ChangeAnnotation> = "changeAnnotations",
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum DocumentChanges {
    Edits(Vec<TextDocumentEdit>),
    Operations(Vec<DocumentChangeOperation>),
}

impl_untagged_enum!(DocumentChanges {
    Edits(Vec<TextDocumentEdit>),
    Operations(Vec<DocumentChangeOperation>),
});

// TODO: Once https://github.com/serde-rs/serde/issues/912 is solved
// we can remove ResourceOp and switch to the following implementation
// of DocumentChangeOperation:
//
// #[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
// #[serde(tag = "kind", rename_all="lowercase" )]
// pub enum DocumentChangeOperation {
//     Create(CreateFile),
//     Rename(RenameFile),
//     Delete(DeleteFile),
//
//     #[serde(other)]
//     Edit(TextDocumentEdit),
// }

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum DocumentChangeOperation {
    Op(ResourceOp),
    Edit(TextDocumentEdit),
}

impl_untagged_enum!(DocumentChangeOperation {
    Op(ResourceOp),
    Edit(TextDocumentEdit),
});

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum ResourceOp {
    Create(CreateFile),
    Rename(RenameFile),
    Delete(DeleteFile),
}

impl_tagged_enum!(ResourceOp, tag = "kind" {
    Create(CreateFile) = "create",
    Rename(RenameFile) = "rename",
    Delete(DeleteFile) = "delete",
});

pub(crate) type DidChangeConfigurationClientCapabilities = DynamicRegistrationClientCapabilities;

impl WorkspaceEdit {
    #[allow(clippy::mutable_key_type)]
    pub fn new(changes: HashMap<Uri, Vec<TextEdit>>) -> WorkspaceEdit {
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            ..Default::default()
        }
    }
}

/// Text documents are identified using a URI. On the protocol level, URIs are passed as strings.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TextDocumentIdentifier {
    // !!!!!! Note:
    // In the spec VersionedTextDocumentIdentifier extends TextDocumentIdentifier
    // This modelled by "mixing-in" TextDocumentIdentifier in VersionedTextDocumentIdentifier,
    // so any changes to this type must be effected in the sub-type as well.
    /// The text document's URI.
    pub uri: Uri,
}

impl_lsp_struct!(
    TextDocumentIdentifier,
    required { uri: Uri = "uri" },
    optional {}
);

impl TextDocumentIdentifier {
    pub fn new(uri: Uri) -> TextDocumentIdentifier {
        TextDocumentIdentifier { uri }
    }
}

/// An identifier which optionally denotes a specific version of a text document. This information usually flows from the server to the client
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct OptionalVersionedTextDocumentIdentifier {
    // This field was "mixed-in" from TextDocumentIdentifier
    /// The text document's URI.
    pub uri: Uri,

    /// The version number of this document. If an optional versioned text document
    /// identifier is sent from the server to the client and the file is not
    /// open in the editor (the server has not received an open notification
    /// before) the server can send `null` to indicate that the version is
    /// known and the content on disk is the master (as specified with document
    /// content ownership).
    ///
    /// The version number of a document will increase after each change,
    /// including undo/redo. The number doesn't need to be consecutive.
    pub version: Option<i32>,
}

impl_lsp_struct!(
    OptionalVersionedTextDocumentIdentifier,
    required { uri: Uri = "uri" },
    nullable {
        version: i32 = "version"
    },
    optional {}
);

impl OptionalVersionedTextDocumentIdentifier {
    pub fn new(uri: Uri, version: i32) -> OptionalVersionedTextDocumentIdentifier {
        OptionalVersionedTextDocumentIdentifier {
            uri,
            version: Some(version),
        }
    }
}

/// A parameter literal used in requests to pass a text document and a position inside that document.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TextDocumentPositionParams {
    // !!!!!! Note:
    // In the spec ReferenceParams extends TextDocumentPositionParams
    // This modelled by "mixing-in" TextDocumentPositionParams in ReferenceParams,
    // so any changes to this type must be effected in sub-type as well.
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

impl TextDocumentPositionParams {
    pub fn new(
        text_document: TextDocumentIdentifier,
        position: Position,
    ) -> TextDocumentPositionParams {
        TextDocumentPositionParams {
            text_document,
            position,
        }
    }
}

/// A document filter denotes a document through properties like language, schema or pattern.
/// Examples are a filter that applies to TypeScript files on disk or a filter the applies to JSON
/// files with name package.json:
///
/// { language: 'typescript', scheme: 'file' }
/// { language: 'json', pattern: '**/package.json' }
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentFilter {
    /// A language id, like `typescript`.
    pub language: Option<String>,

    /// A Uri [scheme](#Uri.scheme), like `file` or `untitled`.
    pub scheme: Option<String>,

    /// A glob pattern, like `*.{ts,js}`.
    pub pattern: Option<String>,
}

impl_lsp_struct!(
    DocumentFilter,
    required {},
    optional {
        language: String = "language",
        scheme: String = "scheme",
        pattern: String = "pattern",
    }
);

/// A document selector is the combination of one or many document filters.
pub(crate) type DocumentSelector = Vec<DocumentFilter>;

// ========================= Actual Protocol =========================

#[derive(Debug, PartialEq, Clone, Default)]
pub struct InitializeParams {
    /// The process Id of the parent process that started
    /// the server. Is null if the process has not been started by another process.
    /// If the parent process is not alive then the server should exit (see exit notification) its process.
    pub process_id: Option<u32>,

    /// The rootPath of the workspace. Is null if no folder is open.
    #[deprecated(note = "Use `root_uri` instead when possible")]
    pub root_path: Option<String>,

    /// The rootUri of the workspace. Is null if no folder is open.
    /// If both `rootPath` and `rootUri` are set `rootUri` wins.
    #[deprecated(note = "Use `workspace_folders` instead when possible")]
    pub root_uri: Option<Uri>,

    /// User provided initialization options.
    pub initialization_options: Option<Value>,

    /// The capabilities provided by the client (editor or tool)
    pub capabilities: ClientCapabilities,

    /// The initial trace setting. If omitted trace is disabled ('off').
    pub trace: Option<TraceValue>,

    /// The workspace folders configured in the client when the server starts.
    /// This property is only available if the client supports workspace folders.
    /// It can be `null` if the client supports workspace folders but none are
    /// configured.
    pub workspace_folders: Option<Vec<WorkspaceFolder>>,

    /// Information about the client.
    pub client_info: Option<ClientInfo>,

    /// The locale the client is currently showing the user interface in.
    ///
    /// @since 3.16.0
    pub locale: Option<String>,

    /// The LSP server may report about initialization progress to the client
    /// by using the following work done token if it was passed by the client.
    pub work_done_progress_params: WorkDoneProgressParams,
}

impl_lsp_struct!(InitializeParams,
    required { capabilities: ClientCapabilities = "capabilities" },
    nullable {
        process_id: u32 = "processId",
        root_uri: Uri = "rootUri",
    },
    optional {
        root_path: String = "rootPath",
        initialization_options: Value = "initializationOptions",
        trace: TraceValue = "trace",
        workspace_folders: Vec<WorkspaceFolder> = "workspaceFolders",
        client_info: ClientInfo = "clientInfo",
        locale: String = "locale",
    },
    flatten { work_done_progress_params: WorkDoneProgressParams }
);

#[derive(Debug, PartialEq, Clone)]
pub struct ClientInfo {
    /// The name of the client as defined by the client.
    pub name: String,
    /// The client's version as defined by the client.
    pub version: Option<String>,
}

impl_lsp_struct!(
    ClientInfo,
    required {
        name: String = "name"
    },
    optional {
        version: String = "version"
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Copy, Default)]
pub struct DynamicRegistrationClientCapabilities {
    /// This capability supports dynamic registration.
    pub dynamic_registration: Option<bool>,
}

impl_lsp_struct!(
    DynamicRegistrationClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration"
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Copy, Default)]
pub struct GotoCapability {
    pub dynamic_registration: Option<bool>,

    /// The client supports additional metadata in the form of definition links.
    pub link_support: Option<bool>,
}

impl_lsp_struct!(
    GotoCapability,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        link_support: bool = "linkSupport",
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceEditClientCapabilities {
    /// The client supports versioned document changes in `WorkspaceEdit`s
    pub document_changes: Option<bool>,

    /// The resource operations the client supports. Clients should at least
    /// support 'create', 'rename' and 'delete' files and folders.
    pub resource_operations: Option<Vec<ResourceOperationKind>>,

    /// The failure handling strategy of a client if applying the workspace edit fails.
    pub failure_handling: Option<FailureHandlingKind>,

    /// Whether the client normalizes line endings to the client specific
    /// setting.
    /// If set to `true` the client will normalize line ending characters
    /// in a workspace edit to the client specific new line character(s).
    ///
    /// @since 3.16.0
    pub normalizes_line_endings: Option<bool>,

    /// Whether the client in general supports change annotations on text edits,
    /// create file, rename file and delete file changes.
    ///
    /// @since 3.16.0
    pub change_annotation_support: Option<ChangeAnnotationWorkspaceEditClientCapabilities>,
}

impl_lsp_struct!(WorkspaceEditClientCapabilities,
    required {},
    optional {
        document_changes: bool = "documentChanges",
        resource_operations: Vec<ResourceOperationKind> = "resourceOperations",
        failure_handling: FailureHandlingKind = "failureHandling",
        normalizes_line_endings: bool = "normalizesLineEndings",
        change_annotation_support: ChangeAnnotationWorkspaceEditClientCapabilities = "changeAnnotationSupport",
    }
);

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum ResourceOperationKind {
    Create,
    Rename,
    Delete,
}

impl_str_enum!(ResourceOperationKind { Create = "create", Rename = "rename", Delete = "delete" });

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum FailureHandlingKind {
    Abort,
    Transactional,
    TextOnlyTransactional,
    Undo,
}

impl_str_enum!(FailureHandlingKind {
    Abort = "abort",
    Transactional = "transactional",
    TextOnlyTransactional = "textOnlyTransactional",
    Undo = "undo",
});

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

/// Specific capabilities for the `SymbolKind` in the `workspace/symbol` request.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SymbolKindCapability {
    /// The symbol kind values the client supports. When this
    /// property exists the client also guarantees that it will
    /// handle values outside its set gracefully and falls back
    /// to a default value when unknown.
    ///
    /// If this property is not present the client only supports
    /// the symbol kinds from `File` to `Array` as defined in
    /// the initial version of the protocol.
    pub value_set: Option<Vec<SymbolKind>>,
}

impl_lsp_struct!(SymbolKindCapability,
    required {},
    optional { value_set: Vec<SymbolKind> = "valueSet" }
);

/// Workspace specific client capabilities.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceClientCapabilities {
    /// The client supports applying batch edits to the workspace by supporting
    /// the request 'workspace/applyEdit'
    pub apply_edit: Option<bool>,

    /// Capabilities specific to `WorkspaceEdit`s
    pub workspace_edit: Option<WorkspaceEditClientCapabilities>,

    /// Capabilities specific to the `workspace/didChangeConfiguration` notification.
    pub did_change_configuration: Option<DidChangeConfigurationClientCapabilities>,

    /// Capabilities specific to the `workspace/didChangeWatchedFiles` notification.
    pub did_change_watched_files: Option<DidChangeWatchedFilesClientCapabilities>,

    /// Capabilities specific to the `workspace/symbol` request.
    pub symbol: Option<WorkspaceSymbolClientCapabilities>,

    /// Capabilities specific to the `workspace/executeCommand` request.
    pub execute_command: Option<ExecuteCommandClientCapabilities>,

    /// The client has support for workspace folders.
    ///
    /// @since 3.6.0
    pub workspace_folders: Option<bool>,

    /// The client supports `workspace/configuration` requests.
    ///
    /// @since 3.6.0
    pub configuration: Option<bool>,

    /// Capabilities specific to the semantic token requests scoped to the workspace.
    ///
    /// @since 3.16.0
    pub semantic_tokens: Option<SemanticTokensWorkspaceClientCapabilities>,

    /// Client workspace capabilities specific to inlay hints.
    ///
    /// @since 3.17.0
    pub inlay_hint: Option<InlayHintWorkspaceClientCapabilities>,
}

impl_lsp_struct!(
    WorkspaceClientCapabilities,
    required {},
    optional {
        apply_edit: bool = "applyEdit",
        workspace_edit: WorkspaceEditClientCapabilities = "workspaceEdit",
        did_change_configuration: DidChangeConfigurationClientCapabilities =
            "didChangeConfiguration",
        did_change_watched_files: DidChangeWatchedFilesClientCapabilities = "didChangeWatchedFiles",
        symbol: WorkspaceSymbolClientCapabilities = "symbol",
        execute_command: ExecuteCommandClientCapabilities = "executeCommand",
        workspace_folders: bool = "workspaceFolders",
        configuration: bool = "configuration",
        semantic_tokens: SemanticTokensWorkspaceClientCapabilities = "semanticTokens",
        inlay_hint: InlayHintWorkspaceClientCapabilities = "inlayHint",
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct TextDocumentSyncClientCapabilities {
    /// Whether text document synchronization supports dynamic registration.
    pub dynamic_registration: Option<bool>,

    /// The client supports sending will save notifications.
    pub will_save: Option<bool>,

    /// The client supports sending a will save request and
    /// waits for a response providing text edits which will
    /// be applied to the document before it is saved.
    pub will_save_wait_until: Option<bool>,

    /// The client supports did save notifications.
    pub did_save: Option<bool>,
}

impl_lsp_struct!(
    TextDocumentSyncClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        will_save: bool = "willSave",
        will_save_wait_until: bool = "willSaveWaitUntil",
        did_save: bool = "didSave",
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct PublishDiagnosticsClientCapabilities {
    /// Whether the clients accepts diagnostics with related information.
    pub related_information: Option<bool>,

    /// Client supports the tag property to provide meta data about a diagnostic.
    /// Clients supporting tags have to handle unknown tags gracefully.
    pub tag_support: Option<TagSupport<DiagnosticTag>>,

    /// Whether the client interprets the version property of the
    /// `textDocument/publishDiagnostics` notification's parameter.
    ///
    /// @since 3.15.0
    pub version_support: Option<bool>,

    /// Client supports a codeDescription property
    ///
    /// @since 3.16.0
    pub code_description_support: Option<bool>,

    /// Whether code action supports the `data` property which is
    /// preserved between a `textDocument/publishDiagnostics` and
    /// `textDocument/codeAction` request.
    ///
    /// @since 3.16.0
    pub data_support: Option<bool>,
}

impl serde::Serialize for PublishDiagnosticsClientCapabilities {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let hint = (self.related_information.is_some() as usize)
            + self.tag_support.is_some() as usize
            + self.version_support.is_some() as usize
            + self.code_description_support.is_some() as usize
            + self.data_support.is_some() as usize;
        let mut map = serializer.serialize_map(Some(hint))?;
        if let Some(ref v) = self.related_information {
            map.serialize_entry("relatedInformation", v)?;
        }
        if let Some(ref v) = self.tag_support {
            map.serialize_entry("tagSupport", v)?;
        }
        if let Some(ref v) = self.version_support {
            map.serialize_entry("versionSupport", v)?;
        }
        if let Some(ref v) = self.code_description_support {
            map.serialize_entry("codeDescriptionSupport", v)?;
        }
        if let Some(ref v) = self.data_support {
            map.serialize_entry("dataSupport", v)?;
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for PublishDiagnosticsClientCapabilities {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PublishDiagnosticsClientCapabilities;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "PublishDiagnosticsClientCapabilities object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut result = PublishDiagnosticsClientCapabilities::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "relatedInformation" => {
                            result.related_information = Some(map.next_value()?);
                        }
                        "tagSupport" => {
                            result.tag_support = TagSupport::deserialize_compat(
                                map.next_value::<serde_json::Value>()
                                    .map(serde::de::IntoDeserializer::into_deserializer)
                                    .map_err(serde::de::Error::custom)?,
                            )
                            .map_err(serde::de::Error::custom)?;
                        }
                        "versionSupport" => {
                            result.version_support = Some(map.next_value()?);
                        }
                        "codeDescriptionSupport" => {
                            result.code_description_support = Some(map.next_value()?);
                        }
                        "dataSupport" => {
                            result.data_support = Some(map.next_value()?);
                        }
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct TagSupport<T> {
    /// The tags supported by the client.
    pub value_set: Vec<T>,
}

impl<T: serde::Serialize> serde::Serialize for TagSupport<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("valueSet", &self.value_set)?;
        map.end()
    }
}

impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for TagSupport<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: serde::Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
            type Value = TagSupport<T>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "TagSupport object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut value_set = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "valueSet" => {
                            value_set = Some(map.next_value()?);
                        }
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                let value_set =
                    value_set.ok_or_else(|| serde::de::Error::missing_field("valueSet"))?;
                Ok(TagSupport { value_set })
            }
        }
        deserializer.deserialize_map(Visitor(std::marker::PhantomData))
    }
}

impl<T> TagSupport<T> {
    /// Support for deserializing a boolean tag Support, in case it's present.
    ///
    /// This is currently the case for vscode 1.41.1
    pub(crate) fn deserialize_compat<'de, S>(
        serializer: S,
    ) -> Result<Option<TagSupport<T>>, S::Error>
    where
        S: serde::Deserializer<'de>,
        T: serde::Deserialize<'de>,
    {
        Ok(
            match Option::<Value>::deserialize(serializer).map_err(serde::de::Error::custom)? {
                Some(Value::Bool(false)) => None,
                Some(Value::Bool(true)) => Some(TagSupport { value_set: vec![] }),
                Some(other) => {
                    Some(TagSupport::<T>::deserialize(other).map_err(serde::de::Error::custom)?)
                }
                None => None,
            },
        )
    }
}

/// Text document specific client capabilities.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct TextDocumentClientCapabilities {
    pub synchronization: Option<TextDocumentSyncClientCapabilities>,
    /// Capabilities specific to the `textDocument/completion`
    pub completion: Option<CompletionClientCapabilities>,

    /// Capabilities specific to the `textDocument/hover`
    pub hover: Option<HoverClientCapabilities>,

    /// Capabilities specific to the `textDocument/signatureHelp`
    pub signature_help: Option<SignatureHelpClientCapabilities>,

    /// Capabilities specific to the `textDocument/references`
    pub references: Option<ReferenceClientCapabilities>,

    /// Capabilities specific to the `textDocument/documentHighlight`
    pub document_highlight: Option<DocumentHighlightClientCapabilities>,

    /// Capabilities specific to the `textDocument/documentSymbol`
    pub document_symbol: Option<DocumentSymbolClientCapabilities>,
    /// Capabilities specific to the `textDocument/formatting`
    pub formatting: Option<DocumentFormattingClientCapabilities>,

    /// Capabilities specific to the `textDocument/rangeFormatting`
    pub range_formatting: Option<DocumentRangeFormattingClientCapabilities>,

    /// Capabilities specific to the `textDocument/onTypeFormatting`
    pub on_type_formatting: Option<DocumentOnTypeFormattingClientCapabilities>,

    /// Capabilities specific to the `textDocument/declaration`
    pub declaration: Option<GotoCapability>,

    /// Capabilities specific to the `textDocument/definition`
    pub definition: Option<GotoCapability>,

    /// Capabilities specific to the `textDocument/typeDefinition`
    pub type_definition: Option<GotoCapability>,

    /// Capabilities specific to the `textDocument/implementation`
    pub implementation: Option<GotoCapability>,

    /// Capabilities specific to the `textDocument/codeAction`
    pub code_action: Option<CodeActionClientCapabilities>,

    /// Capabilities specific to the `textDocument/documentColor` and the
    /// `textDocument/colorPresentation` request.
    pub color_provider: Option<DocumentColorClientCapabilities>,

    /// Capabilities specific to the `textDocument/rename`
    pub rename: Option<RenameClientCapabilities>,

    /// Capabilities specific to `textDocument/publishDiagnostics`.
    pub publish_diagnostics: Option<PublishDiagnosticsClientCapabilities>,

    /// Capabilities specific to the `textDocument/semanticTokens/*` requests.
    pub semantic_tokens: Option<SemanticTokensClientCapabilities>,

    /// Capabilities specific to the `textDocument/inlayHint` request.
    ///
    /// @since 3.17.0
    pub inlay_hint: Option<InlayHintClientCapabilities>,
}

impl_lsp_struct!(
    TextDocumentClientCapabilities,
    required {},
    optional {
        synchronization: TextDocumentSyncClientCapabilities = "synchronization",
        completion: CompletionClientCapabilities = "completion",
        hover: HoverClientCapabilities = "hover",
        signature_help: SignatureHelpClientCapabilities = "signatureHelp",
        references: ReferenceClientCapabilities = "references",
        document_highlight: DocumentHighlightClientCapabilities = "documentHighlight",
        document_symbol: DocumentSymbolClientCapabilities = "documentSymbol",
        formatting: DocumentFormattingClientCapabilities = "formatting",
        range_formatting: DocumentRangeFormattingClientCapabilities = "rangeFormatting",
        on_type_formatting: DocumentOnTypeFormattingClientCapabilities = "onTypeFormatting",
        declaration: GotoCapability = "declaration",
        definition: GotoCapability = "definition",
        type_definition: GotoCapability = "typeDefinition",
        implementation: GotoCapability = "implementation",
        code_action: CodeActionClientCapabilities = "codeAction",
        color_provider: DocumentColorClientCapabilities = "colorProvider",
        rename: RenameClientCapabilities = "rename",
        publish_diagnostics: PublishDiagnosticsClientCapabilities = "publishDiagnostics",
        semantic_tokens: SemanticTokensClientCapabilities = "semanticTokens",
        inlay_hint: InlayHintClientCapabilities = "inlayHint",
    }
);

/// Where ClientCapabilities are currently empty:
#[derive(Debug, PartialEq, Clone, Default)]
pub struct ClientCapabilities {
    /// Workspace specific client capabilities.
    pub workspace: Option<WorkspaceClientCapabilities>,

    /// Text document specific client capabilities.
    pub text_document: Option<TextDocumentClientCapabilities>,

    /// Window specific client capabilities.
    pub window: Option<WindowClientCapabilities>,

    /// General client capabilities.
    pub general: Option<GeneralClientCapabilities>,

    /// Unofficial UT8-offsets extension.
    ///
    /// See https://clangd.llvm.org/extensions.html#utf-8-offsets.
    #[cfg(feature = "proposed")]
    pub offset_encoding: Option<Vec<String>>,

    /// Experimental client capabilities.
    pub experimental: Option<Value>,
}

impl serde::Serialize for ClientCapabilities {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut hint = 0usize;
        if self.workspace.is_some() {
            hint += 1;
        }
        if self.text_document.is_some() {
            hint += 1;
        }
        if self.window.is_some() {
            hint += 1;
        }
        if self.general.is_some() {
            hint += 1;
        }
        if self.experimental.is_some() {
            hint += 1;
        }
        #[cfg(feature = "proposed")]
        if self.offset_encoding.is_some() {
            hint += 1;
        }
        let mut map = serializer.serialize_map(Some(hint))?;
        if let Some(ref v) = self.workspace {
            map.serialize_entry("workspace", v)?;
        }
        if let Some(ref v) = self.text_document {
            map.serialize_entry("textDocument", v)?;
        }
        if let Some(ref v) = self.window {
            map.serialize_entry("window", v)?;
        }
        if let Some(ref v) = self.general {
            map.serialize_entry("general", v)?;
        }
        if let Some(ref v) = self.experimental {
            map.serialize_entry("experimental", v)?;
        }
        #[cfg(feature = "proposed")]
        if let Some(ref v) = self.offset_encoding {
            map.serialize_entry("offsetEncoding", v)?;
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for ClientCapabilities {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ClientCapabilities;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "ClientCapabilities object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut result = ClientCapabilities::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "workspace" => {
                            result.workspace = Some(map.next_value()?);
                        }
                        "textDocument" => {
                            result.text_document = Some(map.next_value()?);
                        }
                        "window" => {
                            result.window = Some(map.next_value()?);
                        }
                        "general" => {
                            result.general = Some(map.next_value()?);
                        }
                        "experimental" => {
                            result.experimental = Some(map.next_value()?);
                        }
                        #[cfg(feature = "proposed")]
                        "offsetEncoding" => {
                            result.offset_encoding = Some(map.next_value()?);
                        }
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

#[derive(Debug, PartialEq, Clone, Default)]
pub struct GeneralClientCapabilities {
    /// Client capabilities specific to regular expressions.
    ///
    /// @since 3.16.0
    pub regular_expressions: Option<RegularExpressionsClientCapabilities>,

    /// Client capabilities specific to the client's markdown parser.
    ///
    /// @since 3.16.0
    pub markdown: Option<MarkdownClientCapabilities>,

    /// Client capability that signals how the client handles stale requests (e.g. a request for
    /// which the client will not process the response anymore since the information is outdated).
    ///
    /// @since 3.17.0
    pub stale_request_support: Option<StaleRequestSupportClientCapabilities>,

    /// The position encodings supported by the client. Client and server
    /// have to agree on the same position encoding to ensure that offsets
    /// (e.g. character position in a line) are interpreted the same on both
    /// side.
    ///
    /// To keep the protocol backwards compatible the following applies: if
    /// the value 'utf-16' is missing from the array of position encodings
    /// servers can assume that the client supports UTF-16. UTF-16 is
    /// therefore a mandatory encoding.
    ///
    /// If omitted it defaults to ['utf-16'].
    ///
    /// Implementation considerations: since the conversion from one encoding
    /// into another requires the content of the file / line the conversion
    /// is best done where the file is read which is usually on the server
    /// side.
    ///
    /// @since 3.17.0
    pub position_encodings: Option<Vec<PositionEncodingKind>>,
}

impl_lsp_struct!(GeneralClientCapabilities,
    required {},
    optional {
        regular_expressions: RegularExpressionsClientCapabilities = "regularExpressions",
        markdown: MarkdownClientCapabilities = "markdown",
        stale_request_support: StaleRequestSupportClientCapabilities = "staleRequestSupport",
        position_encodings: Vec<PositionEncodingKind> = "positionEncodings",
    }
);

/// Client capability that signals how the client
/// handles stale requests (e.g. a request
/// for which the client will not process the response
/// anymore since the information is outdated).
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Clone, Default)]
pub struct StaleRequestSupportClientCapabilities {
    /// The client will actively cancel the request.
    pub cancel: bool,

    /// The list of requests for which the client
    /// will retry the request if it receives a
    /// response with error code `ContentModified``
    pub retry_on_content_modified: Vec<String>,
}

impl_lsp_struct!(StaleRequestSupportClientCapabilities,
    required {
        cancel: bool = "cancel",
        retry_on_content_modified: Vec<String> = "retryOnContentModified",
    },
    optional {}
);

#[derive(Debug, PartialEq, Clone, Default)]
pub struct RegularExpressionsClientCapabilities {
    /// The engine's name.
    pub engine: String,

    /// The engine's version
    pub version: Option<String>,
}

impl_lsp_struct!(
    RegularExpressionsClientCapabilities,
    required {
        engine: String = "engine"
    },
    optional {
        version: String = "version"
    }
);

#[derive(Debug, PartialEq, Clone, Default)]
pub struct MarkdownClientCapabilities {
    /// The name of the parser.
    pub parser: String,

    /// The version of the parser.
    pub version: Option<String>,

    /// A list of HTML tags that the client allows / supports in
    /// Markdown.
    ///
    /// @since 3.17.0
    pub allowed_tags: Option<Vec<String>>,
}

impl_lsp_struct!(MarkdownClientCapabilities,
    required { parser: String = "parser" },
    optional {
        version: String = "version",
        allowed_tags: Vec<String> = "allowedTags",
    }
);

#[derive(Debug, PartialEq, Clone, Default)]
pub struct InitializeResult {
    /// The capabilities the language server provides.
    pub capabilities: ServerCapabilities,

    /// Information about the server.
    pub server_info: Option<ServerInfo>,

    /// Unofficial UT8-offsets extension.
    ///
    /// See https://clangd.llvm.org/extensions.html#utf-8-offsets.
    #[cfg(feature = "proposed")]
    pub offset_encoding: Option<String>,
}

impl serde::Serialize for InitializeResult {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut hint = 1usize;
        if self.server_info.is_some() {
            hint += 1;
        }
        #[cfg(feature = "proposed")]
        if self.offset_encoding.is_some() {
            hint += 1;
        }
        let mut map = serializer.serialize_map(Some(hint))?;
        map.serialize_entry("capabilities", &self.capabilities)?;
        if let Some(ref v) = self.server_info {
            map.serialize_entry("serverInfo", v)?;
        }
        #[cfg(feature = "proposed")]
        if let Some(ref v) = self.offset_encoding {
            map.serialize_entry("offsetEncoding", v)?;
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for InitializeResult {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = InitializeResult;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "InitializeResult object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut result = InitializeResult::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "capabilities" => {
                            result.capabilities = map.next_value()?;
                        }
                        "serverInfo" => {
                            result.server_info = Some(map.next_value()?);
                        }
                        #[cfg(feature = "proposed")]
                        "offsetEncoding" => {
                            result.offset_encoding = Some(map.next_value()?);
                        }
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct ServerInfo {
    /// The name of the server as defined by the server.
    pub name: String,
    /// The servers's version as defined by the server.
    pub version: Option<String>,
}

impl_lsp_struct!(
    ServerInfo,
    required {
        name: String = "name"
    },
    optional {
        version: String = "version"
    }
);

// The server can signal the following capabilities:

/// Defines how the host (editor) should sync document changes to the language server.
#[derive(Eq, PartialEq, Clone, Copy)]
pub struct TextDocumentSyncKind(i32);
lsp_enum! {
impl TextDocumentSyncKind {
    /// Documents should not be synced at all.
    pub const NONE: TextDocumentSyncKind = TextDocumentSyncKind(0);

    /// Documents are synced by always sending the full content of the document.
    pub const FULL: TextDocumentSyncKind = TextDocumentSyncKind(1);

    /// Documents are synced by sending the full content on open. After that only
    /// incremental updates to the document are sent.
    pub const INCREMENTAL: TextDocumentSyncKind = TextDocumentSyncKind(2);
}
}

pub(crate) type ExecuteCommandClientCapabilities = DynamicRegistrationClientCapabilities;

/// Execute command options.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct ExecuteCommandOptions {
    /// The commands to be executed on the server
    pub commands: Vec<String>,

    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(ExecuteCommandOptions,
    required { commands: Vec<String> = "commands" },
    optional {},
    flatten { work_done_progress_options: WorkDoneProgressOptions }
);

/// Save options.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SaveOptions {
    /// The client is supposed to include the content on save.
    pub include_text: Option<bool>,
}

impl_lsp_struct!(
    SaveOptions,
    required {},
    optional {
        include_text: bool = "includeText"
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum TextDocumentSyncSaveOptions {
    Supported(bool),
    SaveOptions(SaveOptions),
}

impl_untagged_enum!(TextDocumentSyncSaveOptions {
    Supported(bool),
    SaveOptions(SaveOptions),
});

impl From<SaveOptions> for TextDocumentSyncSaveOptions {
    fn from(from: SaveOptions) -> Self {
        Self::SaveOptions(from)
    }
}

impl From<bool> for TextDocumentSyncSaveOptions {
    fn from(from: bool) -> Self {
        Self::Supported(from)
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct TextDocumentSyncOptions {
    /// Open and close notifications are sent to the server.
    pub open_close: Option<bool>,

    /// Change notifications are sent to the server. See TextDocumentSyncKind.None, TextDocumentSyncKind.Full
    /// and TextDocumentSyncKind.Incremental.
    pub change: Option<TextDocumentSyncKind>,

    /// Will save notifications are sent to the server.
    pub will_save: Option<bool>,

    /// Will save wait until requests are sent to the server.
    pub will_save_wait_until: Option<bool>,

    /// Save notifications are sent to the server.
    pub save: Option<TextDocumentSyncSaveOptions>,
}

impl_lsp_struct!(
    TextDocumentSyncOptions,
    required {},
    optional {
        open_close: bool = "openClose",
        change: TextDocumentSyncKind = "change",
        will_save: bool = "willSave",
        will_save_wait_until: bool = "willSaveWaitUntil",
        save: TextDocumentSyncSaveOptions = "save",
    }
);

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Clone)]
pub enum OneOf<A, B> {
    Left(A),
    Right(B),
}

impl<A, B> serde::Serialize for OneOf<A, B>
where
    A: serde::Serialize,
    B: serde::Serialize,
{
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            OneOf::Left(v) => v.serialize(s),
            OneOf::Right(v) => v.serialize(s),
        }
    }
}

impl<'de, A, B> serde::Deserialize<'de> for OneOf<A, B>
where
    A: serde::de::DeserializeOwned,
    B: serde::de::DeserializeOwned,
{
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let val = serde_json::Value::deserialize(d)?;
        if let Ok(v) = serde_json::from_value::<A>(val.clone()) {
            return Ok(OneOf::Left(v));
        }
        if let Ok(v) = serde_json::from_value::<B>(val) {
            return Ok(OneOf::Right(v));
        }
        Err(serde::de::Error::custom(
            "data did not match either variant of OneOf",
        ))
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum TextDocumentSyncCapability {
    Kind(TextDocumentSyncKind),
    Options(TextDocumentSyncOptions),
}

impl_untagged_enum!(TextDocumentSyncCapability {
    Kind(TextDocumentSyncKind),
    Options(TextDocumentSyncOptions),
});

impl From<TextDocumentSyncOptions> for TextDocumentSyncCapability {
    fn from(from: TextDocumentSyncOptions) -> Self {
        Self::Options(from)
    }
}

impl From<TextDocumentSyncKind> for TextDocumentSyncCapability {
    fn from(from: TextDocumentSyncKind) -> Self {
        Self::Kind(from)
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum ImplementationProviderCapability {
    Simple(bool),
    Options(StaticTextDocumentRegistrationOptions),
}

impl_untagged_enum!(ImplementationProviderCapability {
    Simple(bool),
    Options(StaticTextDocumentRegistrationOptions),
});

impl From<StaticTextDocumentRegistrationOptions> for ImplementationProviderCapability {
    fn from(from: StaticTextDocumentRegistrationOptions) -> Self {
        Self::Options(from)
    }
}

impl From<bool> for ImplementationProviderCapability {
    fn from(from: bool) -> Self {
        Self::Simple(from)
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum TypeDefinitionProviderCapability {
    Simple(bool),
    Options(StaticTextDocumentRegistrationOptions),
}

impl_untagged_enum!(TypeDefinitionProviderCapability {
    Simple(bool),
    Options(StaticTextDocumentRegistrationOptions),
});

impl From<StaticTextDocumentRegistrationOptions> for TypeDefinitionProviderCapability {
    fn from(from: StaticTextDocumentRegistrationOptions) -> Self {
        Self::Options(from)
    }
}

impl From<bool> for TypeDefinitionProviderCapability {
    fn from(from: bool) -> Self {
        Self::Simple(from)
    }
}

#[derive(Debug, PartialEq, Clone, Default)]
pub struct ServerCapabilities {
    /// The position encoding the server picked from the encodings offered
    /// by the client via the client capability `general.positionEncodings`.
    ///
    /// If the client didn't provide any position encodings the only valid
    /// value that a server can return is 'utf-16'.
    ///
    /// If omitted it defaults to 'utf-16'.
    ///
    /// @since 3.17.0
    pub position_encoding: Option<PositionEncodingKind>,

    /// Defines how text documents are synced.
    pub text_document_sync: Option<TextDocumentSyncCapability>,

    /// The server provides hover support.
    pub hover_provider: Option<HoverProviderCapability>,

    /// The server provides completion support.
    pub completion_provider: Option<CompletionOptions>,

    /// The server provides signature help support.
    pub signature_help_provider: Option<SignatureHelpOptions>,

    /// The server provides goto definition support.
    pub definition_provider: Option<OneOf<bool, DefinitionOptions>>,

    /// The server provides goto type definition support.
    pub type_definition_provider: Option<TypeDefinitionProviderCapability>,

    /// The server provides goto implementation support.
    pub implementation_provider: Option<ImplementationProviderCapability>,

    /// The server provides find references support.
    pub references_provider: Option<OneOf<bool, ReferencesOptions>>,

    /// The server provides document highlight support.
    pub document_highlight_provider: Option<OneOf<bool, DocumentHighlightOptions>>,

    /// The server provides document symbol support.
    pub document_symbol_provider: Option<OneOf<bool, DocumentSymbolOptions>>,

    /// The server provides workspace symbol support.
    pub workspace_symbol_provider: Option<OneOf<bool, WorkspaceSymbolOptions>>,

    /// The server provides code actions.
    pub code_action_provider: Option<CodeActionProviderCapability>,

    /// The server provides document formatting.
    pub document_formatting_provider: Option<OneOf<bool, DocumentFormattingOptions>>,

    /// The server provides document range formatting.
    pub document_range_formatting_provider: Option<OneOf<bool, DocumentRangeFormattingOptions>>,

    /// The server provides document formatting on typing.
    pub document_on_type_formatting_provider: Option<DocumentOnTypeFormattingOptions>,

    /// The server provides rename support.
    pub rename_provider: Option<OneOf<bool, RenameOptions>>,

    /// The server provides color provider support.
    pub color_provider: Option<ColorProviderCapability>,

    /// The server provides go to declaration support.
    pub declaration_provider: Option<DeclarationCapability>,

    /// The server provides execute command support.
    pub execute_command_provider: Option<ExecuteCommandOptions>,

    /// Workspace specific server capabilities
    pub workspace: Option<WorkspaceServerCapabilities>,

    /// Semantic tokens server capabilities.
    pub semantic_tokens_provider: Option<SemanticTokensServerCapabilities>,

    /// The server provides inlay hints.
    ///
    /// @since 3.17.0
    pub inlay_hint_provider: Option<OneOf<bool, InlayHintServerCapabilities>>,

    /// Experimental server capabilities.
    pub experimental: Option<Value>,
}

impl serde::Serialize for ServerCapabilities {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        if let Some(ref v) = self.position_encoding {
            map.serialize_entry("positionEncoding", v)?;
        }
        if let Some(ref v) = self.text_document_sync {
            map.serialize_entry("textDocumentSync", v)?;
        }
        if let Some(ref v) = self.hover_provider {
            map.serialize_entry("hoverProvider", v)?;
        }
        if let Some(ref v) = self.completion_provider {
            map.serialize_entry("completionProvider", v)?;
        }
        if let Some(ref v) = self.signature_help_provider {
            map.serialize_entry("signatureHelpProvider", v)?;
        }
        if let Some(ref v) = self.definition_provider {
            map.serialize_entry("definitionProvider", v)?;
        }
        if let Some(ref v) = self.type_definition_provider {
            map.serialize_entry("typeDefinitionProvider", v)?;
        }
        if let Some(ref v) = self.implementation_provider {
            map.serialize_entry("implementationProvider", v)?;
        }
        if let Some(ref v) = self.references_provider {
            map.serialize_entry("referencesProvider", v)?;
        }
        if let Some(ref v) = self.document_highlight_provider {
            map.serialize_entry("documentHighlightProvider", v)?;
        }
        if let Some(ref v) = self.document_symbol_provider {
            map.serialize_entry("documentSymbolProvider", v)?;
        }
        if let Some(ref v) = self.workspace_symbol_provider {
            map.serialize_entry("workspaceSymbolProvider", v)?;
        }
        if let Some(ref v) = self.code_action_provider {
            map.serialize_entry("codeActionProvider", v)?;
        }
        if let Some(ref v) = self.document_formatting_provider {
            map.serialize_entry("documentFormattingProvider", v)?;
        }
        if let Some(ref v) = self.document_range_formatting_provider {
            map.serialize_entry("documentRangeFormattingProvider", v)?;
        }
        if let Some(ref v) = self.document_on_type_formatting_provider {
            map.serialize_entry("documentOnTypeFormattingProvider", v)?;
        }
        if let Some(ref v) = self.rename_provider {
            map.serialize_entry("renameProvider", v)?;
        }
        if let Some(ref v) = self.color_provider {
            map.serialize_entry("colorProvider", v)?;
        }
        if let Some(ref v) = self.declaration_provider {
            map.serialize_entry("declarationProvider", v)?;
        }
        if let Some(ref v) = self.execute_command_provider {
            map.serialize_entry("executeCommandProvider", v)?;
        }
        if let Some(ref v) = self.workspace {
            map.serialize_entry("workspace", v)?;
        }
        if let Some(ref v) = self.semantic_tokens_provider {
            map.serialize_entry("semanticTokensProvider", v)?;
        }
        if let Some(ref v) = self.inlay_hint_provider {
            map.serialize_entry("inlayHintProvider", v)?;
        }
        if let Some(ref v) = self.experimental {
            map.serialize_entry("experimental", v)?;
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for ServerCapabilities {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ServerCapabilities;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "ServerCapabilities object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut result = ServerCapabilities::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "positionEncoding" => {
                            result.position_encoding = Some(map.next_value()?);
                        }
                        "textDocumentSync" => {
                            result.text_document_sync = Some(map.next_value()?);
                        }
                        "hoverProvider" => {
                            result.hover_provider = Some(map.next_value()?);
                        }
                        "completionProvider" => {
                            result.completion_provider = Some(map.next_value()?);
                        }
                        "signatureHelpProvider" => {
                            result.signature_help_provider = Some(map.next_value()?);
                        }
                        "definitionProvider" => {
                            result.definition_provider = Some(map.next_value()?);
                        }
                        "typeDefinitionProvider" => {
                            result.type_definition_provider = Some(map.next_value()?);
                        }
                        "implementationProvider" => {
                            result.implementation_provider = Some(map.next_value()?);
                        }
                        "referencesProvider" => {
                            result.references_provider = Some(map.next_value()?);
                        }
                        "documentHighlightProvider" => {
                            result.document_highlight_provider = Some(map.next_value()?);
                        }
                        "documentSymbolProvider" => {
                            result.document_symbol_provider = Some(map.next_value()?);
                        }
                        "workspaceSymbolProvider" => {
                            result.workspace_symbol_provider = Some(map.next_value()?);
                        }
                        "codeActionProvider" => {
                            result.code_action_provider = Some(map.next_value()?);
                        }
                        "documentFormattingProvider" => {
                            result.document_formatting_provider = Some(map.next_value()?);
                        }
                        "documentRangeFormattingProvider" => {
                            result.document_range_formatting_provider = Some(map.next_value()?);
                        }
                        "documentOnTypeFormattingProvider" => {
                            result.document_on_type_formatting_provider = Some(map.next_value()?);
                        }
                        "renameProvider" => {
                            result.rename_provider = Some(map.next_value()?);
                        }
                        "colorProvider" => {
                            result.color_provider = Some(map.next_value()?);
                        }
                        "declarationProvider" => {
                            result.declaration_provider = Some(map.next_value()?);
                        }
                        "executeCommandProvider" => {
                            result.execute_command_provider = Some(map.next_value()?);
                        }
                        "workspace" => {
                            result.workspace = Some(map.next_value()?);
                        }
                        "semanticTokensProvider" => {
                            result.semantic_tokens_provider = Some(map.next_value()?);
                        }
                        "inlayHintProvider" => {
                            result.inlay_hint_provider = Some(map.next_value()?);
                        }
                        "experimental" => {
                            result.experimental = Some(map.next_value()?);
                        }
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceServerCapabilities {
    /// The server supports workspace folder.
    pub workspace_folders: Option<WorkspaceFoldersServerCapabilities>,
}

impl_lsp_struct!(
    WorkspaceServerCapabilities,
    required {},
    optional {
        workspace_folders: WorkspaceFoldersServerCapabilities = "workspaceFolders",
    }
);

/// Since most of the registration options require to specify a document selector there is a base
/// interface that can be used.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct TextDocumentRegistrationOptions {
    /// A document selector to identify the scope of the registration. If set to null
    /// the document selector provided on the client side will be used.
    pub document_selector: Option<DocumentSelector>,
}

impl_lsp_struct!(
    TextDocumentRegistrationOptions,
    required {},
    nullable {
        document_selector: DocumentSelector = "documentSelector"
    },
    optional {}
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum DeclarationCapability {
    Simple(bool),
    RegistrationOptions(DeclarationRegistrationOptions),
    Options(DeclarationOptions),
}

impl_untagged_enum!(DeclarationCapability {
    Simple(bool),
    RegistrationOptions(DeclarationRegistrationOptions),
    Options(DeclarationOptions),
});

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DeclarationRegistrationOptions {
    pub declaration_options: DeclarationOptions,
    pub text_document_registration_options: TextDocumentRegistrationOptions,
    pub static_registration_options: StaticRegistrationOptions,
}

impl_lsp_struct!(
    DeclarationRegistrationOptions,
    flatten {
        declaration_options: DeclarationOptions,
        text_document_registration_options: TextDocumentRegistrationOptions,
        static_registration_options: StaticRegistrationOptions,
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DeclarationOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    DeclarationOptions,
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct StaticRegistrationOptions {
    pub id: Option<String>,
}

impl_lsp_struct!(
    StaticRegistrationOptions,
    required {},
    optional { id: String = "id" }
);

#[derive(Debug, Default, Eq, PartialEq, Clone, Copy)]
pub struct WorkDoneProgressOptions {
    pub work_done_progress: Option<bool>,
}

impl_lsp_struct!(
    WorkDoneProgressOptions,
    required {},
    optional {
        work_done_progress: bool = "workDoneProgress"
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct DocumentFormattingOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    DocumentFormattingOptions,
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentRangeFormattingOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    DocumentRangeFormattingOptions,
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DefinitionOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    DefinitionOptions,
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentSymbolOptions {
    /// A human-readable string that is shown when multiple outlines trees are
    /// shown for the same document.
    ///
    /// @since 3.16.0
    pub label: Option<String>,

    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    DocumentSymbolOptions,
    optional {
        label: String = "label"
    },
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct ReferencesOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    ReferencesOptions,
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentHighlightOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    DocumentHighlightOptions,
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct WorkspaceSymbolOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,

    /// The server provides support to resolve additional
    /// information for a workspace symbol.
    ///
    /// @since 3.17.0
    pub resolve_provider: Option<bool>,
}

impl_lsp_struct!(
    WorkspaceSymbolOptions,
    optional {
        resolve_provider: bool = "resolveProvider"
    },
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct StaticTextDocumentRegistrationOptions {
    /// A document selector to identify the scope of the registration. If set to null
    /// the document selector provided on the client side will be used.
    pub document_selector: Option<DocumentSelector>,

    pub id: Option<String>,
}

impl_lsp_struct!(
    StaticTextDocumentRegistrationOptions,
    required {},
    nullable {
        document_selector: DocumentSelector = "documentSelector"
    },
    optional { id: String = "id" }
);

#[derive(Debug, Eq, PartialEq, Clone, Copy, Default)]
pub struct DidChangeWatchedFilesClientCapabilities {
    /// Did change watched files notification supports dynamic registration.
    /// Please note that the current protocol doesn't support static
    /// configuration for file changes from the server side.
    pub dynamic_registration: Option<bool>,

    /// Whether the client has support for relative patterns
    /// or not.
    ///
    /// @since 3.17.0
    pub relative_pattern_support: Option<bool>,
}

impl_lsp_struct!(
    DidChangeWatchedFilesClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        relative_pattern_support: bool = "relativePatternSupport",
    }
);

/// The glob pattern to watch relative to the base path. Glob patterns can have
/// the following syntax:
/// - `*` to match one or more characters in a path segment
/// - `?` to match on one character in a path segment
/// - `**` to match any number of path segments, including none
/// - `{}` to group conditions (e.g. `**​/*.{ts,js}` matches all TypeScript
///   and JavaScript files)
/// - `[]` to declare a range of characters to match in a path segment
///   (e.g., `example.[0-9]` to match on `example.0`, `example.1`, …)
/// - `[!...]` to negate a range of characters to match in a path segment
///   (e.g., `example.[!0-9]` to match on `example.a`, `example.b`,
///   but not `example.0`)
///
/// @since 3.17.0
pub type Pattern = String;

bitflags! {
pub struct WatchKind: u8 {
    /// Interested in create events.
    const Create = 1;
    /// Interested in change events
    const Change = 2;
    /// Interested in delete events
    const Delete = 4;
}
}

impl<'de> serde::Deserialize<'de> for WatchKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let i = u8::deserialize(deserializer)?;
        WatchKind::from_bits(i).ok_or_else(|| {
            D::Error::invalid_value(de::Unexpected::Unsigned(u64::from(i)), &"Unknown flag")
        })
    }
}

impl serde::Serialize for WatchKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(self.bits())
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

impl PublishDiagnosticsParams {
    pub fn new(
        uri: Uri,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
    ) -> PublishDiagnosticsParams {
        PublishDiagnosticsParams {
            uri,
            diagnostics,
            version,
        }
    }
}

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

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct GotoDefinitionParams {
    pub text_document_position_params: TextDocumentPositionParams,
    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(
    GotoDefinitionParams,
    flatten {
        text_document_position_params: TextDocumentPositionParams,
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

/// GotoDefinition response can be single location, or multiple Locations or a link.
#[derive(Debug, PartialEq, Clone)]
pub enum GotoDefinitionResponse {
    Scalar(Location),
    Array(Vec<Location>),
    Link(Vec<LocationLink>),
}

impl_untagged_enum!(GotoDefinitionResponse {
    Scalar(Location),
    Array(Vec<Location>),
    Link(Vec<LocationLink>),
});

impl From<Location> for GotoDefinitionResponse {
    fn from(location: Location) -> Self {
        GotoDefinitionResponse::Scalar(location)
    }
}

impl From<Vec<Location>> for GotoDefinitionResponse {
    fn from(locations: Vec<Location>) -> Self {
        GotoDefinitionResponse::Array(locations)
    }
}

impl From<Vec<LocationLink>> for GotoDefinitionResponse {
    fn from(locations: Vec<LocationLink>) -> Self {
        GotoDefinitionResponse::Link(locations)
    }
}

#[derive(Debug, PartialEq, Clone, Default)]
pub struct ExecuteCommandParams {
    /// The identifier of the actual command handler.
    pub command: String,
    /// Arguments that the command should be invoked with.
    pub arguments: Vec<Value>,

    pub work_done_progress_params: WorkDoneProgressParams,
}

impl_lsp_struct!(ExecuteCommandParams,
    required {
        command: String = "command",
        arguments: Vec<Value> = "arguments",
    },
    optional {},
    flatten { work_done_progress_params: WorkDoneProgressParams }
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

/// A parameter literal used to pass a partial result token.
#[derive(Debug, Eq, PartialEq, Default, Clone)]
pub struct PartialResultParams {
    pub partial_result_token: Option<ProgressToken>,
}

impl_lsp_struct!(
    PartialResultParams,
    required {},
    optional {
        partial_result_token: ProgressToken = "partialResultToken"
    }
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
    fn one_of() {
        test_serialization(&OneOf::<bool, ()>::Left(true), r#"true"#);
        test_serialization(&OneOf::<String, ()>::Left("abcd".into()), r#""abcd""#);
        test_serialization(
            &OneOf::<String, WorkDoneProgressOptions>::Right(WorkDoneProgressOptions {
                work_done_progress: Some(false),
            }),
            r#"{"workDoneProgress":false}"#,
        );
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
                document_changes: None,
                ..Default::default()
            },
            r#"{"changes":{}}"#,
        );

        test_serialization(
            &WorkspaceEdit {
                changes: None,
                document_changes: None,
                ..Default::default()
            },
            r#"{}"#,
        );

        test_serialization(
            &WorkspaceEdit {
                changes: Some(
                    vec![("file://test".parse().unwrap(), vec![])]
                        .into_iter()
                        .collect(),
                ),
                document_changes: None,
                ..Default::default()
            },
            r#"{"changes":{"file://test":[]}}"#,
        );
    }

    #[test]
    fn root_uri_can_be_missing() {
        serde_json::from_str::<InitializeParams>(r#"{ "capabilities": {} }"#).unwrap();
    }

    #[test]
    fn test_watch_kind() {
        test_serialization(&WatchKind::Create, "1");
        test_serialization(&(WatchKind::Create | WatchKind::Change), "3");
        test_serialization(
            &(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            "7",
        );
    }

    #[test]
    fn test_resource_operation_kind() {
        test_serialization(
            &vec![
                ResourceOperationKind::Create,
                ResourceOperationKind::Rename,
                ResourceOperationKind::Delete,
            ],
            r#"["create","rename","delete"]"#,
        );
    }
}
