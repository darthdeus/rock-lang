use crate::{
    Documentation, MarkupKind, TextDocumentPositionParams, WorkDoneProgressOptions,
    WorkDoneProgressParams,
};

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SignatureInformationSettings {
    pub documentation_format: Option<Vec<MarkupKind>>,
    pub parameter_information: Option<ParameterInformationSettings>,
    pub active_parameter_support: Option<bool>,
}

impl_lsp_struct!(SignatureInformationSettings,
    required {},
    optional {
        documentation_format: Vec<MarkupKind> = "documentationFormat",
        parameter_information: ParameterInformationSettings = "parameterInformation",
        active_parameter_support: bool = "activeParameterSupport",
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct ParameterInformationSettings {
    pub label_offset_support: Option<bool>,
}

impl_lsp_struct!(
    ParameterInformationSettings,
    required {},
    optional {
        label_offset_support: bool = "labelOffsetSupport"
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SignatureHelpClientCapabilities {
    pub dynamic_registration: Option<bool>,
    pub signature_information: Option<SignatureInformationSettings>,
    pub context_support: Option<bool>,
}

impl_lsp_struct!(
    SignatureHelpClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        signature_information: SignatureInformationSettings = "signatureInformation",
        context_support: bool = "contextSupport",
    }
);

/// Signature help options.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SignatureHelpOptions {
    /// The characters that trigger signature help automatically.
    pub trigger_characters: Option<Vec<String>>,

    /// List of characters that re-trigger signature help.
    pub retrigger_characters: Option<Vec<String>>,

    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(SignatureHelpOptions,
    optional {
        trigger_characters: Vec<String> = "triggerCharacters",
        retrigger_characters: Vec<String> = "retriggerCharacters",
    },
    flatten { work_done_progress_options: WorkDoneProgressOptions }
);

/// Signature help options.
#[derive(Eq, PartialEq, Clone)]
pub struct SignatureHelpTriggerKind(i32);
lsp_enum! {
impl SignatureHelpTriggerKind {
    /// Signature help was invoked manually by the user or by a command.
    pub const INVOKED: SignatureHelpTriggerKind = SignatureHelpTriggerKind(1);
    /// Signature help was triggered by a trigger character.
    pub const TRIGGER_CHARACTER: SignatureHelpTriggerKind = SignatureHelpTriggerKind(2);
    /// Signature help was triggered by the cursor moving or by the document content changing.
    pub const CONTENT_CHANGE: SignatureHelpTriggerKind = SignatureHelpTriggerKind(3);
}
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SignatureHelpParams {
    /// The signature help context. This is only available if the client specifies
    /// to send this using the client capability  `textDocument.signatureHelp.contextSupport === true`
    pub context: Option<SignatureHelpContext>,

    pub text_document_position_params: TextDocumentPositionParams,
    pub work_done_progress_params: WorkDoneProgressParams,
}

impl_lsp_struct!(
    SignatureHelpParams,
    optional {
        context: SignatureHelpContext = "context"
    },
    flatten {
        text_document_position_params: TextDocumentPositionParams,
        work_done_progress_params: WorkDoneProgressParams,
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SignatureHelpContext {
    pub trigger_kind: SignatureHelpTriggerKind,
    pub trigger_character: Option<String>,
    pub is_retrigger: bool,
    pub active_signature_help: Option<SignatureHelp>,
}

impl_lsp_struct!(
    SignatureHelpContext,
    required {
        trigger_kind: SignatureHelpTriggerKind = "triggerKind",
        is_retrigger: bool = "isRetrigger",
    },
    optional {
        trigger_character: String = "triggerCharacter",
        active_signature_help: SignatureHelp = "activeSignatureHelp",
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SignatureHelp {
    pub signatures: Vec<SignatureInformation>,
    pub active_signature: Option<u32>,
    pub active_parameter: Option<u32>,
}

impl_lsp_struct!(SignatureHelp,
    required { signatures: Vec<SignatureInformation> = "signatures" },
    optional {
        active_signature: u32 = "activeSignature",
        active_parameter: u32 = "activeParameter",
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SignatureInformation {
    pub label: String,
    pub documentation: Option<Documentation>,
    pub parameters: Option<Vec<ParameterInformation>>,
    pub active_parameter: Option<u32>,
}

impl_lsp_struct!(SignatureInformation,
    required { label: String = "label" },
    optional {
        documentation: Documentation = "documentation",
        parameters: Vec<ParameterInformation> = "parameters",
        active_parameter: u32 = "activeParameter",
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct ParameterInformation {
    pub label: ParameterLabel,
    pub documentation: Option<Documentation>,
}

impl_lsp_struct!(
    ParameterInformation,
    required {
        label: ParameterLabel = "label"
    },
    optional {
        documentation: Documentation = "documentation"
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum ParameterLabel {
    Simple(String),
    LabelOffsets([u32; 2]),
}

impl_untagged_enum!(ParameterLabel {
    LabelOffsets([u32; 2]),
    Simple(String),
});
