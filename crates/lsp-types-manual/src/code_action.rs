use crate::{Diagnostic, Range, TextDocumentIdentifier, WorkspaceEdit};

lsp_struct! {
    in
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct CodeActionParams {
        required pub text_document: TextDocumentIdentifier = "textDocument",
        required pub range: Range = "range",
        required pub context: CodeActionContext = "context",
    }
}

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

    pub const REFACTOR_INLINE: CodeActionKind = CodeActionKind::new("refactor.inline");

    pub const fn new(tag: &'static str) -> Self {
        CodeActionKind(tag)
    }

    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

lsp_struct! {
    out
    #[derive(Debug, PartialEq, Clone, Default)]
    pub struct CodeAction {
        required pub title: String = "title",
        optional pub kind: CodeActionKind = "kind",
        optional pub diagnostics: Vec<Diagnostic> = "diagnostics",
        optional pub edit: WorkspaceEdit = "edit",
    }
}

lsp_struct! {
    in
    /// Diagnostics relevant to the requested code action.
    #[derive(Debug, Eq, PartialEq, Clone, Default)]
    pub struct CodeActionContext {
        required pub diagnostics: Vec<Diagnostic> = "diagnostics",
    }
}

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
