use super::*;

use serde::{de::DeserializeOwned, Serialize};

/// A request the server answers: what it reads and what it sends back.
pub trait Request {
    type Params: DeserializeOwned;
    type Result: Serialize;
    const METHOD: &'static str;
}

macro_rules! requests {
    ($(
        $(#[$attr:meta])*
        $name:ident: $method:literal, $params:ty => $result:ty;
    )*) => {
        $(
            $(#[$attr])*
            pub enum $name {}
            impl Request for $name {
                type Params = $params;
                type Result = $result;
                const METHOD: &'static str = $method;
            }
        )*
    };
}

requests! {
    /// The Completion request is sent from the client to the server to compute completion items at a given cursor position.
    /// Completion items are presented in the IntelliSense user interface. If computing full completion items is expensive,
    /// servers can additionally provide a handler for the completion item resolve request ('completionItem/resolve').
    /// This request is sent when a completion item is selected in the user interface. A typical use case is for example:
    /// the 'textDocument/completion' request doesn’t fill in the documentation property for returned completion items
    /// since it is expensive to compute. When the item is selected in the user interface then a ‘completionItem/resolve’
    /// request is sent with the selected completion item as a param. The returned completion item should have the
    /// documentation property filled in. The request can delay the computation of the detail and documentation properties.
    /// However, properties that are needed for the initial sorting and filtering, like sortText, filterText, insertText,
    /// and textEdit must be provided in the textDocument/completion request and must not be changed during resolve.
    #[derive(Debug)]
    Completion: "textDocument/completion", CompletionParams => Option<CompletionList>;

    /// The hover request is sent from the client to the server to request hover information at a given text
    /// document position.
    #[derive(Debug)]
    HoverRequest: "textDocument/hover", TextDocumentPositionParams => Option<Hover>;

    /// The signature help request is sent from the client to the server to request signature information at
    /// a given cursor position.
    #[derive(Debug)]
    SignatureHelpRequest: "textDocument/signatureHelp", TextDocumentPositionParams => Option<SignatureHelp>;

    /// The goto definition request is sent from the client to the server to resolve the definition location of
    /// a symbol at a given text document position.
    #[derive(Debug)]
    GotoDefinition: "textDocument/definition", TextDocumentPositionParams => Option<Vec<Location>>;

    /// The references request is sent from the client to the server to resolve project-wide references for the
    /// symbol denoted by the given text document position.
    #[derive(Debug)]
    References: "textDocument/references", TextDocumentPositionParams => Option<Vec<Location>>;

    /// The goto type definition request is sent from the client to the
    /// server to resolve the type definition location of a symbol at a
    /// given text document position.
    #[derive(Debug)]
    GotoTypeDefinition: "textDocument/typeDefinition", TextDocumentPositionParams => Option<Vec<Location>>;

    /// The document highlight request is sent from the client to the server to resolve a document highlights
    /// for a given text document position.
    /// For programming languages this usually highlights all references to the symbol scoped to this file.
    /// However we kept 'textDocument/documentHighlight' and 'textDocument/references' separate requests since
    /// the first one is allowed to be more fuzzy.
    /// Symbol matches usually have a DocumentHighlightKind of Read or Write whereas fuzzy or textual matches
    /// use Text as the kind.
    #[derive(Debug)]
    DocumentHighlightRequest: "textDocument/documentHighlight", TextDocumentPositionParams => Option<Vec<DocumentHighlight>>;

    /// The document symbol request is sent from the client to the server to list all symbols found in a given
    /// text document.
    #[derive(Debug)]
    DocumentSymbolRequest: "textDocument/documentSymbol", TextDocumentParams => Option<Vec<DocumentSymbol>>;

    /// The workspace symbol request is sent from the client to the server to list project-wide symbols
    /// matching the query string.
    #[derive(Debug)]
    WorkspaceSymbolRequest: "workspace/symbol", WorkspaceSymbolParams => Option<Vec<SymbolInformation>>;

    /// The code action request is sent from the client to the server to compute commands for a given text document
    /// and range. The request is triggered when the user moves the cursor into a problem marker in the editor or
    /// presses the lightbulb associated with a marker.
    #[derive(Debug)]
    CodeActionRequest: "textDocument/codeAction", CodeActionParams => Option<Vec<CodeAction>>;

    /// The rename request is sent from the client to the server to perform a workspace-wide rename of a symbol.
    #[derive(Debug)]
    Rename: "textDocument/rename", RenameParams => Option<WorkspaceEdit>;

    /// The prepare rename request is sent from the client to the server to setup and test the validity of a rename operation
    /// at a given location.
    #[derive(Debug)]
    PrepareRenameRequest: "textDocument/prepareRename", TextDocumentPositionParams => Option<Range>;

    SemanticTokensFullRequest: "textDocument/semanticTokens/full", TextDocumentParams => Option<SemanticTokens>;

    SemanticTokensFullDeltaRequest: "textDocument/semanticTokens/full/delta", SemanticTokensDeltaParams => Option<SemanticTokensFullDeltaResult>;

    SemanticTokensRangeRequest: "textDocument/semanticTokens/range", TextDocumentRangeParams => Option<SemanticTokens>;

    /// The inlay hints request is sent from the client to the server to compute inlay hints for a given
    /// [text document, range] tuple that may be rendered in the editor in place with other text.
    InlayHintRequest: "textDocument/inlayHint", TextDocumentRangeParams => Option<Vec<InlayHint>>;
}
