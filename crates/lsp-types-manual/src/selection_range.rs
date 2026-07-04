use crate::{
    PartialResultParams, Position, Range, StaticTextDocumentRegistrationOptions,
    TextDocumentIdentifier, WorkDoneProgressOptions, WorkDoneProgressParams,
};

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SelectionRangeClientCapabilities {
    /// Whether implementation supports dynamic registration for selection range
    /// providers. If this is set to `true` the client supports the new
    /// `SelectionRangeRegistrationOptions` return value for the corresponding
    /// server capability as well.
    pub dynamic_registration: Option<bool>,
}

impl_lsp_struct!(SelectionRangeClientCapabilities,
    required {},
    optional { dynamic_registration: bool = "dynamicRegistration" }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SelectionRangeOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(SelectionRangeOptions,
    flatten { work_done_progress_options: WorkDoneProgressOptions }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SelectionRangeRegistrationOptions {
    pub selection_range_options: SelectionRangeOptions,
    pub registration_options: StaticTextDocumentRegistrationOptions,
}

impl_lsp_struct!(SelectionRangeRegistrationOptions,
    flatten {
        selection_range_options: SelectionRangeOptions,
        registration_options: StaticTextDocumentRegistrationOptions,
    }
);

#[derive(Debug, PartialEq, Clone)]
pub enum SelectionRangeProviderCapability {
    Simple(bool),
    Options(SelectionRangeOptions),
    RegistrationOptions(SelectionRangeRegistrationOptions),
}

impl_untagged_enum!(SelectionRangeProviderCapability {
    Simple(bool),
    Options(SelectionRangeOptions),
    RegistrationOptions(SelectionRangeRegistrationOptions),
});

impl From<SelectionRangeRegistrationOptions> for SelectionRangeProviderCapability {
    fn from(from: SelectionRangeRegistrationOptions) -> Self {
        Self::RegistrationOptions(from)
    }
}

impl From<SelectionRangeOptions> for SelectionRangeProviderCapability {
    fn from(from: SelectionRangeOptions) -> Self {
        Self::Options(from)
    }
}

impl From<bool> for SelectionRangeProviderCapability {
    fn from(from: bool) -> Self {
        Self::Simple(from)
    }
}

/// A parameter literal used in selection range requests.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SelectionRangeParams {
    /// The text document.
    pub text_document: TextDocumentIdentifier,

    /// The positions inside the text document.
    pub positions: Vec<Position>,

    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(SelectionRangeParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        positions: Vec<Position> = "positions",
    },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

/// Represents a selection range.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SelectionRange {
    /// Range of the selection.
    pub range: Range,

    /// The parent selection range containing this range.
    pub parent: Option<Box<SelectionRange>>,
}

impl_lsp_struct!(SelectionRange,
    required { range: Range = "range" },
    optional { parent: Box<SelectionRange> = "parent" }
);
