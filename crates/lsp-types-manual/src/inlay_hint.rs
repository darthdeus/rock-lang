use crate::Position;

/// Inlay hint information.
///
/// @since 3.17.0
#[derive(Debug, Clone)]
pub struct InlayHint {
    pub position: Position,
    pub label: InlayHintLabel,
    pub kind: Option<InlayHintKind>,
    pub padding_left: Option<bool>,
    pub padding_right: Option<bool>,
}

impl_lsp_struct!(out InlayHint,
    required {
        position: Position = "position",
        label: InlayHintLabel = "label",
    },
    optional {
        kind: InlayHintKind = "kind",
        padding_left: bool = "paddingLeft",
        padding_right: bool = "paddingRight",
    }
);

#[derive(Debug, Clone)]
pub enum InlayHintLabel {
    String(String),
    LabelParts(Vec<InlayHintLabelPart>),
}

impl_untagged_enum!(InlayHintLabel { LabelParts, String });

/// An inlay hint label part allows for interactive and composite labels
/// of inlay hints.
#[derive(Debug, Clone, Default)]
pub struct InlayHintLabelPart {
    pub value: String,
}

impl_lsp_struct!(
    out InlayHintLabelPart,
    required {
        value: String = "value"
    },
    optional {}
);

/// Inlay hint kinds.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub struct InlayHintKind(i32);
lsp_enum! {
impl InlayHintKind {
    /// An inlay hint that for a type annotation.
    pub const TYPE: InlayHintKind = InlayHintKind(1);

    /// An inlay hint that is for a parameter.
    pub const PARAMETER: InlayHintKind = InlayHintKind(2);
}
}
