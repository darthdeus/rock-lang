use crate::{LSPAny, MarkupContent, Position, TextEdit};

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

impl_lsp_struct!(out InlayHint,
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

impl_untagged_enum!(InlayHintLabel { LabelParts, String });

#[derive(Debug, Clone)]
pub enum InlayHintTooltip {
    String(String),
    MarkupContent(MarkupContent),
}

impl_untagged_enum!(InlayHintTooltip {
    MarkupContent,
    String,
});

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
