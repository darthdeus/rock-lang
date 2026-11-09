use crate::{
    MarkedString, MarkupContent, MarkupKind, Range, TextDocumentPositionParams,
    WorkDoneProgressOptions, WorkDoneProgressParams,
};

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct HoverClientCapabilities {
    /// Whether completion supports dynamic registration.
    pub dynamic_registration: Option<bool>,
    /// Client supports the following content formats for the content property.
    /// The order describes the preferred format of the client.
    pub content_format: Option<Vec<MarkupKind>>,
}

impl_lsp_struct!(HoverClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        content_format: Vec<MarkupKind> = "contentFormat",
    }
);

/// Hover options.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct HoverOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    HoverOptions,
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum HoverProviderCapability {
    Simple(bool),
    Options(HoverOptions),
}

impl_untagged_enum!(HoverProviderCapability {
    Simple(bool),
    Options(HoverOptions),
});

impl From<HoverOptions> for HoverProviderCapability {
    fn from(from: HoverOptions) -> Self {
        Self::Options(from)
    }
}

impl From<bool> for HoverProviderCapability {
    fn from(from: bool) -> Self {
        Self::Simple(from)
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct HoverParams {
    pub text_document_position_params: TextDocumentPositionParams,
    pub work_done_progress_params: WorkDoneProgressParams,
}

impl_lsp_struct!(
    HoverParams,
    flatten {
        text_document_position_params: TextDocumentPositionParams,
        work_done_progress_params: WorkDoneProgressParams,
    }
);

/// The result of a hover request.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct Hover {
    /// The hover's content
    pub contents: HoverContents,
    /// An optional range is a range inside a text document
    /// that is used to visualize a hover, e.g. by changing the background color.
    pub range: Option<Range>,
}

impl_lsp_struct!(
    Hover,
    required {
        contents: HoverContents = "contents"
    },
    optional {
        range: Range = "range"
    }
);

/// Hover contents could be single entry or multiple entries.
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum HoverContents {
    Scalar(MarkedString),
    Array(Vec<MarkedString>),
    Markup(MarkupContent),
}

impl_untagged_enum!(HoverContents {
    Markup(MarkupContent),
    Scalar(MarkedString),
    Array(Vec<MarkedString>),
});
