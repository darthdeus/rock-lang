use crate::{
    DynamicRegistrationClientCapabilities, Range, StaticRegistrationOptions,
    TextDocumentPositionParams, TextDocumentRegistrationOptions, WorkDoneProgressOptions,
    WorkDoneProgressParams,
};

pub type LinkedEditingRangeClientCapabilities = DynamicRegistrationClientCapabilities;

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct LinkedEditingRangeOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    LinkedEditingRangeOptions,
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct LinkedEditingRangeRegistrationOptions {
    pub text_document_registration_options: TextDocumentRegistrationOptions,
    pub linked_editing_range_options: LinkedEditingRangeOptions,
    pub static_registration_options: StaticRegistrationOptions,
}

impl_lsp_struct!(
    LinkedEditingRangeRegistrationOptions,
    flatten {
        text_document_registration_options: TextDocumentRegistrationOptions,
        linked_editing_range_options: LinkedEditingRangeOptions,
        static_registration_options: StaticRegistrationOptions,
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum LinkedEditingRangeServerCapabilities {
    Simple(bool),
    Options(LinkedEditingRangeOptions),
    RegistrationOptions(LinkedEditingRangeRegistrationOptions),
}

impl_untagged_enum!(LinkedEditingRangeServerCapabilities {
    Simple(bool),
    Options(LinkedEditingRangeOptions),
    RegistrationOptions(LinkedEditingRangeRegistrationOptions),
});

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct LinkedEditingRangeParams {
    pub text_document_position_params: TextDocumentPositionParams,
    pub work_done_progress_params: WorkDoneProgressParams,
}

impl_lsp_struct!(
    LinkedEditingRangeParams,
    flatten {
        text_document_position_params: TextDocumentPositionParams,
        work_done_progress_params: WorkDoneProgressParams,
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct LinkedEditingRanges {
    /// A list of ranges that can be renamed together. The ranges must have
    /// identical length and contain identical text content. The ranges cannot overlap.
    pub ranges: Vec<Range>,

    /// An optional word pattern (regular expression) that describes valid contents for
    /// the given ranges. If no pattern is provided, the client configuration's word
    /// pattern will be used.
    pub word_pattern: Option<String>,
}

impl_lsp_struct!(LinkedEditingRanges,
    required { ranges: Vec<Range> = "ranges" },
    optional { word_pattern: String = "wordPattern" }
);
