use crate::Range;

lsp_struct! {
    out
    /// A document highlight is a range inside a text document which deserves
    /// special attention. Usually a document highlight is visualized by changing
    /// the background color of its range.
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct DocumentHighlight {
        required pub range: Range = "range",
    }
}
