use crate::{
    DynamicRegistrationClientCapabilities, LSPAny, PartialResultParams, Range,
    StaticRegistrationOptions, SymbolKind, SymbolTag, TextDocumentPositionParams,
    TextDocumentRegistrationOptions, Uri, WorkDoneProgressOptions, WorkDoneProgressParams,
};

pub type TypeHierarchyClientCapabilities = DynamicRegistrationClientCapabilities;

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct TypeHierarchyOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(TypeHierarchyOptions,
    flatten { work_done_progress_options: WorkDoneProgressOptions }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct TypeHierarchyRegistrationOptions {
    pub text_document_registration_options: TextDocumentRegistrationOptions,
    pub type_hierarchy_options: TypeHierarchyOptions,
    pub static_registration_options: StaticRegistrationOptions,
}

impl_lsp_struct!(TypeHierarchyRegistrationOptions,
    flatten {
        text_document_registration_options: TextDocumentRegistrationOptions,
        type_hierarchy_options: TypeHierarchyOptions,
        static_registration_options: StaticRegistrationOptions,
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TypeHierarchyPrepareParams {
    pub text_document_position_params: TextDocumentPositionParams,
    pub work_done_progress_params: WorkDoneProgressParams,
}

impl_lsp_struct!(TypeHierarchyPrepareParams,
    flatten {
        text_document_position_params: TextDocumentPositionParams,
        work_done_progress_params: WorkDoneProgressParams,
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TypeHierarchySupertypesParams {
    pub item: TypeHierarchyItem,
    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(TypeHierarchySupertypesParams,
    required { item: TypeHierarchyItem = "item" },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TypeHierarchySubtypesParams {
    pub item: TypeHierarchyItem,
    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(TypeHierarchySubtypesParams,
    required { item: TypeHierarchyItem = "item" },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TypeHierarchyItem {
    /// The name of this item.
    pub name: String,

    /// The kind of this item.
    pub kind: SymbolKind,

    /// Tags for this item.
    pub tags: Option<SymbolTag>,

    /// More detail for this item, e.g. the signature of a function.
    pub detail: Option<String>,

    /// The resource identifier of this item.
    pub uri: Uri,

    /// The range enclosing this symbol not including leading/trailing whitespace
    /// but everything else, e.g. comments and code.
    pub range: Range,

    /// The range that should be selected and revealed when this symbol is being
    /// picked, e.g. the name of a function. Must be contained by the
    /// [`range`](#TypeHierarchyItem.range).
    pub selection_range: Range,

    /// A data entry field that is preserved between a type hierarchy prepare and
    /// supertypes or subtypes requests.
    pub data: Option<LSPAny>,
}

impl_lsp_struct!(TypeHierarchyItem,
    required {
        name: String = "name",
        kind: SymbolKind = "kind",
        uri: Uri = "uri",
        range: Range = "range",
        selection_range: Range = "selectionRange",
    },
    optional {
        tags: SymbolTag = "tags",
        detail: String = "detail",
        data: LSPAny = "data",
    }
);
