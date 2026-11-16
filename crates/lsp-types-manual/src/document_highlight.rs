use crate::Range;

/// A document highlight is a range inside a text document which deserves
/// special attention. Usually a document highlight is visualized by changing
/// the background color of its range.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentHighlight {
    /// The range this highlight applies to.
    pub range: Range,

    /// The highlight kind, default is DocumentHighlightKind.Text.
    pub kind: Option<DocumentHighlightKind>,
}

impl_lsp_struct!(
    out DocumentHighlight,
    required {
        range: Range = "range"
    },
    optional {
        kind: DocumentHighlightKind = "kind"
    }
);

/// A document highlight kind.
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub struct DocumentHighlightKind(i32);
lsp_enum! {
impl DocumentHighlightKind {
    /// A textual occurrence.
    pub const TEXT: DocumentHighlightKind = DocumentHighlightKind(1);

    /// Read-access of a symbol, like reading a variable.
    pub const READ: DocumentHighlightKind = DocumentHighlightKind(2);

    /// Write-access of a symbol, like writing to a variable.
    pub const WRITE: DocumentHighlightKind = DocumentHighlightKind(3);
}
}
