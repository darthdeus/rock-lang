use super::*;

use serde::{de::DeserializeOwned, Serialize};

pub trait Request {
    type Params: DeserializeOwned + Serialize + Send + Sync + 'static;
    type Result: DeserializeOwned + Serialize + Send + Sync + 'static;
    const METHOD: &'static str;
}

/// The shutdown request is sent from the client to the server. It asks the server to shut down,
/// but to not exit (otherwise the response might not be delivered correctly to the client).
/// There is a separate exit notification that asks the server to exit.
#[derive(Debug)]
pub enum Shutdown {}

impl Request for Shutdown {
    type Params = ();
    type Result = ();
    const METHOD: &'static str = "shutdown";
}

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
pub enum Completion {}

impl Request for Completion {
    type Params = CompletionParams;
    type Result = Option<CompletionResponse>;
    const METHOD: &'static str = "textDocument/completion";
}

/// The hover request is sent from the client to the server to request hover information at a given text
/// document position.
#[derive(Debug)]
pub enum HoverRequest {}

impl Request for HoverRequest {
    type Params = HoverParams;
    type Result = Option<Hover>;
    const METHOD: &'static str = "textDocument/hover";
}

/// The signature help request is sent from the client to the server to request signature information at
/// a given cursor position.
#[derive(Debug)]
pub enum SignatureHelpRequest {}

impl Request for SignatureHelpRequest {
    type Params = SignatureHelpParams;
    type Result = Option<SignatureHelp>;
    const METHOD: &'static str = "textDocument/signatureHelp";
}

/// The goto definition request is sent from the client to the server to resolve the definition location of
/// a symbol at a given text document position.
#[derive(Debug)]
pub enum GotoDefinition {}

impl Request for GotoDefinition {
    type Params = GotoDefinitionParams;
    type Result = Option<GotoDefinitionResponse>;
    const METHOD: &'static str = "textDocument/definition";
}

/// The references request is sent from the client to the server to resolve project-wide references for the
/// symbol denoted by the given text document position.
#[derive(Debug)]
pub enum References {}

impl Request for References {
    type Params = ReferenceParams;
    type Result = Option<Vec<Location>>;
    const METHOD: &'static str = "textDocument/references";
}

/// The goto type definition request is sent from the client to the
/// server to resolve the type definition location of a symbol at a
/// given text document position.
#[derive(Debug)]
pub enum GotoTypeDefinition {}

pub type GotoTypeDefinitionParams = GotoDefinitionParams;
pub type GotoTypeDefinitionResponse = GotoDefinitionResponse;

impl Request for GotoTypeDefinition {
    type Params = GotoTypeDefinitionParams;
    type Result = Option<GotoTypeDefinitionResponse>;
    const METHOD: &'static str = "textDocument/typeDefinition";
}

/// The document highlight request is sent from the client to the server to resolve a document highlights
/// for a given text document position.
/// For programming languages this usually highlights all references to the symbol scoped to this file.
/// However we kept 'textDocument/documentHighlight' and 'textDocument/references' separate requests since
/// the first one is allowed to be more fuzzy.
/// Symbol matches usually have a DocumentHighlightKind of Read or Write whereas fuzzy or textual matches
/// use Text as the kind.
#[derive(Debug)]
pub enum DocumentHighlightRequest {}

impl Request for DocumentHighlightRequest {
    type Params = DocumentHighlightParams;
    type Result = Option<Vec<DocumentHighlight>>;
    const METHOD: &'static str = "textDocument/documentHighlight";
}

/// The document symbol request is sent from the client to the server to list all symbols found in a given
/// text document.
#[derive(Debug)]
pub enum DocumentSymbolRequest {}

impl Request for DocumentSymbolRequest {
    type Params = DocumentSymbolParams;
    type Result = Option<DocumentSymbolResponse>;
    const METHOD: &'static str = "textDocument/documentSymbol";
}

/// The workspace symbol request is sent from the client to the server to list project-wide symbols
/// matching the query string.
#[derive(Debug)]
pub enum WorkspaceSymbolRequest {}

impl Request for WorkspaceSymbolRequest {
    type Params = WorkspaceSymbolParams;
    type Result = Option<WorkspaceSymbolResponse>;
    const METHOD: &'static str = "workspace/symbol";
}

/// The code action request is sent from the client to the server to compute commands for a given text document
/// and range. The request is triggered when the user moves the cursor into a problem marker in the editor or
/// presses the lightbulb associated with a marker.
#[derive(Debug)]
pub enum CodeActionRequest {}

impl Request for CodeActionRequest {
    type Params = CodeActionParams;
    type Result = Option<CodeActionResponse>;
    const METHOD: &'static str = "textDocument/codeAction";
}

/// The rename request is sent from the client to the server to perform a workspace-wide rename of a symbol.
#[derive(Debug)]
pub enum Rename {}

impl Request for Rename {
    type Params = RenameParams;
    type Result = Option<WorkspaceEdit>;
    const METHOD: &'static str = "textDocument/rename";
}

/// The prepare rename request is sent from the client to the server to setup and test the validity of a rename operation
/// at a given location.
#[derive(Debug)]
pub enum PrepareRenameRequest {}

impl Request for PrepareRenameRequest {
    type Params = TextDocumentPositionParams;
    type Result = Option<PrepareRenameResponse>;
    const METHOD: &'static str = "textDocument/prepareRename";
}

pub enum SemanticTokensFullRequest {}

impl Request for SemanticTokensFullRequest {
    type Params = SemanticTokensParams;
    type Result = Option<SemanticTokensResult>;
    const METHOD: &'static str = "textDocument/semanticTokens/full";
}

pub enum SemanticTokensFullDeltaRequest {}

impl Request for SemanticTokensFullDeltaRequest {
    type Params = SemanticTokensDeltaParams;
    type Result = Option<SemanticTokensFullDeltaResult>;
    const METHOD: &'static str = "textDocument/semanticTokens/full/delta";
}

pub enum SemanticTokensRangeRequest {}

impl Request for SemanticTokensRangeRequest {
    type Params = SemanticTokensRangeParams;
    type Result = Option<SemanticTokensRangeResult>;
    const METHOD: &'static str = "textDocument/semanticTokens/range";
}

/// The inlay hints request is sent from the client to the server to compute inlay hints for a given
/// [text document, range] tuple that may be rendered in the editor in place with other text.
pub enum InlayHintRequest {}

impl Request for InlayHintRequest {
    type Params = InlayHintParams;
    type Result = Option<Vec<InlayHint>>;
    const METHOD: &'static str = "textDocument/inlayHint";
}
