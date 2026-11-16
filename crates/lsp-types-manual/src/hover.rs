use crate::{MarkedString, MarkupContent, Range};

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
    out Hover,
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
    Markup,
    Scalar,
    Array,
});
