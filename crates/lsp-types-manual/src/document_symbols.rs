use crate::{Location, Range, SymbolKind};

lsp_struct! {
    out
    /// Represents programming constructs like variables, classes, interfaces etc.
    /// that appear in a document. Document symbols can be hierarchical and they have two ranges:
    /// one that encloses its definition and one that points to its most interesting range,
    /// e.g. the range of an identifier.
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct DocumentSymbol {
        required pub name: String = "name",
        optional pub detail: String = "detail",
        required pub kind: SymbolKind = "kind",
        required pub range: Range = "range",
        required pub selection_range: Range = "selectionRange",
        optional pub children: Vec<DocumentSymbol> = "children",
    }
}

lsp_struct! {
    out
    /// Represents information about programming constructs like variables, classes,
    /// interfaces etc.
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct SymbolInformation {
        required pub name: String = "name",
        required pub kind: SymbolKind = "kind",
        required pub location: Location = "location",
        optional pub container_name: String = "containerName",
    }
}

lsp_struct! {
    in
    /// The parameters of a Workspace Symbol Request.
    #[derive(Debug, Eq, PartialEq, Clone, Default)]
    pub struct WorkspaceSymbolParams {
        required pub query: String = "query",
    }
}
