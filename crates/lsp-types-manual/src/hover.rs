use crate::{MarkupContent, Range};

lsp_struct! {
    out
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct Hover {
        required pub contents: HoverContents = "contents",
        optional pub range: Range = "range",
    }
}

/// Markdown-formatted content shown by a hover.
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum HoverContents {
    Markup(MarkupContent),
}

impl_untagged_enum!(HoverContents { Markup });
