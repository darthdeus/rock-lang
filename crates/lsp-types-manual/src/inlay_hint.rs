use crate::{
    Command, LSPAny, Location, MarkupContent, Position, Range, StaticRegistrationOptions,
    TextDocumentIdentifier, TextDocumentRegistrationOptions, TextEdit, WorkDoneProgressOptions,
    WorkDoneProgressParams,
};

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum InlayHintServerCapabilities {
    Options(InlayHintOptions),
    RegistrationOptions(InlayHintRegistrationOptions),
}

impl_untagged_enum!(InlayHintServerCapabilities {
    Options(InlayHintOptions),
    RegistrationOptions(InlayHintRegistrationOptions),
});

/// Inlay hint client capabilities.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlayHintClientCapabilities {
    pub dynamic_registration: Option<bool>,
    pub resolve_support: Option<InlayHintResolveClientCapabilities>,
}

impl_lsp_struct!(
    InlayHintClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        resolve_support: InlayHintResolveClientCapabilities = "resolveSupport",
    }
);

/// Inlay hint options used during static registration.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlayHintOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,

    /// The server provides support to resolve additional
    /// information for an inlay hint item.
    pub resolve_provider: Option<bool>,
}

impl_lsp_struct!(
    InlayHintOptions,
    optional {
        resolve_provider: bool = "resolveProvider"
    },
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

/// Inlay hint options used during static or dynamic registration.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlayHintRegistrationOptions {
    pub inlay_hint_options: InlayHintOptions,
    pub text_document_registration_options: TextDocumentRegistrationOptions,
    pub static_registration_options: StaticRegistrationOptions,
}

impl_lsp_struct!(
    InlayHintRegistrationOptions,
    flatten {
        inlay_hint_options: InlayHintOptions,
        text_document_registration_options: TextDocumentRegistrationOptions,
        static_registration_options: StaticRegistrationOptions,
    }
);

/// A parameter literal used in inlay hint requests.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct InlayHintParams {
    pub work_done_progress_params: WorkDoneProgressParams,

    /// The text document.
    pub text_document: TextDocumentIdentifier,

    /// The visible document range for which inlay hints should be computed.
    pub range: Range,
}

impl_lsp_struct!(
    InlayHintParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        range: Range = "range",
    },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams
    }
);

/// Inlay hint information.
///
/// @since 3.17.0
#[derive(Debug, Clone)]
pub struct InlayHint {
    pub position: Position,
    pub label: InlayHintLabel,
    pub kind: Option<InlayHintKind>,
    pub text_edits: Option<Vec<TextEdit>>,
    pub tooltip: Option<InlayHintTooltip>,
    pub padding_left: Option<bool>,
    pub padding_right: Option<bool>,
    pub data: Option<LSPAny>,
}

impl_lsp_struct!(InlayHint,
    required {
        position: Position = "position",
        label: InlayHintLabel = "label",
    },
    optional {
        kind: InlayHintKind = "kind",
        text_edits: Vec<TextEdit> = "textEdits",
        tooltip: InlayHintTooltip = "tooltip",
        padding_left: bool = "paddingLeft",
        padding_right: bool = "paddingRight",
        data: LSPAny = "data",
    }
);

#[derive(Debug, Clone)]
pub enum InlayHintLabel {
    String(String),
    LabelParts(Vec<InlayHintLabelPart>),
}

impl_untagged_enum!(InlayHintLabel {
    LabelParts(Vec<InlayHintLabelPart>),
    String(String),
});

impl From<String> for InlayHintLabel {
    #[inline]
    fn from(from: String) -> Self {
        Self::String(from)
    }
}

impl From<Vec<InlayHintLabelPart>> for InlayHintLabel {
    #[inline]
    fn from(from: Vec<InlayHintLabelPart>) -> Self {
        Self::LabelParts(from)
    }
}

#[derive(Debug, Clone)]
pub enum InlayHintTooltip {
    String(String),
    MarkupContent(MarkupContent),
}

impl_untagged_enum!(InlayHintTooltip {
    MarkupContent(MarkupContent),
    String(String),
});

impl From<String> for InlayHintTooltip {
    #[inline]
    fn from(from: String) -> Self {
        Self::String(from)
    }
}

impl From<MarkupContent> for InlayHintTooltip {
    #[inline]
    fn from(from: MarkupContent) -> Self {
        Self::MarkupContent(from)
    }
}

/// An inlay hint label part allows for interactive and composite labels
/// of inlay hints.
#[derive(Debug, Clone, Default)]
pub struct InlayHintLabelPart {
    pub value: String,
    pub tooltip: Option<InlayHintLabelPartTooltip>,
    pub location: Option<Location>,
    pub command: Option<Command>,
}

impl_lsp_struct!(
    InlayHintLabelPart,
    required {
        value: String = "value"
    },
    optional {
        tooltip: InlayHintLabelPartTooltip = "tooltip",
        location: Location = "location",
        command: Command = "command",
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum InlayHintLabelPartTooltip {
    String(String),
    MarkupContent(MarkupContent),
}

impl_untagged_enum!(InlayHintLabelPartTooltip {
    MarkupContent(MarkupContent),
    String(String),
});

impl From<String> for InlayHintLabelPartTooltip {
    #[inline]
    fn from(from: String) -> Self {
        Self::String(from)
    }
}

impl From<MarkupContent> for InlayHintLabelPartTooltip {
    #[inline]
    fn from(from: MarkupContent) -> Self {
        Self::MarkupContent(from)
    }
}

/// Inlay hint kinds.
///
/// @since 3.17.0
#[derive(Eq, PartialEq, Copy, Clone)]
pub struct InlayHintKind(i32);
lsp_enum! {
impl InlayHintKind {
    /// An inlay hint that for a type annotation.
    pub const TYPE: InlayHintKind = InlayHintKind(1);

    /// An inlay hint that is for a parameter.
    pub const PARAMETER: InlayHintKind = InlayHintKind(2);
}
}

/// Inlay hint client capabilities.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlayHintResolveClientCapabilities {
    pub properties: Vec<String>,
}

impl_lsp_struct!(InlayHintResolveClientCapabilities,
    required { properties: Vec<String> = "properties" },
    optional {}
);

/// Client workspace capabilities specific to inlay hints.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlayHintWorkspaceClientCapabilities {
    pub refresh_support: Option<bool>,
}

impl_lsp_struct!(
    InlayHintWorkspaceClientCapabilities,
    required {},
    optional {
        refresh_support: bool = "refreshSupport"
    }
);

// TODO(sno2): add tests once stabilized
