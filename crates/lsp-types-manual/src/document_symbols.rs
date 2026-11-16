use crate::{Location, Range, SymbolKind, SymbolTag, TextDocumentIdentifier};

#[derive(Debug, PartialEq, Clone)]
pub enum DocumentSymbolResponse {
    Nested(Vec<DocumentSymbol>),
}

impl_untagged_enum!(DocumentSymbolResponse {
    Nested(Vec<DocumentSymbol>),
});

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentSymbolParams {
    /// The text document.
    pub text_document: TextDocumentIdentifier,
}

impl_lsp_struct!(
    DocumentSymbolParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument"
    },
    optional {}
);

/// Represents programming constructs like variables, classes, interfaces etc.
/// that appear in a document. Document symbols can be hierarchical and they have two ranges:
/// one that encloses its definition and one that points to its most interesting range,
/// e.g. the range of an identifier.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentSymbol {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    pub tags: Option<Vec<SymbolTag>>,
    #[deprecated(note = "Use tags instead")]
    pub deprecated: Option<bool>,
    pub range: Range,
    pub selection_range: Range,
    pub children: Option<Vec<DocumentSymbol>>,
}

impl_lsp_struct!(DocumentSymbol,
    required {
        name: String = "name",
        kind: SymbolKind = "kind",
        range: Range = "range",
        selection_range: Range = "selectionRange",
    },
    optional {
        detail: String = "detail",
        tags: Vec<SymbolTag> = "tags",
        deprecated: bool = "deprecated",
        children: Vec<DocumentSymbol> = "children",
    }
);

/// Represents information about programming constructs like variables, classes,
/// interfaces etc.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SymbolInformation {
    pub name: String,
    pub kind: SymbolKind,
    pub tags: Option<Vec<SymbolTag>>,
    #[deprecated(note = "Use tags instead")]
    pub deprecated: Option<bool>,
    pub location: Location,
    pub container_name: Option<String>,
}

impl_lsp_struct!(SymbolInformation,
    required {
        name: String = "name",
        kind: SymbolKind = "kind",
        location: Location = "location",
    },
    optional {
        tags: Vec<SymbolTag> = "tags",
        deprecated: bool = "deprecated",
        container_name: String = "containerName",
    }
);
