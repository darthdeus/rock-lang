use crate::{Position, TextDocumentIdentifier};

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct RenameParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
    pub new_name: String,
}

impl_lsp_struct!(
    in RenameParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        position: Position = "position",
        new_name: String = "newName",
    },
    optional {}
);
