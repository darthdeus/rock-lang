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
    #[derive(Debug)]
    Completion: "textDocument/completion", CompletionParams => Option<CompletionList>;

    #[derive(Debug)]
    HoverRequest: "textDocument/hover", TextDocumentPositionParams => Option<Hover>;

    #[derive(Debug)]
    SignatureHelpRequest: "textDocument/signatureHelp", TextDocumentPositionParams => Option<SignatureHelp>;

    #[derive(Debug)]
    GotoDefinition: "textDocument/definition", TextDocumentPositionParams => Option<Vec<Location>>;

    #[derive(Debug)]
    References: "textDocument/references", TextDocumentPositionParams => Option<Vec<Location>>;

    #[derive(Debug)]
    GotoTypeDefinition: "textDocument/typeDefinition", TextDocumentPositionParams => Option<Vec<Location>>;

    #[derive(Debug)]
    DocumentHighlightRequest: "textDocument/documentHighlight", TextDocumentPositionParams => Option<Vec<DocumentHighlight>>;

    #[derive(Debug)]
    DocumentSymbolRequest: "textDocument/documentSymbol", TextDocumentParams => Option<Vec<DocumentSymbol>>;

    #[derive(Debug)]
    WorkspaceSymbolRequest: "workspace/symbol", WorkspaceSymbolParams => Option<Vec<SymbolInformation>>;

    #[derive(Debug)]
    CodeActionRequest: "textDocument/codeAction", CodeActionParams => Option<Vec<CodeAction>>;

    #[derive(Debug)]
    Rename: "textDocument/rename", RenameParams => Option<WorkspaceEdit>;

    #[derive(Debug)]
    PrepareRenameRequest: "textDocument/prepareRename", TextDocumentPositionParams => Option<Range>;

    SemanticTokensFullRequest: "textDocument/semanticTokens/full", TextDocumentParams => Option<SemanticTokens>;

    SemanticTokensFullDeltaRequest: "textDocument/semanticTokens/full/delta", SemanticTokensDeltaParams => Option<SemanticTokensFullDeltaResult>;

    SemanticTokensRangeRequest: "textDocument/semanticTokens/range", TextDocumentRangeParams => Option<SemanticTokens>;

    InlayHintRequest: "textDocument/inlayHint", TextDocumentRangeParams => Option<Vec<InlayHint>>;
}
