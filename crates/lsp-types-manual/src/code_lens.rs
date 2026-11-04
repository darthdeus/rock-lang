use serde_json::Value;

use crate::{
    Command, DynamicRegistrationClientCapabilities, PartialResultParams, Range,
    TextDocumentIdentifier, WorkDoneProgressParams,
};

pub type CodeLensClientCapabilities = DynamicRegistrationClientCapabilities;

/// Code Lens options.
#[derive(Debug, Eq, PartialEq, Clone, Default, Copy)]
pub struct CodeLensOptions {
    pub resolve_provider: Option<bool>,
}

impl_lsp_struct!(
    CodeLensOptions,
    required {},
    optional {
        resolve_provider: bool = "resolveProvider"
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct CodeLensParams {
    /// The document to request code lens for.
    pub text_document: TextDocumentIdentifier,

    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(
    CodeLensParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument"
    },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

/// A code lens represents a command that should be shown along with
/// source text, like the number of references, a way to run tests, etc.
///
/// A code lens is _unresolved_ when no command is associated to it. For performance
/// reasons the creation of a code lens and resolving should be done in two stages.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeLens {
    pub range: Range,
    pub command: Option<Command>,
    pub data: Option<Value>,
}

impl_lsp_struct!(
    CodeLens,
    required {
        range: Range = "range"
    },
    optional {
        command: Command = "command",
        data: Value = "data",
    }
);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodeLensWorkspaceClientCapabilities {
    pub refresh_support: Option<bool>,
}

impl_lsp_struct!(
    CodeLensWorkspaceClientCapabilities,
    required {},
    optional {
        refresh_support: bool = "refreshSupport"
    }
);
