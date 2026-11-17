use crate::{Location, Range, SymbolKind};

lsp_struct! {
    out
    /// `range` encloses the symbol; `selection_range` identifies its name.
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct DocumentSymbol {
        required pub name: String = "name",
        required pub kind: SymbolKind = "kind",
        required pub range: Range = "range",
        required pub selection_range: Range = "selectionRange",
    }
}

lsp_struct! {
    out
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct SymbolInformation {
        required pub name: String = "name",
        required pub kind: SymbolKind = "kind",
        required pub location: Location = "location",
    }
}

lsp_struct! {
    in
    #[derive(Debug, Eq, PartialEq, Clone, Default)]
    pub struct WorkspaceSymbolParams {
        required pub query: String = "query",
    }
}
