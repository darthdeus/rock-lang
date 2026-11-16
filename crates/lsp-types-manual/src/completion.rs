use crate::{Command, Documentation, TextDocumentPositionParams, TextEdit};

use serde_json::Value;
use std::fmt::Debug;

/// Defines how to interpret the insert text in a completion item
#[derive(Eq, PartialEq, Clone, Copy)]
pub struct InsertTextFormat(i32);
lsp_enum! {
impl InsertTextFormat {
    pub const PLAIN_TEXT: InsertTextFormat = InsertTextFormat(1);
    pub const SNIPPET: InsertTextFormat = InsertTextFormat(2);
}
}

/// The kind of a completion entry.
#[derive(Eq, PartialEq, Clone, Copy)]
pub struct CompletionItemKind(i32);
lsp_enum! {
impl CompletionItemKind {
    pub const TEXT: CompletionItemKind = CompletionItemKind(1);
    pub const METHOD: CompletionItemKind = CompletionItemKind(2);
    pub const FUNCTION: CompletionItemKind = CompletionItemKind(3);
    pub const CONSTRUCTOR: CompletionItemKind = CompletionItemKind(4);
    pub const FIELD: CompletionItemKind = CompletionItemKind(5);
    pub const VARIABLE: CompletionItemKind = CompletionItemKind(6);
    pub const CLASS: CompletionItemKind = CompletionItemKind(7);
    pub const INTERFACE: CompletionItemKind = CompletionItemKind(8);
    pub const MODULE: CompletionItemKind = CompletionItemKind(9);
    pub const PROPERTY: CompletionItemKind = CompletionItemKind(10);
    pub const UNIT: CompletionItemKind = CompletionItemKind(11);
    pub const VALUE: CompletionItemKind = CompletionItemKind(12);
    pub const ENUM: CompletionItemKind = CompletionItemKind(13);
    pub const KEYWORD: CompletionItemKind = CompletionItemKind(14);
    pub const SNIPPET: CompletionItemKind = CompletionItemKind(15);
    pub const COLOR: CompletionItemKind = CompletionItemKind(16);
    pub const FILE: CompletionItemKind = CompletionItemKind(17);
    pub const REFERENCE: CompletionItemKind = CompletionItemKind(18);
    pub const FOLDER: CompletionItemKind = CompletionItemKind(19);
    pub const ENUM_MEMBER: CompletionItemKind = CompletionItemKind(20);
    pub const CONSTANT: CompletionItemKind = CompletionItemKind(21);
    pub const STRUCT: CompletionItemKind = CompletionItemKind(22);
    pub const EVENT: CompletionItemKind = CompletionItemKind(23);
    pub const OPERATOR: CompletionItemKind = CompletionItemKind(24);
    pub const TYPE_PARAMETER: CompletionItemKind = CompletionItemKind(25);
}
}

#[derive(Debug, PartialEq, Clone)]
pub struct CompletionParams {
    pub text_document_position: TextDocumentPositionParams,
    pub context: Option<CompletionContext>,
}

impl_lsp_struct!(
    CompletionParams,
    optional {
        context: CompletionContext = "context"
    },
    flatten {
        text_document_position: TextDocumentPositionParams,
    }
);

#[derive(Debug, PartialEq, Clone)]
pub struct CompletionContext {
    /// How the completion was triggered.
    pub trigger_kind: CompletionTriggerKind,

    /// The trigger character (a single character) that has trigger code complete.
    /// Is undefined if `triggerKind !== CompletionTriggerKind.TriggerCharacter`
    pub trigger_character: Option<String>,
}

impl_lsp_struct!(
    CompletionContext,
    required {
        trigger_kind: CompletionTriggerKind = "triggerKind"
    },
    optional {
        trigger_character: String = "triggerCharacter"
    }
);

/// How a completion was triggered.
#[derive(Eq, PartialEq, Clone, Copy)]
pub struct CompletionTriggerKind(i32);
lsp_enum! {
impl CompletionTriggerKind {
    pub const INVOKED: CompletionTriggerKind = CompletionTriggerKind(1);
    pub const TRIGGER_CHARACTER: CompletionTriggerKind = CompletionTriggerKind(2);
    pub const TRIGGER_FOR_INCOMPLETE_COMPLETIONS: CompletionTriggerKind = CompletionTriggerKind(3);
}
}

/// Represents a collection of [completion items](#CompletionItem) to be presented
/// in the editor.
#[derive(Debug, PartialEq, Clone, Default)]
pub struct CompletionList {
    /// This list it not complete. Further typing should result in recomputing
    /// this list.
    pub is_incomplete: bool,

    /// The completion items.
    pub items: Vec<CompletionItem>,
}

impl_lsp_struct!(CompletionList,
    required {
        is_incomplete: bool = "isIncomplete",
        items: Vec<CompletionItem> = "items",
    },
    optional {}
);

#[derive(Debug, PartialEq, Default, Clone)]
pub struct CompletionItem {
    /// The label of this completion item. By default
    /// also the text that is inserted when selecting
    /// this completion.
    pub label: String,

    /// Additional details for the label
    ///
    /// @since 3.17.0
    pub label_details: Option<CompletionItemLabelDetails>,

    /// The kind of this completion item. Based of the kind
    /// an icon is chosen by the editor.
    pub kind: Option<CompletionItemKind>,

    /// A human-readable string with additional information
    /// about this item, like type or symbol information.
    pub detail: Option<String>,

    /// A human-readable string that represents a doc-comment.
    pub documentation: Option<Documentation>,

    /// Indicates if this item is deprecated.
    pub deprecated: Option<bool>,

    /// A string that should be used when comparing this item
    /// with other items. When `falsy` the label is used
    /// as the sort text for this item.
    pub sort_text: Option<String>,

    /// A string that should be used when filtering a set of
    /// completion items. When `falsy` the label is used as the
    /// filter text for this item.
    pub filter_text: Option<String>,

    /// A string that should be inserted into a document when selecting
    /// this completion. When `falsy` the label is used as the insert text
    /// for this item.
    pub insert_text: Option<String>,

    /// The format of the insert text. The format applies to both the `insertText` property
    /// and the `newText` property of a provided `textEdit`. If omitted defaults to `InsertTextFormat.PlainText`.
    ///
    /// @since 3.16.0
    pub insert_text_format: Option<InsertTextFormat>,

    /// An optional array of additional text edits that are applied when
    /// selecting this completion. Edits must not overlap with the main edit
    /// nor with themselves.
    pub additional_text_edits: Option<Vec<TextEdit>>,

    /// An optional command that is executed *after* inserting this completion.
    pub command: Option<Command>,

    /// An data entry field that is preserved on a completion item between
    /// a completion and a completion resolve request.
    pub data: Option<Value>,
}

impl_lsp_struct!(CompletionItem,
    required { label: String = "label" },
    optional {
        label_details: CompletionItemLabelDetails = "labelDetails",
        kind: CompletionItemKind = "kind",
        detail: String = "detail",
        documentation: Documentation = "documentation",
        deprecated: bool = "deprecated",
        sort_text: String = "sortText",
        filter_text: String = "filterText",
        insert_text: String = "insertText",
        insert_text_format: InsertTextFormat = "insertTextFormat",
        additional_text_edits: Vec<TextEdit> = "additionalTextEdits",
        command: Command = "command",
        data: Value = "data",
    }
);

/// Additional details for a completion item label.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Default, Clone)]
pub struct CompletionItemLabelDetails {
    /// An optional string which is rendered less prominently directly after
    /// {@link CompletionItemLabel.label label}, without any spacing. Should be
    /// used for function signatures or type annotations.
    pub detail: Option<String>,

    /// An optional string which is rendered less prominently after
    /// {@link CompletionItemLabel.detail}. Should be used for fully qualified
    /// names or file path.
    pub description: Option<String>,
}

impl_lsp_struct!(
    CompletionItemLabelDetails,
    required {},
    optional {
        detail: String = "detail",
        description: String = "description",
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_enum() {
        assert_eq!(format!("{:?}", CompletionItemKind::TEXT), "Text");
        assert_eq!(
            format!("{:?}", CompletionItemKind::TYPE_PARAMETER),
            "TypeParameter"
        );
    }

    #[test]
    fn test_try_from_enum() {
        use std::convert::TryInto;
        assert_eq!("Text".try_into(), Ok(CompletionItemKind::TEXT));
        assert_eq!(
            "TypeParameter".try_into(),
            Ok(CompletionItemKind::TYPE_PARAMETER)
        );
    }
}
