use crate::Position;

lsp_struct! {
    out
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
}

impl_untagged_enum!(InlayHintLabel { String });

/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub struct InlayHintKind(i32);
lsp_enum! {
impl InlayHintKind {
    /// Type annotation hint.
    pub const TYPE: InlayHintKind = InlayHintKind(1);

    /// Parameter hint.
    pub const PARAMETER: InlayHintKind = InlayHintKind(2);
}
}
