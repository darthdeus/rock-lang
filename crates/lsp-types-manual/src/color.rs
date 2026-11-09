use crate::{DocumentSelector, DynamicRegistrationClientCapabilities};

pub type DocumentColorClientCapabilities = DynamicRegistrationClientCapabilities;

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct ColorProviderOptions {}

impl_lsp_struct!(ColorProviderOptions, required {}, optional {});

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct StaticTextDocumentColorProviderOptions {
    pub document_selector: Option<DocumentSelector>,
    pub id: Option<String>,
}

impl_lsp_struct!(
    StaticTextDocumentColorProviderOptions,
    required {},
    nullable {
        document_selector: DocumentSelector = "documentSelector"
    },
    optional { id: String = "id" }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum ColorProviderCapability {
    Simple(bool),
    ColorProvider(ColorProviderOptions),
    Options(StaticTextDocumentColorProviderOptions),
}

impl_untagged_enum!(ColorProviderCapability {
    Simple(bool),
    ColorProvider(ColorProviderOptions),
    Options(StaticTextDocumentColorProviderOptions),
});

impl From<ColorProviderOptions> for ColorProviderCapability {
    fn from(from: ColorProviderOptions) -> Self {
        Self::ColorProvider(from)
    }
}

impl From<StaticTextDocumentColorProviderOptions> for ColorProviderCapability {
    fn from(from: StaticTextDocumentColorProviderOptions) -> Self {
        Self::Options(from)
    }
}

impl From<bool> for ColorProviderCapability {
    fn from(from: bool) -> Self {
        Self::Simple(from)
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Color {
    pub red: f32,
    pub green: f32,
    pub blue: f32,
    pub alpha: f32,
}

impl_lsp_struct!(
    Color,
    required {
        red: f32 = "red",
        green: f32 = "green",
        blue: f32 = "blue",
        alpha: f32 = "alpha",
    },
    optional {}
);
