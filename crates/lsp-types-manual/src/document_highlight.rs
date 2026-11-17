use crate::Range;

lsp_struct! {
    out
    /// A document range to highlight.
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct DocumentHighlight {
        required pub range: Range = "range",
    }
}
