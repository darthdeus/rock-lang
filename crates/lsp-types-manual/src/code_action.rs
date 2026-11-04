use crate::{
    Command, Diagnostic, PartialResultParams, Range, TextDocumentIdentifier,
    WorkDoneProgressOptions, WorkDoneProgressParams, WorkspaceEdit,
};
use serde_json::Value;

use std::borrow::Cow;
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum CodeActionProviderCapability {
    Simple(bool),
    Options(CodeActionOptions),
}

impl_untagged_enum!(CodeActionProviderCapability {
    Simple(bool),
    Options(CodeActionOptions),
});

impl From<CodeActionOptions> for CodeActionProviderCapability {
    fn from(from: CodeActionOptions) -> Self {
        Self::Options(from)
    }
}

impl From<bool> for CodeActionProviderCapability {
    fn from(from: bool) -> Self {
        Self::Simple(from)
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CodeActionClientCapabilities {
    pub dynamic_registration: Option<bool>,
    pub code_action_literal_support: Option<CodeActionLiteralSupport>,
    pub is_preferred_support: Option<bool>,
    pub disabled_support: Option<bool>,
    pub data_support: Option<bool>,
    pub resolve_support: Option<CodeActionCapabilityResolveSupport>,
    pub honors_change_annotations: Option<bool>,
}

impl_lsp_struct!(
    CodeActionClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        code_action_literal_support: CodeActionLiteralSupport = "codeActionLiteralSupport",
        is_preferred_support: bool = "isPreferredSupport",
        disabled_support: bool = "disabledSupport",
        data_support: bool = "dataSupport",
        resolve_support: CodeActionCapabilityResolveSupport = "resolveSupport",
        honors_change_annotations: bool = "honorsChangeAnnotations",
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CodeActionCapabilityResolveSupport {
    pub properties: Vec<String>,
}

impl_lsp_struct!(CodeActionCapabilityResolveSupport,
    required { properties: Vec<String> = "properties" },
    optional {}
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CodeActionLiteralSupport {
    pub code_action_kind: CodeActionKindLiteralSupport,
}

impl_lsp_struct!(
    CodeActionLiteralSupport,
    required {
        code_action_kind: CodeActionKindLiteralSupport = "codeActionKind"
    },
    optional {}
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CodeActionKindLiteralSupport {
    pub value_set: Vec<String>,
}

impl_lsp_struct!(CodeActionKindLiteralSupport,
    required { value_set: Vec<String> = "valueSet" },
    optional {}
);

/// Params for the CodeActionRequest
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct CodeActionParams {
    /// The document in which the command was invoked.
    pub text_document: TextDocumentIdentifier,

    /// The range for which the command was invoked.
    pub range: Range,

    /// Context carrying additional information.
    pub context: CodeActionContext,

    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(
    CodeActionParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        range: Range = "range",
        context: CodeActionContext = "context",
    },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

/// response for CodeActionRequest
pub type CodeActionResponse = Vec<CodeActionOrCommand>;

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum CodeActionOrCommand {
    Command(Command),
    CodeAction(CodeAction),
}

impl_untagged_enum!(CodeActionOrCommand {
    Command(Command),
    CodeAction(CodeAction),
});

impl From<Command> for CodeActionOrCommand {
    fn from(command: Command) -> Self {
        CodeActionOrCommand::Command(command)
    }
}

impl From<CodeAction> for CodeActionOrCommand {
    fn from(action: CodeAction) -> Self {
        CodeActionOrCommand::CodeAction(action)
    }
}

#[derive(Debug, Eq, PartialEq, Hash, PartialOrd, Clone)]
pub struct CodeActionKind(Cow<'static, str>);

impl serde::Serialize for CodeActionKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for CodeActionKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(CodeActionKind(Cow::Owned(String::deserialize(d)?)))
    }
}

impl CodeActionKind {
    /// Empty kind.
    pub const EMPTY: CodeActionKind = CodeActionKind::new("");

    /// Base kind for quickfix actions: 'quickfix'
    pub const QUICKFIX: CodeActionKind = CodeActionKind::new("quickfix");

    /// Base kind for refactoring actions: 'refactor'
    pub const REFACTOR: CodeActionKind = CodeActionKind::new("refactor");

    /// Base kind for refactoring extraction actions: 'refactor.extract'
    ///
    /// Example extract actions:
    ///
    /// - Extract method
    /// - Extract function
    /// - Extract variable
    /// - Extract interface from class
    /// - ...
    pub const REFACTOR_EXTRACT: CodeActionKind = CodeActionKind::new("refactor.extract");

    /// Base kind for refactoring inline actions: 'refactor.inline'
    ///
    /// Example inline actions:
    ///
    /// - Inline function
    /// - Inline variable
    /// - Inline constant
    /// - ...
    pub const REFACTOR_INLINE: CodeActionKind = CodeActionKind::new("refactor.inline");

    /// Base kind for refactoring rewrite actions: 'refactor.rewrite'
    ///
    /// Example rewrite actions:
    ///
    /// - Convert JavaScript function to class
    /// - Add or remove parameter
    /// - Encapsulate field
    /// - Make method static
    /// - Move method to base class
    /// - ...
    pub const REFACTOR_REWRITE: CodeActionKind = CodeActionKind::new("refactor.rewrite");

    /// Base kind for source actions: `source`
    ///
    /// Source code actions apply to the entire file.
    pub const SOURCE: CodeActionKind = CodeActionKind::new("source");

    /// Base kind for an organize imports source action: `source.organizeImports`
    pub const SOURCE_ORGANIZE_IMPORTS: CodeActionKind =
        CodeActionKind::new("source.organizeImports");

    /// Base kind for a 'fix all' source action: `source.fixAll`.
    ///
    /// 'Fix all' actions automatically fix errors that have a clear fix that
    /// do not require user input. They should not suppress errors or perform
    /// unsafe fixes such as generating new types or classes.
    ///
    /// @since 3.17.0
    pub const SOURCE_FIX_ALL: CodeActionKind = CodeActionKind::new("source.fixAll");

    pub const fn new(tag: &'static str) -> Self {
        CodeActionKind(Cow::Borrowed(tag))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for CodeActionKind {
    fn from(from: String) -> Self {
        CodeActionKind(Cow::from(from))
    }
}

impl From<&'static str> for CodeActionKind {
    fn from(from: &'static str) -> Self {
        CodeActionKind::new(from)
    }
}

#[derive(Debug, PartialEq, Clone, Default)]
pub struct CodeAction {
    pub title: String,
    pub kind: Option<CodeActionKind>,
    pub diagnostics: Option<Vec<Diagnostic>>,
    pub edit: Option<WorkspaceEdit>,
    pub command: Option<Command>,
    pub is_preferred: Option<bool>,
    pub disabled: Option<CodeActionDisabled>,
    pub data: Option<Value>,
}

impl_lsp_struct!(CodeAction,
    required { title: String = "title" },
    optional {
        kind: CodeActionKind = "kind",
        diagnostics: Vec<Diagnostic> = "diagnostics",
        edit: WorkspaceEdit = "edit",
        command: Command = "command",
        is_preferred: bool = "isPreferred",
        disabled: CodeActionDisabled = "disabled",
        data: Value = "data",
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CodeActionDisabled {
    pub reason: String,
}

impl_lsp_struct!(
    CodeActionDisabled,
    required {
        reason: String = "reason"
    },
    optional {}
);

/// The reason why code actions were requested.
///
/// @since 3.17.0
#[derive(Eq, PartialEq, Clone, Copy)]
pub struct CodeActionTriggerKind(i32);
lsp_enum! {
impl CodeActionTriggerKind {
    /// Code actions were explicitly requested by the user or by an extension.
    pub const INVOKED: CodeActionTriggerKind = CodeActionTriggerKind(1);

    /// Code actions were requested automatically.
    ///
    /// This typically happens when current selection in a file changes, but can
    /// also be triggered when file content changes.
    pub const AUTOMATIC: CodeActionTriggerKind = CodeActionTriggerKind(2);
}
}

/// Contains additional diagnostic information about the context in which
/// a code action is run.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CodeActionContext {
    pub diagnostics: Vec<Diagnostic>,
    pub only: Option<Vec<CodeActionKind>>,
    pub trigger_kind: Option<CodeActionTriggerKind>,
}

impl_lsp_struct!(CodeActionContext,
    required { diagnostics: Vec<Diagnostic> = "diagnostics" },
    optional {
        only: Vec<CodeActionKind> = "only",
        trigger_kind: CodeActionTriggerKind = "triggerKind",
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CodeActionOptions {
    /// CodeActionKinds that this server may return.
    ///
    /// The list of kinds may be generic, such as `CodeActionKind.Refactor`, or the server
    /// may list out every specific kind they provide.
    pub code_action_kinds: Option<Vec<CodeActionKind>>,

    pub work_done_progress_options: WorkDoneProgressOptions,

    /// The server provides support to resolve additional
    /// information for a code action.
    ///
    /// @since 3.16.0
    pub resolve_provider: Option<bool>,
}

impl_lsp_struct!(CodeActionOptions,
    optional {
        code_action_kinds: Vec<CodeActionKind> = "codeActionKinds",
        resolve_provider: bool = "resolveProvider",
    },
    flatten { work_done_progress_options: WorkDoneProgressOptions }
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_serialization;

    #[test]
    fn test_code_action_response() {
        test_serialization(
            &vec![
                CodeActionOrCommand::Command(Command {
                    title: "title".to_string(),
                    command: "command".to_string(),
                    arguments: None,
                }),
                CodeActionOrCommand::CodeAction(CodeAction {
                    title: "title".to_string(),
                    kind: Some(CodeActionKind::QUICKFIX),
                    command: None,
                    diagnostics: None,
                    edit: None,
                    is_preferred: None,
                    ..CodeAction::default()
                }),
            ],
            r#"[{"title":"title","command":"command"},{"title":"title","kind":"quickfix"}]"#,
        )
    }
}
