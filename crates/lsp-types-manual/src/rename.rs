use crate::TextDocumentPositionParams;

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct RenameParams {
    /// Text Document and Position fields
    pub text_document_position: TextDocumentPositionParams,

    /// The new name of the symbol. If the given name is not valid the
    /// request must return a [ResponseError](#ResponseError) with an
    /// appropriate message set.
    pub new_name: String,
}

impl_lsp_struct!(
    RenameParams,
    required {
        new_name: String = "newName"
    },
    optional {},
    flatten {
        text_document_position: TextDocumentPositionParams,
    }
);
