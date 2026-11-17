use crate::{MarkupContent, Range};

lsp_struct! {
    out
    /// The result of a hover request.
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct Hover {
        /// The hover's content
        required pub contents: HoverContents = "contents",
        /// that is used to visualize a hover, e.g. by changing the background color.
        optional pub range: Range = "range",
    }
}

/// Markdown or plain text shown by a hover.
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum HoverContents {
    Markup(MarkupContent),
}

impl_untagged_enum!(HoverContents { Markup });
