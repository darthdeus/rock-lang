use crate::{
    DynamicRegistrationClientCapabilities, PartialResultParams, TextDocumentPositionParams,
    TextDocumentRegistrationOptions, WorkDoneProgressOptions, WorkDoneProgressParams,
};

pub type MonikerClientCapabilities = DynamicRegistrationClientCapabilities;

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct MonikerOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    MonikerOptions,
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct MonikerRegistrationOptions {
    pub text_document_registration_options: TextDocumentRegistrationOptions,
    pub moniker_options: MonikerOptions,
}

impl_lsp_struct!(
    MonikerRegistrationOptions,
    flatten {
        text_document_registration_options: TextDocumentRegistrationOptions,
        moniker_options: MonikerOptions,
    }
);

/// Moniker uniqueness level to define scope of the moniker.
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum UniquenessLevel {
    /// The moniker is only unique inside a document
    Document,
    /// The moniker is unique inside a project for which a dump got created
    Project,
    /// The moniker is unique inside the group to which a project belongs
    Group,
    /// The moniker is unique inside the moniker scheme.
    Scheme,
    /// The moniker is globally unique
    Global,
}

impl_str_enum!(UniquenessLevel {
    Document = "document",
    Project = "project",
    Group = "group",
    Scheme = "scheme",
    Global = "global",
});

/// The moniker kind.
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum MonikerKind {
    /// The moniker represent a symbol that is imported into a project
    Import,
    /// The moniker represent a symbol that is exported into a project
    Export,
    /// The moniker represents a symbol that is local to a project (e.g. a local
    /// variable of a function, a class not visible outside the project, ...)
    Local,
}

impl_str_enum!(MonikerKind { Import = "import", Export = "export", Local = "local" });

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct MonikerParams {
    pub text_document_position_params: TextDocumentPositionParams,
    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(
    MonikerParams,
    flatten {
        text_document_position_params: TextDocumentPositionParams,
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

/// Moniker definition to match LSIF 0.5 moniker definition.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct Moniker {
    /// The scheme of the moniker. For example tsc or .Net
    pub scheme: String,

    /// The identifier of the moniker. The value is opaque in LSIF however
    /// schema owners are allowed to define the structure if they want.
    pub identifier: String,

    /// The scope in which the moniker is unique
    pub unique: UniquenessLevel,

    /// The moniker kind if known.
    pub kind: Option<MonikerKind>,
}

impl_lsp_struct!(
    Moniker,
    required {
        scheme: String = "scheme",
        identifier: String = "identifier",
        unique: UniquenessLevel = "unique",
    },
    optional {
        kind: MonikerKind = "kind"
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum MonikerServerCapabilities {
    Options(MonikerOptions),
    RegistrationOptions(MonikerRegistrationOptions),
}

impl_untagged_enum!(MonikerServerCapabilities {
    Options(MonikerOptions),
    RegistrationOptions(MonikerRegistrationOptions),
});
