use crate::Documentation;

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
