use crate::SymbolInformation;

/// The parameters of a Workspace Symbol Request.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceSymbolParams {
    /// A non-empty query string
    pub query: String,
}

impl_lsp_struct!(
    WorkspaceSymbolParams,
    required {
        query: String = "query"
    },
    optional {}
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum WorkspaceSymbolResponse {
    Flat(Vec<SymbolInformation>),
}

impl_untagged_enum!(WorkspaceSymbolResponse {
    Flat(Vec<SymbolInformation>),
});
