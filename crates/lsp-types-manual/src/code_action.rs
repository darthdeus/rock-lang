use crate::{Command, Diagnostic, Range, TextDocumentIdentifier, WorkspaceEdit};
use serde_json::Value;

use std::borrow::Cow;

/// Params for the CodeActionRequest
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct CodeActionParams {
    /// The document in which the command was invoked.
    pub text_document: TextDocumentIdentifier,

    /// The range for which the command was invoked.
    pub range: Range,

    /// Context carrying additional information.
    pub context: CodeActionContext,
}

impl_lsp_struct!(
    CodeActionParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        range: Range = "range",
        context: CodeActionContext = "context",
    },
    optional {}
);

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
        data: Value = "data",
    }
);

/// Contains additional diagnostic information about the context in which
/// a code action is run.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CodeActionContext {
    pub diagnostics: Vec<Diagnostic>,
}

impl_lsp_struct!(CodeActionContext,
    required { diagnostics: Vec<Diagnostic> = "diagnostics" },
    optional {}
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_serialization;

    #[test]
    fn test_code_action_response() {
        test_serialization(
            &vec![CodeAction {
                title: "title".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                ..CodeAction::default()
            }],
            r#"[{"title":"title","kind":"quickfix"}]"#,
        )
    }
}
