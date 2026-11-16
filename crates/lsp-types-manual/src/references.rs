use crate::TextDocumentPositionParams;

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct ReferenceParams {
    pub text_document_position: TextDocumentPositionParams,
}

impl_lsp_struct!(
    ReferenceParams,
    flatten {
        text_document_position: TextDocumentPositionParams,
    }
);
