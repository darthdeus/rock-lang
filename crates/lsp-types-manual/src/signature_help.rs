use crate::Documentation;

lsp_struct! {
    out
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct SignatureHelp {
        required pub signatures: Vec<SignatureInformation> = "signatures",
        optional pub active_signature: u32 = "activeSignature",
        optional pub active_parameter: u32 = "activeParameter",
    }
}

lsp_struct! {
    out
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct SignatureInformation {
        required pub label: String = "label",
        optional pub documentation: Documentation = "documentation",
        optional pub parameters: Vec<ParameterInformation> = "parameters",
        optional pub active_parameter: u32 = "activeParameter",
    }
}

lsp_struct! {
    out
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct ParameterInformation {
        required pub label: ParameterLabel = "label",
        optional pub documentation: Documentation = "documentation",
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum ParameterLabel {
    Simple(String),
    LabelOffsets([u32; 2]),
}

impl_untagged_enum!(ParameterLabel {
    LabelOffsets,
    Simple,
});
