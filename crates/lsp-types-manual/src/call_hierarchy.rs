use serde_json::Value;

use crate::{
    DynamicRegistrationClientCapabilities, PartialResultParams, Range, SymbolKind, SymbolTag,
    TextDocumentPositionParams, Uri, WorkDoneProgressOptions, WorkDoneProgressParams,
};

pub type CallHierarchyClientCapabilities = DynamicRegistrationClientCapabilities;

#[derive(Debug, Eq, PartialEq, Clone, Default, Copy)]
pub struct CallHierarchyOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(CallHierarchyOptions,
    flatten { work_done_progress_options: WorkDoneProgressOptions }
);

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum CallHierarchyServerCapability {
    Simple(bool),
    Options(CallHierarchyOptions),
}

impl_untagged_enum!(CallHierarchyServerCapability {
    Simple(bool),
    Options(CallHierarchyOptions),
});

impl From<CallHierarchyOptions> for CallHierarchyServerCapability {
    fn from(from: CallHierarchyOptions) -> Self {
        Self::Options(from)
    }
}

impl From<bool> for CallHierarchyServerCapability {
    fn from(from: bool) -> Self {
        Self::Simple(from)
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct CallHierarchyPrepareParams {
    pub text_document_position_params: TextDocumentPositionParams,
    pub work_done_progress_params: WorkDoneProgressParams,
}

impl_lsp_struct!(CallHierarchyPrepareParams,
    flatten {
        text_document_position_params: TextDocumentPositionParams,
        work_done_progress_params: WorkDoneProgressParams,
    }
);

#[derive(Debug, PartialEq, Clone)]
pub struct CallHierarchyItem {
    pub name: String,
    pub kind: SymbolKind,
    pub tags: Option<Vec<SymbolTag>>,
    pub detail: Option<String>,
    pub uri: Uri,
    pub range: Range,
    pub selection_range: Range,
    pub data: Option<Value>,
}

impl_lsp_struct!(CallHierarchyItem,
    required {
        name: String = "name",
        kind: SymbolKind = "kind",
        uri: Uri = "uri",
        range: Range = "range",
        selection_range: Range = "selectionRange",
    },
    optional {
        tags: Vec<SymbolTag> = "tags",
        detail: String = "detail",
        data: Value = "data",
    }
);

#[derive(Debug, PartialEq, Clone)]
pub struct CallHierarchyIncomingCallsParams {
    pub item: CallHierarchyItem,
    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(CallHierarchyIncomingCallsParams,
    required { item: CallHierarchyItem = "item" },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

/// Represents an incoming call, e.g. a caller of a method or constructor.
#[derive(Debug, PartialEq, Clone)]
pub struct CallHierarchyIncomingCall {
    pub from: CallHierarchyItem,
    pub from_ranges: Vec<Range>,
}

impl_lsp_struct!(CallHierarchyIncomingCall,
    required {
        from: CallHierarchyItem = "from",
        from_ranges: Vec<Range> = "fromRanges",
    },
    optional {}
);

#[derive(Debug, PartialEq, Clone)]
pub struct CallHierarchyOutgoingCallsParams {
    pub item: CallHierarchyItem,
    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(CallHierarchyOutgoingCallsParams,
    required { item: CallHierarchyItem = "item" },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

/// Represents an outgoing call, e.g. calling a getter from a method or a method from a constructor etc.
#[derive(Debug, PartialEq, Clone)]
pub struct CallHierarchyOutgoingCall {
    pub to: CallHierarchyItem,
    pub from_ranges: Vec<Range>,
}

impl_lsp_struct!(CallHierarchyOutgoingCall,
    required {
        to: CallHierarchyItem = "to",
        from_ranges: Vec<Range> = "fromRanges",
    },
    optional {}
);
