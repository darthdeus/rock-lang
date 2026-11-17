use crate::{MarkupContent, Range};

lsp_struct! {
    out
    /// The result of a hover request.
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct Hover {
        required pub contents: HoverContents = "contents",
        optional pub range: Range = "range",
    }
}

/// Markdown or plain text shown by a hover.
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum HoverContents {
    Markup(MarkupContent),
}

impl_untagged_enum!(HoverContents { Markup });
