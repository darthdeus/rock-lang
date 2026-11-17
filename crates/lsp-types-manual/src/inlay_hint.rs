use crate::Position;

lsp_struct! {
    out
    /// Inlay hint information.
    ///
    /// @since 3.17.0
    #[derive(Debug, Clone)]
    pub struct InlayHint {
        required pub position: Position = "position",
        required pub label: InlayHintLabel = "label",
        optional pub kind: InlayHintKind = "kind",
        optional pub padding_left: bool = "paddingLeft",
        optional pub padding_right: bool = "paddingRight",
    }
}

#[derive(Debug, Clone)]
pub enum InlayHintLabel {
    String(String),
    LabelParts(Vec<InlayHintLabelPart>),
}

impl_untagged_enum!(InlayHintLabel { LabelParts, String });

lsp_struct! {
    out
    /// An inlay hint label part allows for interactive and composite labels
    /// of inlay hints.
    #[derive(Debug, Clone, Default)]
    pub struct InlayHintLabelPart {
        required pub value: String = "value",
    }
}

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
