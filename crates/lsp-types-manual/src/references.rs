use crate::{
    DynamicRegistrationClientCapabilities, PartialResultParams, TextDocumentPositionParams,
    WorkDoneProgressParams,
};

pub type ReferenceClientCapabilities = DynamicRegistrationClientCapabilities;

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct ReferenceContext {
    /// Include the declaration of the current symbol.
    pub include_declaration: bool,
}

impl_lsp_struct!(ReferenceContext,
    required { include_declaration: bool = "includeDeclaration" },
    optional {}
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct ReferenceParams {
    pub text_document_position: TextDocumentPositionParams,
    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
    pub context: ReferenceContext,
}

impl_lsp_struct!(ReferenceParams,
    required { context: ReferenceContext = "context" },
    optional {},
    flatten {
        text_document_position: TextDocumentPositionParams,
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);
