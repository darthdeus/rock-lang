use crate::{
    PartialResultParams, StaticTextDocumentColorProviderOptions, TextDocumentIdentifier,
    WorkDoneProgressParams,
};
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct FoldingRangeParams {
    /// The text document.
    pub text_document: TextDocumentIdentifier,

    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(
    FoldingRangeParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument"
    },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum FoldingRangeProviderCapability {
    Simple(bool),
    FoldingProvider(FoldingProviderOptions),
    Options(StaticTextDocumentColorProviderOptions),
}

impl_untagged_enum!(FoldingRangeProviderCapability {
    Simple(bool),
    FoldingProvider(FoldingProviderOptions),
    Options(StaticTextDocumentColorProviderOptions),
});

impl From<StaticTextDocumentColorProviderOptions> for FoldingRangeProviderCapability {
    fn from(from: StaticTextDocumentColorProviderOptions) -> Self {
        Self::Options(from)
    }
}

impl From<FoldingProviderOptions> for FoldingRangeProviderCapability {
    fn from(from: FoldingProviderOptions) -> Self {
        Self::FoldingProvider(from)
    }
}

impl From<bool> for FoldingRangeProviderCapability {
    fn from(from: bool) -> Self {
        Self::Simple(from)
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FoldingProviderOptions {}

impl_lsp_struct!(FoldingProviderOptions, required {}, optional {});

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FoldingRangeKindCapability {
    pub value_set: Option<Vec<FoldingRangeKind>>,
}

impl_lsp_struct!(FoldingRangeKindCapability,
    required {},
    optional { value_set: Vec<FoldingRangeKind> = "valueSet" }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FoldingRangeCapability {
    pub collapsed_text: Option<bool>,
}

impl_lsp_struct!(
    FoldingRangeCapability,
    required {},
    optional {
        collapsed_text: bool = "collapsedText"
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FoldingRangeClientCapabilities {
    pub dynamic_registration: Option<bool>,
    pub range_limit: Option<u32>,
    pub line_folding_only: Option<bool>,
    pub folding_range_kind: Option<FoldingRangeKindCapability>,
    pub folding_range: Option<FoldingRangeCapability>,
}

impl_lsp_struct!(
    FoldingRangeClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        range_limit: u32 = "rangeLimit",
        line_folding_only: bool = "lineFoldingOnly",
        folding_range_kind: FoldingRangeKindCapability = "foldingRangeKind",
        folding_range: FoldingRangeCapability = "foldingRange",
    }
);

/// Enum of known range kinds
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum FoldingRangeKind {
    /// Folding range for a comment
    Comment,
    /// Folding range for a imports or includes
    Imports,
    /// Folding range for a region (e.g. `#region`)
    Region,
}

impl_str_enum!(FoldingRangeKind { Comment = "comment", Imports = "imports", Region = "region" });

/// Represents a folding range.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FoldingRange {
    pub start_line: u32,
    pub start_character: Option<u32>,
    pub end_line: u32,
    pub end_character: Option<u32>,
    pub kind: Option<FoldingRangeKind>,
    pub collapsed_text: Option<String>,
}

impl_lsp_struct!(
    FoldingRange,
    required {
        start_line: u32 = "startLine",
        end_line: u32 = "endLine",
    },
    optional {
        start_character: u32 = "startCharacter",
        end_character: u32 = "endCharacter",
        kind: FoldingRangeKind = "kind",
        collapsed_text: String = "collapsedText",
    }
);
