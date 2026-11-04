use crate::{
    FullDocumentDiagnosticReport, PartialResultParams, UnchangedDocumentDiagnosticReport, Uri,
    WorkDoneProgressParams,
};

/// Workspace client capabilities specific to diagnostic pull requests.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct DiagnosticWorkspaceClientCapabilities {
    pub refresh_support: Option<bool>,
}

impl_lsp_struct!(
    DiagnosticWorkspaceClientCapabilities,
    required {},
    optional {
        refresh_support: bool = "refreshSupport"
    }
);

/// A previous result ID in a workspace pull request.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct PreviousResultId {
    pub uri: Uri,
    pub value: String,
}

impl_lsp_struct!(
    PreviousResultId,
    required {
        uri: Uri = "uri",
        value: String = "value",
    },
    optional {}
);

/// Parameters of the workspace diagnostic request.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct WorkspaceDiagnosticParams {
    /// The additional identifier provided during registration.
    pub identifier: Option<String>,

    /// The currently known diagnostic reports with their
    /// previous result ids.
    pub previous_result_ids: Vec<PreviousResultId>,

    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(WorkspaceDiagnosticParams,
    required { previous_result_ids: Vec<PreviousResultId> = "previousResultIds" },
    nullable { identifier: String = "identifier" },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

/// A full document diagnostic report for a workspace diagnostic result.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Clone)]
pub struct WorkspaceFullDocumentDiagnosticReport {
    /// The URI for which diagnostic information is reported.
    pub uri: Uri,

    /// The version number for which the diagnostics are reported.
    ///
    /// If the document is not marked as open, `None` can be provided.
    pub version: Option<i64>,

    pub full_document_diagnostic_report: FullDocumentDiagnosticReport,
}

impl_lsp_struct!(
    WorkspaceFullDocumentDiagnosticReport,
    required { uri: Uri = "uri" },
    nullable {
        version: i64 = "version"
    },
    optional {},
    flatten {
        full_document_diagnostic_report: FullDocumentDiagnosticReport
    }
);

/// An unchanged document diagnostic report for a workspace diagnostic result.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Clone)]
pub struct WorkspaceUnchangedDocumentDiagnosticReport {
    /// The URI for which diagnostic information is reported.
    pub uri: Uri,

    /// The version number for which the diagnostics are reported.
    ///
    /// If the document is not marked as open, `None` can be provided.
    pub version: Option<i64>,

    pub unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport,
}

impl_lsp_struct!(
    WorkspaceUnchangedDocumentDiagnosticReport,
    required { uri: Uri = "uri" },
    nullable {
        version: i64 = "version"
    },
    optional {},
    flatten {
        unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport
    }
);

/// A workspace diagnostic document report.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Clone)]
pub enum WorkspaceDocumentDiagnosticReport {
    Full(WorkspaceFullDocumentDiagnosticReport),
    Unchanged(WorkspaceUnchangedDocumentDiagnosticReport),
}

impl_tagged_enum!(WorkspaceDocumentDiagnosticReport, tag = "kind" {
    Full(WorkspaceFullDocumentDiagnosticReport) = "full",
    Unchanged(WorkspaceUnchangedDocumentDiagnosticReport) = "unchanged",
});

impl From<WorkspaceFullDocumentDiagnosticReport> for WorkspaceDocumentDiagnosticReport {
    fn from(from: WorkspaceFullDocumentDiagnosticReport) -> Self {
        WorkspaceDocumentDiagnosticReport::Full(from)
    }
}

impl From<WorkspaceUnchangedDocumentDiagnosticReport> for WorkspaceDocumentDiagnosticReport {
    fn from(from: WorkspaceUnchangedDocumentDiagnosticReport) -> Self {
        WorkspaceDocumentDiagnosticReport::Unchanged(from)
    }
}

/// A workspace diagnostic report.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Default, Clone)]
pub struct WorkspaceDiagnosticReport {
    pub items: Vec<WorkspaceDocumentDiagnosticReport>,
}

impl_lsp_struct!(WorkspaceDiagnosticReport,
    required { items: Vec<WorkspaceDocumentDiagnosticReport> = "items" },
    optional {}
);

/// A partial result for a workspace diagnostic report.
///
/// @since 3.17.0
#[derive(Debug, PartialEq, Default, Clone)]
pub struct WorkspaceDiagnosticReportPartialResult {
    pub items: Vec<WorkspaceDocumentDiagnosticReport>,
}

impl_lsp_struct!(WorkspaceDiagnosticReportPartialResult,
    required { items: Vec<WorkspaceDocumentDiagnosticReport> = "items" },
    optional {}
);

#[derive(Debug, PartialEq, Clone)]
pub enum WorkspaceDiagnosticReportResult {
    Report(WorkspaceDiagnosticReport),
    Partial(WorkspaceDiagnosticReportPartialResult),
}

impl_untagged_enum!(WorkspaceDiagnosticReportResult {
    Report(WorkspaceDiagnosticReport),
    Partial(WorkspaceDiagnosticReportPartialResult),
});

impl From<WorkspaceDiagnosticReport> for WorkspaceDiagnosticReportResult {
    fn from(from: WorkspaceDiagnosticReport) -> Self {
        WorkspaceDiagnosticReportResult::Report(from)
    }
}

impl From<WorkspaceDiagnosticReportPartialResult> for WorkspaceDiagnosticReportResult {
    fn from(from: WorkspaceDiagnosticReportPartialResult) -> Self {
        WorkspaceDiagnosticReportResult::Partial(from)
    }
}
