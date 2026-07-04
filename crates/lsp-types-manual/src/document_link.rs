use crate::{
    PartialResultParams, Range, TextDocumentIdentifier, Uri, WorkDoneProgressOptions,
    WorkDoneProgressParams,
};
use serde_json::Value;

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct DocumentLinkClientCapabilities {
    /// Whether document link supports dynamic registration.
    pub dynamic_registration: Option<bool>,

    /// Whether the client support the `tooltip` property on `DocumentLink`.
    pub tooltip_support: Option<bool>,
}

impl_lsp_struct!(DocumentLinkClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        tooltip_support: bool = "tooltipSupport",
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentLinkOptions {
    /// Document links have a resolve provider as well.
    pub resolve_provider: Option<bool>,

    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(DocumentLinkOptions,
    optional { resolve_provider: bool = "resolveProvider" },
    flatten { work_done_progress_options: WorkDoneProgressOptions }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentLinkParams {
    /// The document to provide document links for.
    pub text_document: TextDocumentIdentifier,

    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(DocumentLinkParams,
    required { text_document: TextDocumentIdentifier = "textDocument" },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

/// A document link is a range in a text document that links to an internal or external resource, like another
/// text document or a web site.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentLink {
    /// The range this link applies to.
    pub range: Range,
    /// The uri this link points to.
    pub target: Option<Uri>,

    /// The tooltip text when you hover over this link.
    ///
    /// If a tooltip is provided, is will be displayed in a string that includes instructions on how to
    /// trigger the link, such as `{0} (ctrl + click)`. The specific instructions vary depending on OS,
    /// user settings, and localization.
    pub tooltip: Option<String>,

    /// A data entry field that is preserved on a document link between a DocumentLinkRequest
    /// and a DocumentLinkResolveRequest.
    pub data: Option<Value>,
}

impl_lsp_struct!(DocumentLink,
    required { range: Range = "range" },
    optional {
        target: Uri = "target",
        tooltip: String = "tooltip",
        data: Value = "data",
    }
);
