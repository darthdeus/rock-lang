use crate::{
    Command, Documentation, MarkupKind, PartialResultParams, TagSupport,
    TextDocumentPositionParams, TextEdit, WorkDoneProgressOptions, WorkDoneProgressParams,
};

use crate::Range;
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

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CompletionItemCapability {
    /// Client supports snippets as insert text.
    ///
    /// A snippet can define tab stops and placeholders with `$1`, `$2`
    /// and `${3:foo}`. `$0` defines the final tab stop, it defaults to
    /// the end of the snippet. Placeholders with equal identifiers are linked,
    /// that is typing in one will update others too.
    pub snippet_support: Option<bool>,

    /// Client supports commit characters on a completion item.
    pub commit_characters_support: Option<bool>,

    /// Client supports the follow content formats for the documentation
    /// property. The order describes the preferred format of the client.
    pub documentation_format: Option<Vec<MarkupKind>>,

    /// Client supports the deprecated property on a completion item.
    pub deprecated_support: Option<bool>,

    /// Client supports the preselect property on a completion item.
    pub preselect_support: Option<bool>,

    /// Client supports the tag property on a completion item. Clients supporting
    /// tags have to handle unknown tags gracefully. Clients especially need to
    /// preserve unknown tags when sending a completion item back to the server in
    /// a resolve call.
    pub tag_support: Option<TagSupport<CompletionItemTag>>,

    /// Client support insert replace edit to control different behavior if a
    /// completion item is inserted in the text or should replace text.
    ///
    /// @since 3.16.0
    pub insert_replace_support: Option<bool>,

    /// Indicates which properties a client can resolve lazily on a completion
    /// item. Before version 3.16.0 only the predefined properties `documentation`
    /// and `details` could be resolved lazily.
    ///
    /// @since 3.16.0
    pub resolve_support: Option<CompletionItemCapabilityResolveSupport>,

    /// The client supports the `insertTextMode` property on
    /// a completion item to override the whitespace handling mode
    /// as defined by the client.
    ///
    /// @since 3.16.0
    pub insert_text_mode_support: Option<InsertTextModeSupport>,

    /// The client has support for completion item label
    /// details (see also `CompletionItemLabelDetails`).
    ///
    /// @since 3.17.0
    pub label_details_support: Option<bool>,
}

impl serde::Serialize for CompletionItemCapability {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("CompletionItemCapability", 9)?;
        if let Some(ref v) = self.snippet_support {
            st.serialize_field("snippetSupport", v)?;
        }
        if let Some(ref v) = self.commit_characters_support {
            st.serialize_field("commitCharactersSupport", v)?;
        }
        if let Some(ref v) = self.documentation_format {
            st.serialize_field("documentationFormat", v)?;
        }
        if let Some(ref v) = self.deprecated_support {
            st.serialize_field("deprecatedSupport", v)?;
        }
        if let Some(ref v) = self.preselect_support {
            st.serialize_field("preselectSupport", v)?;
        }
        if let Some(ref v) = self.tag_support {
            st.serialize_field("tagSupport", v)?;
        }
        if let Some(ref v) = self.insert_replace_support {
            st.serialize_field("insertReplaceSupport", v)?;
        }
        if let Some(ref v) = self.resolve_support {
            st.serialize_field("resolveSupport", v)?;
        }
        if let Some(ref v) = self.insert_text_mode_support {
            st.serialize_field("insertTextModeSupport", v)?;
        }
        if let Some(ref v) = self.label_details_support {
            st.serialize_field("labelDetailsSupport", v)?;
        }
        st.end()
    }
}

impl<'de> serde::Deserialize<'de> for CompletionItemCapability {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let snippet_support = crate::deserialize_optional(map.remove("snippetSupport"))?;
        let commit_characters_support =
            crate::deserialize_optional(map.remove("commitCharactersSupport"))?;
        let documentation_format = crate::deserialize_optional(map.remove("documentationFormat"))?;
        let deprecated_support = crate::deserialize_optional(map.remove("deprecatedSupport"))?;
        let preselect_support = crate::deserialize_optional(map.remove("preselectSupport"))?;
        let tag_support = match map.remove("tagSupport") {
            Some(value) => TagSupport::<CompletionItemTag>::deserialize_compat(
                serde::de::IntoDeserializer::into_deserializer(value),
            )
            .map_err(serde::de::Error::custom)?,
            None => None,
        };
        let insert_replace_support =
            crate::deserialize_optional(map.remove("insertReplaceSupport"))?;
        let resolve_support = crate::deserialize_optional(map.remove("resolveSupport"))?;
        let insert_text_mode_support =
            crate::deserialize_optional(map.remove("insertTextModeSupport"))?;
        let label_details_support = crate::deserialize_optional(map.remove("labelDetailsSupport"))?;
        Ok(CompletionItemCapability {
            snippet_support,
            commit_characters_support,
            documentation_format,
            deprecated_support,
            preselect_support,
            tag_support,
            insert_replace_support,
            resolve_support,
            insert_text_mode_support,
            label_details_support,
        })
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CompletionItemCapabilityResolveSupport {
    /// The properties that a client can resolve lazily.
    pub properties: Vec<String>,
}

impl_lsp_struct!(CompletionItemCapabilityResolveSupport,
    required { properties: Vec<String> = "properties" },
    optional {}
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InsertTextModeSupport {
    pub value_set: Vec<InsertTextMode>,
}

impl_lsp_struct!(InsertTextModeSupport,
    required { value_set: Vec<InsertTextMode> = "valueSet" },
    optional {}
);

/// How whitespace and indentation is handled during completion
/// item insertion.
///
/// @since 3.16.0
#[derive(Eq, PartialEq, Clone, Copy)]
pub struct InsertTextMode(i32);
lsp_enum! {
impl InsertTextMode {
    /// The insertion or replace strings is taken as it is. If the
    /// value is multi line the lines below the cursor will be
    /// inserted using the indentation defined in the string value.
    /// The client will not apply any kind of adjustments to the
    /// string.
    pub const AS_IS: InsertTextMode = InsertTextMode(1);

    /// The editor adjusts leading whitespace of new lines so that
    /// they match the indentation up to the cursor of the line for
    /// which the item is accepted.
    ///
    /// Consider a line like this: <2tabs><cursor><3tabs>foo. Accepting a
    /// multi line completion item is indented using 2 tabs all
    /// following lines inserted will be indented using 2 tabs as well.
    pub const ADJUST_INDENTATION: InsertTextMode = InsertTextMode(2);
}
}

#[derive(Eq, PartialEq, Clone)]
pub struct CompletionItemTag(i32);
lsp_enum! {
impl CompletionItemTag {
    pub const DEPRECATED: CompletionItemTag = CompletionItemTag(1);
}
}

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CompletionItemKindCapability {
    /// The completion item kind values the client supports. When this
    /// property exists the client also guarantees that it will
    /// handle values outside its set gracefully and falls back
    /// to a default value when unknown.
    ///
    /// If this property is not present the client only supports
    /// the completion items kinds from `Text` to `Reference` as defined in
    /// the initial version of the protocol.
    pub value_set: Option<Vec<CompletionItemKind>>,
}

impl_lsp_struct!(CompletionItemKindCapability,
    required {},
    optional { value_set: Vec<CompletionItemKind> = "valueSet" }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CompletionListCapability {
    /// The client supports the following itemDefaults on
    /// a completion list.
    ///
    /// The value lists the supported property names of the
    /// `CompletionList.itemDefaults` object. If omitted
    /// no properties are supported.
    ///
    /// @since 3.17.0
    pub item_defaults: Option<Vec<String>>,
}

impl_lsp_struct!(CompletionListCapability,
    required {},
    optional { item_defaults: Vec<String> = "itemDefaults" }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CompletionClientCapabilities {
    /// Whether completion supports dynamic registration.
    pub dynamic_registration: Option<bool>,

    /// The client supports the following `CompletionItem` specific
    /// capabilities.
    pub completion_item: Option<CompletionItemCapability>,

    pub completion_item_kind: Option<CompletionItemKindCapability>,

    /// The client supports to send additional context information for a
    /// `textDocument/completion` request.
    pub context_support: Option<bool>,

    /// The client's default when the completion item doesn't provide a
    /// `insertTextMode` property.
    ///
    /// @since 3.17.0
    pub insert_text_mode: Option<InsertTextMode>,

    /// The client supports the following `CompletionList` specific
    /// capabilities.
    ///
    /// @since 3.17.0
    pub completion_list: Option<CompletionListCapability>,
}

impl_lsp_struct!(
    CompletionClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        completion_item: CompletionItemCapability = "completionItem",
        completion_item_kind: CompletionItemKindCapability = "completionItemKind",
        context_support: bool = "contextSupport",
        insert_text_mode: InsertTextMode = "insertTextMode",
        completion_list: CompletionListCapability = "completionList",
    }
);

/// A special text edit to provide an insert and a replace operation.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InsertReplaceEdit {
    /// The string to be inserted.
    pub new_text: String,

    /// The range if the insert is requested
    pub insert: Range,

    /// The range if the replace is requested.
    pub replace: Range,
}

impl_lsp_struct!(
    InsertReplaceEdit,
    required {
        new_text: String = "newText",
        insert: Range = "insert",
        replace: Range = "replace",
    },
    optional {}
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum CompletionTextEdit {
    Edit(TextEdit),
    InsertAndReplace(InsertReplaceEdit),
}

impl_untagged_enum!(CompletionTextEdit {
    Edit(TextEdit),
    InsertAndReplace(InsertReplaceEdit),
});

impl From<TextEdit> for CompletionTextEdit {
    fn from(edit: TextEdit) -> Self {
        CompletionTextEdit::Edit(edit)
    }
}

impl From<InsertReplaceEdit> for CompletionTextEdit {
    fn from(edit: InsertReplaceEdit) -> Self {
        CompletionTextEdit::InsertAndReplace(edit)
    }
}

/// Completion options.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CompletionOptions {
    /// The server provides support to resolve additional information for a completion item.
    pub resolve_provider: Option<bool>,

    /// Characters that trigger completion automatically.
    pub trigger_characters: Option<Vec<String>>,

    /// Characters that commit a completion.
    ///
    /// @since 3.2.0
    pub all_commit_characters: Option<Vec<String>>,

    pub work_done_progress_options: WorkDoneProgressOptions,

    /// The server supports the following `CompletionItem` specific capabilities.
    ///
    /// @since 3.17.0
    pub completion_item: Option<CompletionOptionsCompletionItem>,
}

impl_lsp_struct!(CompletionOptions,
    optional {
        resolve_provider: bool = "resolveProvider",
        trigger_characters: Vec<String> = "triggerCharacters",
        all_commit_characters: Vec<String> = "allCommitCharacters",
        completion_item: CompletionOptionsCompletionItem = "completionItem",
    },
    flatten { work_done_progress_options: WorkDoneProgressOptions }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CompletionOptionsCompletionItem {
    /// The server has support for completion item label
    /// details (see also `CompletionItemLabelDetails`) when receiving
    /// a completion item in a resolve call.
    ///
    /// @since 3.17.0
    pub label_details_support: Option<bool>,
}

impl_lsp_struct!(
    CompletionOptionsCompletionItem,
    required {},
    optional {
        label_details_support: bool = "labelDetailsSupport"
    }
);

#[derive(Debug, PartialEq, Clone)]
pub enum CompletionResponse {
    Array(Vec<CompletionItem>),
    List(CompletionList),
}

impl_untagged_enum!(CompletionResponse {
    Array(Vec<CompletionItem>),
    List(CompletionList),
});

impl From<Vec<CompletionItem>> for CompletionResponse {
    fn from(items: Vec<CompletionItem>) -> Self {
        CompletionResponse::Array(items)
    }
}

impl From<CompletionList> for CompletionResponse {
    fn from(list: CompletionList) -> Self {
        CompletionResponse::List(list)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct CompletionParams {
    pub text_document_position: TextDocumentPositionParams,
    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
    pub context: Option<CompletionContext>,
}

impl_lsp_struct!(
    CompletionParams,
    optional {
        context: CompletionContext = "context"
    },
    flatten {
        text_document_position: TextDocumentPositionParams,
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
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

    /// Select this item when showing.
    pub preselect: Option<bool>,

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

    /// How whitespace and indentation is handled during completion
    /// item insertion. If not provided the client's default value depends on
    /// the `textDocument.completion.insertTextMode` client capability.
    ///
    /// @since 3.16.0
    /// @since 3.17.0 - support for `textDocument.completion.insertTextMode`
    pub insert_text_mode: Option<InsertTextMode>,

    /// An edit which is applied to a document when selecting
    /// this completion. When an edit is provided the value of
    /// insertText is ignored.
    ///
    /// @since 3.16.0 additional type `InsertReplaceEdit`
    pub text_edit: Option<CompletionTextEdit>,

    /// An optional array of additional text edits that are applied when
    /// selecting this completion. Edits must not overlap with the main edit
    /// nor with themselves.
    pub additional_text_edits: Option<Vec<TextEdit>>,

    /// An optional command that is executed *after* inserting this completion.
    pub command: Option<Command>,

    /// An optional set of characters that when pressed while this completion is
    /// active will accept it first and then type that character.
    pub commit_characters: Option<Vec<String>>,

    /// An data entry field that is preserved on a completion item between
    /// a completion and a completion resolve request.
    pub data: Option<Value>,

    /// Tags for this completion item.
    pub tags: Option<Vec<CompletionItemTag>>,
}

impl_lsp_struct!(CompletionItem,
    required { label: String = "label" },
    optional {
        label_details: CompletionItemLabelDetails = "labelDetails",
        kind: CompletionItemKind = "kind",
        detail: String = "detail",
        documentation: Documentation = "documentation",
        deprecated: bool = "deprecated",
        preselect: bool = "preselect",
        sort_text: String = "sortText",
        filter_text: String = "filterText",
        insert_text: String = "insertText",
        insert_text_format: InsertTextFormat = "insertTextFormat",
        insert_text_mode: InsertTextMode = "insertTextMode",
        text_edit: CompletionTextEdit = "textEdit",
        additional_text_edits: Vec<TextEdit> = "additionalTextEdits",
        command: Command = "command",
        commit_characters: Vec<String> = "commitCharacters",
        data: Value = "data",
        tags: Vec<CompletionItemTag> = "tags",
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
    use crate::tests::test_deserialization;

    #[test]
    fn test_tag_support_deserialization() {
        let empty = CompletionItemCapability {
            tag_support: None,
            ..CompletionItemCapability::default()
        };

        test_deserialization(r#"{}"#, &empty);
        test_deserialization(r#"{"tagSupport": false}"#, &empty);

        let t = CompletionItemCapability {
            tag_support: Some(TagSupport { value_set: vec![] }),
            ..CompletionItemCapability::default()
        };
        test_deserialization(r#"{"tagSupport": true}"#, &t);

        let t = CompletionItemCapability {
            tag_support: Some(TagSupport {
                value_set: vec![CompletionItemTag::DEPRECATED],
            }),
            ..CompletionItemCapability::default()
        };
        test_deserialization(r#"{"tagSupport": {"valueSet": [1]}}"#, &t);
    }

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
