use crate::{
    Command, InsertTextFormat, Range, StaticRegistrationOptions, TextDocumentPositionParams,
    TextDocumentRegistrationOptions, WorkDoneProgressOptions, WorkDoneProgressParams,
};

/// Client capabilities specific to inline completions.
///
/// @since 3.18.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlineCompletionClientCapabilities {
    pub dynamic_registration: Option<bool>,
}

impl_lsp_struct!(
    InlineCompletionClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration"
    }
);

/// Inline completion options used during static registration.
///
/// @since 3.18.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlineCompletionOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    InlineCompletionOptions,
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

/// Inline completion options used during static or dynamic registration.
///
/// @since 3.18.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlineCompletionRegistrationOptions {
    pub inline_completion_options: InlineCompletionOptions,
    pub text_document_registration_options: TextDocumentRegistrationOptions,
    pub static_registration_options: StaticRegistrationOptions,
}

impl_lsp_struct!(
    InlineCompletionRegistrationOptions,
    flatten {
        inline_completion_options: InlineCompletionOptions,
        text_document_registration_options: TextDocumentRegistrationOptions,
        static_registration_options: StaticRegistrationOptions,
    }
);

/// A parameter literal used in inline completion requests.
///
/// @since 3.18.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct InlineCompletionParams {
    pub work_done_progress_params: WorkDoneProgressParams,
    pub text_document_position: TextDocumentPositionParams,

    /// Additional information about the context in which inline completions were requested.
    pub context: InlineCompletionContext,
}

impl_lsp_struct!(
    InlineCompletionParams,
    required {
        context: InlineCompletionContext = "context"
    },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        text_document_position: TextDocumentPositionParams,
    }
);

/// Describes how an [`InlineCompletionItemProvider`] was triggered.
///
/// @since 3.18.0
#[derive(Eq, PartialEq, Clone, Copy)]
pub struct InlineCompletionTriggerKind(i32);
lsp_enum! {
impl InlineCompletionTriggerKind {
    /// Completion was triggered explicitly by a user gesture.
    /// Return multiple completion items to enable cycling through them.
    pub const Invoked: InlineCompletionTriggerKind = InlineCompletionTriggerKind(1);

    /// Completion was triggered automatically while editing.
    /// It is sufficient to return a single completion item in this case.
    pub const Automatic: InlineCompletionTriggerKind = InlineCompletionTriggerKind(2);
}
}

/// Describes the currently selected completion item.
///
/// @since 3.18.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SelectedCompletionInfo {
    pub range: Range,
    pub text: String,
}

impl_lsp_struct!(
    SelectedCompletionInfo,
    required {
        range: Range = "range",
        text: String = "text",
    },
    optional {}
);

/// Provides information about the context in which an inline completion was
/// requested.
///
/// @since 3.18.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct InlineCompletionContext {
    pub trigger_kind: InlineCompletionTriggerKind,
    pub selected_completion_info: Option<SelectedCompletionInfo>,
}

impl_lsp_struct!(
    InlineCompletionContext,
    required {
        trigger_kind: InlineCompletionTriggerKind = "triggerKind"
    },
    optional {
        selected_completion_info: SelectedCompletionInfo = "selectedCompletionInfo"
    }
);

/// InlineCompletion response can be multiple completion items, or a list of completion items
#[derive(Debug, PartialEq, Clone)]
pub enum InlineCompletionResponse {
    Array(Vec<InlineCompletionItem>),
    List(InlineCompletionList),
}

impl_untagged_enum!(InlineCompletionResponse {
    Array(Vec<InlineCompletionItem>),
    List(InlineCompletionList),
});

/// Represents a collection of [`InlineCompletionItem`] to be presented in the editor.
///
/// @since 3.18.0
#[derive(Debug, PartialEq, Clone)]
pub struct InlineCompletionList {
    pub items: Vec<InlineCompletionItem>,
}

impl_lsp_struct!(InlineCompletionList,
    required { items: Vec<InlineCompletionItem> = "items" },
    optional {}
);

/// An inline completion item represents a text snippet that is proposed inline
/// to complete text that is being typed.
///
/// @since 3.18.0
#[derive(Debug, PartialEq, Clone)]
pub struct InlineCompletionItem {
    pub insert_text: String,
    pub filter_text: Option<String>,
    pub range: Option<Range>,
    pub command: Option<Command>,
    pub insert_text_format: Option<InsertTextFormat>,
}

impl_lsp_struct!(
    InlineCompletionItem,
    required {
        insert_text: String = "insertText"
    },
    optional {
        filter_text: String = "filterText",
        range: Range = "range",
        command: Command = "command",
        insert_text_format: InsertTextFormat = "insertTextFormat",
    }
);
