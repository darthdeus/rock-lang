use crate::{Diagnostic, Range, TextDocumentIdentifier, WorkspaceEdit};

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
    in CodeActionParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        range: Range = "range",
        context: CodeActionContext = "context",
    },
    optional {}
);

#[derive(Debug, Eq, PartialEq, Hash, PartialOrd, Clone)]
pub struct CodeActionKind(&'static str);

impl serde::Serialize for CodeActionKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
    }
}

impl CodeActionKind {
    /// Base kind for quickfix actions: 'quickfix'
    pub const QUICKFIX: CodeActionKind = CodeActionKind::new("quickfix");

    /// Base kind for refactoring inline actions: 'refactor.inline'
    ///
    /// Example inline actions:
    ///
    /// - Inline function
    /// - Inline variable
    /// - Inline constant
    /// - ...
    pub const REFACTOR_INLINE: CodeActionKind = CodeActionKind::new("refactor.inline");

    pub const fn new(tag: &'static str) -> Self {
        CodeActionKind(tag)
    }

    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

#[derive(Debug, PartialEq, Clone, Default)]
pub struct CodeAction {
    pub title: String,
    pub kind: Option<CodeActionKind>,
    pub diagnostics: Option<Vec<Diagnostic>>,
    pub edit: Option<WorkspaceEdit>,
}

impl_lsp_struct!(out CodeAction,
    required { title: String = "title" },
    optional {
        kind: CodeActionKind = "kind",
        diagnostics: Vec<Diagnostic> = "diagnostics",
        edit: WorkspaceEdit = "edit",
    }
);

/// Contains additional diagnostic information about the context in which
/// a code action is run.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CodeActionContext {
    pub diagnostics: Vec<Diagnostic>,
}

impl_lsp_struct!(in CodeActionContext,
    required { diagnostics: Vec<Diagnostic> = "diagnostics" },
    optional {}
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::assert_json;

    #[test]
    fn test_code_action_response() {
        assert_json(
            &vec![CodeAction {
                title: "title".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                ..CodeAction::default()
            }],
            r#"[{"title":"title","kind":"quickfix"}]"#,
        )
    }
}
