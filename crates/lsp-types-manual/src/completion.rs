use crate::{Command, Position, TextDocumentIdentifier, TextEdit};

/// Defines how to interpret the insert text in a completion item
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct InsertTextFormat(i32);
lsp_enum! {
impl InsertTextFormat {
    pub const SNIPPET: InsertTextFormat = InsertTextFormat(2);
}
}

/// The kind of a completion entry.
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct CompletionItemKind(i32);
lsp_enum! {
impl CompletionItemKind {
    pub const METHOD: CompletionItemKind = CompletionItemKind(2);
    pub const FUNCTION: CompletionItemKind = CompletionItemKind(3);
    pub const FIELD: CompletionItemKind = CompletionItemKind(5);
    pub const VARIABLE: CompletionItemKind = CompletionItemKind(6);
    pub const CLASS: CompletionItemKind = CompletionItemKind(7);
    pub const MODULE: CompletionItemKind = CompletionItemKind(9);
    pub const ENUM: CompletionItemKind = CompletionItemKind(13);
    pub const KEYWORD: CompletionItemKind = CompletionItemKind(14);
    pub const SNIPPET: CompletionItemKind = CompletionItemKind(15);
    pub const ENUM_MEMBER: CompletionItemKind = CompletionItemKind(20);
    pub const STRUCT: CompletionItemKind = CompletionItemKind(22);
}
}

lsp_struct! {
    in
    #[derive(Debug, PartialEq, Clone)]
    pub struct CompletionParams {
        required pub text_document: TextDocumentIdentifier = "textDocument",
        required pub position: Position = "position",
        optional pub context: CompletionContext = "context",
    }
}

lsp_struct! {
    in
    #[derive(Debug, PartialEq, Clone)]
    pub struct CompletionContext {
        required pub trigger_kind: CompletionTriggerKind = "triggerKind",
        optional pub trigger_character: String = "triggerCharacter",
    }
}

/// How a completion was triggered.
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct CompletionTriggerKind(i32);
lsp_enum! {
impl CompletionTriggerKind {
    pub const TRIGGER_CHARACTER: CompletionTriggerKind = CompletionTriggerKind(2);
}
}

lsp_struct! {
    out
    /// Represents a collection of [completion items](#CompletionItem) to be presented
    /// in the editor.
    #[derive(Debug, PartialEq, Clone, Default)]
    pub struct CompletionList {
        /// The editor should request a new list as the prefix changes.
        required pub is_incomplete: bool = "isIncomplete",
        required pub items: Vec<CompletionItem> = "items",
    }
}

lsp_struct! {
    out
    #[derive(Debug, PartialEq, Default, Clone)]
    pub struct CompletionItem {
        required pub label: String = "label",
        optional pub label_details: CompletionItemLabelDetails = "labelDetails",
        optional pub kind: CompletionItemKind = "kind",
        optional pub detail: String = "detail",
        optional pub sort_text: String = "sortText",

        /// Defaults to the label when omitted.
        optional pub filter_text: String = "filterText",

        /// Defaults to the label when omitted.
        optional pub insert_text: String = "insertText",

        /// Defaults to plain text.
        optional pub insert_text_format: InsertTextFormat = "insertTextFormat",

        /// Edits must not overlap each other or the main completion edit.
        optional pub additional_text_edits: Vec<TextEdit> = "additionalTextEdits",
        optional pub command: Command = "command",
    }
}

lsp_struct! {
    out
    /// Additional details for a completion item label.
    ///
    /// @since 3.17.0
    #[derive(Debug, PartialEq, Default, Clone)]
    pub struct CompletionItemLabelDetails {
        optional pub detail: String = "detail",
        optional pub description: String = "description",
    }
}
