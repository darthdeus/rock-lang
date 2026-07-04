use std::collections::HashMap;

use crate::{
    Diagnostic, PartialResultParams, StaticRegistrationOptions, TextDocumentIdentifier,
    TextDocumentRegistrationOptions, Uri, WorkDoneProgressOptions, WorkDoneProgressParams,
};

/// Client capabilities specific to diagnostic pull requests.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct DiagnosticClientCapabilities {
    pub dynamic_registration: Option<bool>,
    pub related_document_support: Option<bool>,
}

impl_lsp_struct!(DiagnosticClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        related_document_support: bool = "relatedDocumentSupport",
    }
);

/// Diagnostic options.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct DiagnosticOptions {
    /// An optional identifier under which the diagnostics are
    /// managed by the client.
    pub identifier: Option<String>,

    /// Whether the language has inter file dependencies, meaning that editing code in one file can
    /// result in a different diagnostic set in another file.
    pub inter_file_dependencies: bool,

    /// The server provides support for workspace diagnostics as well.
    pub workspace_diagnostics: bool,

    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(DiagnosticOptions,
    required {
        inter_file_dependencies: bool = "interFileDependencies",
        workspace_diagnostics: bool = "workspaceDiagnostics",
    },
    optional { identifier: String = "identifier" },
    flatten { work_done_progress_options: WorkDoneProgressOptions }
);

/// Diagnostic registration options.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct DiagnosticRegistrationOptions {
    pub text_document_registration_options: TextDocumentRegistrationOptions,
    pub diagnostic_options: DiagnosticOptions,
    pub static_registration_options: StaticRegistrationOptions,
}

impl_lsp_struct!(DiagnosticRegistrationOptions,
    flatten {
        text_document_registration_options: TextDocumentRegistrationOptions,
        diagnostic_options: DiagnosticOptions,
        static_registration_options: StaticRegistrationOptions,
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum DiagnosticServerCapabilities {
    Options(DiagnosticOptions),
    RegistrationOptions(DiagnosticRegistrationOptions),
}

impl_untagged_enum!(DiagnosticServerCapabilities {
    Options(DiagnosticOptions),
    RegistrationOptions(DiagnosticRegistrationOptions),
});

/// Parameters of the document diagnostic request.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentDiagnosticParams {
    /// The text document.
    pub text_document: TextDocumentIdentifier,

    /// The additional identifier provided during registration.
    pub identifier: Option<String>,

    /// The result ID of a previous response if provided.
    pub previous_result_id: Option<String>,

    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(DocumentDiagnosticParams,
    required { text_document: TextDocumentIdentifier = "textDocument" },
    nullable {
        identifier: String = "identifier",
        previous_result_id: String = "previousResultId",
    },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

/// A diagnostic report with a full set of problems.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Default, Clone)]
pub struct FullDocumentDiagnosticReport {
    pub result_id: Option<String>,
    pub items: Vec<Diagnostic>,
}

impl_lsp_struct!(FullDocumentDiagnosticReport,
    required { items: Vec<Diagnostic> = "items" },
    optional { result_id: String = "resultId" }
);

/// A diagnostic report indicating that the last returned report is still accurate.
///
/// A server can only return `unchanged` if result ids are provided.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Clone)]
pub struct UnchangedDocumentDiagnosticReport {
    pub result_id: String,
}

impl_lsp_struct!(UnchangedDocumentDiagnosticReport,
    required { result_id: String = "resultId" },
    optional {}
);

/// The document diagnostic report kinds.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Clone)]
pub enum DocumentDiagnosticReportKind {
    /// A diagnostic report with a full set of problems.
    Full(FullDocumentDiagnosticReport),
    /// A report indicating that the last returned report is still accurate.
    Unchanged(UnchangedDocumentDiagnosticReport),
}

impl_tagged_enum!(DocumentDiagnosticReportKind, tag = "kind" {
    Full(FullDocumentDiagnosticReport) = "full",
    Unchanged(UnchangedDocumentDiagnosticReport) = "unchanged",
});

impl From<FullDocumentDiagnosticReport> for DocumentDiagnosticReportKind {
    fn from(from: FullDocumentDiagnosticReport) -> Self {
        DocumentDiagnosticReportKind::Full(from)
    }
}

impl From<UnchangedDocumentDiagnosticReport> for DocumentDiagnosticReportKind {
    fn from(from: UnchangedDocumentDiagnosticReport) -> Self {
        DocumentDiagnosticReportKind::Unchanged(from)
    }
}

/// A full diagnostic report with a set of related documents.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Default, Clone)]
pub struct RelatedFullDocumentDiagnosticReport {
    /// Diagnostics of related documents.
    ///
    /// This information is useful in programming languages where code in a file A can generate
    /// diagnostics in a file B which A depends on. An example of such a language is C/C++ where
    /// macro definitions in a file `a.cpp` result in errors in a header file `b.hpp`.
    ///
    /// @since 3.17.0
    // relatedDocuments?: { [uri: string]: FullDocumentDiagnosticReport | UnchangedDocumentDiagnosticReport; };
    pub related_documents: Option<HashMap<Uri, DocumentDiagnosticReportKind>>,
    pub full_document_diagnostic_report: FullDocumentDiagnosticReport,
}

impl_lsp_struct!(RelatedFullDocumentDiagnosticReport,
    optional { related_documents: HashMap<Uri, DocumentDiagnosticReportKind> = "relatedDocuments" },
    flatten { full_document_diagnostic_report: FullDocumentDiagnosticReport }
);

/// An unchanged diagnostic report with a set of related documents.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Clone)]
pub struct RelatedUnchangedDocumentDiagnosticReport {
    /// Diagnostics of related documents.
    ///
    /// This information is useful in programming languages where code in a file A can generate
    /// diagnostics in a file B which A depends on. An example of such a language is C/C++ where
    /// macro definitions in a file `a.cpp` result in errors in a header file `b.hpp`.
    ///
    /// @since 3.17.0
    // relatedDocuments?: { [uri: string]: FullDocumentDiagnosticReport | UnchangedDocumentDiagnosticReport; };
    pub related_documents: Option<HashMap<Uri, DocumentDiagnosticReportKind>>,
    pub unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport,
}

impl_lsp_struct!(RelatedUnchangedDocumentDiagnosticReport,
    optional { related_documents: HashMap<Uri, DocumentDiagnosticReportKind> = "relatedDocuments" },
    flatten { unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport }
);

/// The result of a document diagnostic pull request.
///
/// A report can either be a full report containing all diagnostics for the requested document or
/// an unchanged report indicating that nothing has changed in terms of diagnostics in comparison
/// to the last pull request.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Clone)]
pub enum DocumentDiagnosticReport {
    /// A diagnostic report with a full set of problems.
    Full(RelatedFullDocumentDiagnosticReport),
    /// A report indicating that the last returned report is still accurate.
    Unchanged(RelatedUnchangedDocumentDiagnosticReport),
}

impl_tagged_enum!(DocumentDiagnosticReport, tag = "kind" {
    Full(RelatedFullDocumentDiagnosticReport) = "full",
    Unchanged(RelatedUnchangedDocumentDiagnosticReport) = "unchanged",
});

impl From<RelatedFullDocumentDiagnosticReport> for DocumentDiagnosticReport {
    fn from(from: RelatedFullDocumentDiagnosticReport) -> Self {
        DocumentDiagnosticReport::Full(from)
    }
}

impl From<RelatedUnchangedDocumentDiagnosticReport> for DocumentDiagnosticReport {
    fn from(from: RelatedUnchangedDocumentDiagnosticReport) -> Self {
        DocumentDiagnosticReport::Unchanged(from)
    }
}

/// A partial result for a document diagnostic report.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Default, Clone)]
pub struct DocumentDiagnosticReportPartialResult {
    pub related_documents: Option<HashMap<Uri, DocumentDiagnosticReportKind>>,
}

impl_lsp_struct!(DocumentDiagnosticReportPartialResult,
    required {},
    optional { related_documents: HashMap<Uri, DocumentDiagnosticReportKind> = "relatedDocuments" }
);

#[derive(Debug, PartialEq, Clone)]
pub enum DocumentDiagnosticReportResult {
    Report(DocumentDiagnosticReport),
    Partial(DocumentDiagnosticReportPartialResult),
}

impl_untagged_enum!(DocumentDiagnosticReportResult {
    Report(DocumentDiagnosticReport),
    Partial(DocumentDiagnosticReportPartialResult),
});

impl From<DocumentDiagnosticReport> for DocumentDiagnosticReportResult {
    fn from(from: DocumentDiagnosticReport) -> Self {
        DocumentDiagnosticReportResult::Report(from)
    }
}

impl From<DocumentDiagnosticReportPartialResult> for DocumentDiagnosticReportResult {
    fn from(from: DocumentDiagnosticReportPartialResult) -> Self {
        DocumentDiagnosticReportResult::Partial(from)
    }
}

/// Cancellation data returned from a diagnostic request.
///
/// If no data is provided, it defaults to `{ retrigger_request: true }`.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Clone)]
pub struct DiagnosticServerCancellationData {
    pub retrigger_request: bool,
}

impl_lsp_struct!(DiagnosticServerCancellationData,
    required { retrigger_request: bool = "retriggerRequest" },
    optional {}
);

impl Default for DiagnosticServerCancellationData {
    fn default() -> Self {
        DiagnosticServerCancellationData {
            retrigger_request: true,
        }
    }
}
