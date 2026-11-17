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
        optional pub parameters: Vec<ParameterInformation> = "parameters",
        optional pub active_parameter: u32 = "activeParameter",
    }
}

lsp_struct! {
    out
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct ParameterInformation {
        required pub label: ParameterLabel = "label",
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum ParameterLabel {
    Simple(String),
}

impl_untagged_enum!(ParameterLabel { Simple });
