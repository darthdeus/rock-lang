use crate::{
    DocumentSelector, DynamicRegistrationClientCapabilities, PartialResultParams, Range,
    TextDocumentIdentifier, TextEdit, WorkDoneProgressParams,
};

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

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentColorParams {
    /// The text document
    pub text_document: TextDocumentIdentifier,

    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(
    DocumentColorParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument"
    },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

#[derive(Debug, PartialEq, Clone)]
pub struct ColorInformation {
    pub range: Range,
    pub color: Color,
}

impl_lsp_struct!(
    ColorInformation,
    required {
        range: Range = "range",
        color: Color = "color",
    },
    optional {}
);

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

#[derive(Debug, PartialEq, Clone)]
pub struct ColorPresentationParams {
    /// The text document.
    pub text_document: TextDocumentIdentifier,

    /// The color information to request presentations for.
    pub color: Color,

    /// The range where the color would be inserted. Serves as a context.
    pub range: Range,

    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(
    ColorPresentationParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        color: Color = "color",
        range: Range = "range",
    },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

#[derive(Debug, PartialEq, Eq, Default, Clone)]
pub struct ColorPresentation {
    pub label: String,
    pub text_edit: Option<TextEdit>,
    pub additional_text_edits: Option<Vec<TextEdit>>,
}

impl_lsp_struct!(ColorPresentation,
    required { label: String = "label" },
    optional {
        text_edit: TextEdit = "textEdit",
        additional_text_edits: Vec<TextEdit> = "additionalTextEdits",
    }
);
