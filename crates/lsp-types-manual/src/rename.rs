use crate::{Position, TextDocumentIdentifier};

lsp_struct! {
    in
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct RenameParams {
        required pub text_document: TextDocumentIdentifier = "textDocument",
        required pub position: Position = "position",
        required pub new_name: String = "newName",}
}
