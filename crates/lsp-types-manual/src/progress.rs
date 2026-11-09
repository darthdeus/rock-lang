use crate::NumberOrString;

pub(crate) type ProgressToken = NumberOrString;

/// The progress notification is sent from the server to the client to ask
/// the client to indicate progress.
#[derive(Debug, PartialEq, Clone)]
pub struct ProgressParams {
    /// The progress token provided by the client.
    pub token: ProgressToken,
    /// The progress data.
    pub value: ProgressParamsValue,
}

impl_lsp_struct!(
    ProgressParams,
    required {
        token: ProgressToken = "token",
        value: ProgressParamsValue = "value",
    },
    optional {}
);

#[derive(Debug, PartialEq, Clone)]
pub enum ProgressParamsValue {
    WorkDone(WorkDoneProgress),
}

impl_untagged_enum!(ProgressParamsValue {
    WorkDone(WorkDoneProgress),
});

/// An optional token that a server can use to report work done progress.
#[derive(Debug, Eq, PartialEq, Default, Clone)]
pub struct WorkDoneProgressParams {
    pub work_done_token: Option<ProgressToken>,
}

impl_lsp_struct!(
    WorkDoneProgressParams,
    required {},
    optional {
        work_done_token: ProgressToken = "workDoneToken"
    }
);

#[derive(Debug, PartialEq, Default, Clone)]
pub struct WorkDoneProgressBegin {
    /// Mandatory title of the progress operation. Used to briefly inform about
    /// the kind of operation being performed.
    /// Examples: "Indexing" or "Linking dependencies".
    pub title: String,

    /// Controls if a cancel button should show to allow the user to cancel the
    /// long running operation. Clients that don't support cancellation are allowed
    /// to ignore the setting.
    pub cancellable: Option<bool>,

    /// Optional, more detailed associated progress message. Contains
    /// complementary information to the `title`.
    ///
    /// Examples: "3/25 files", "project/src/module2", "node_modules/some_dep".
    /// If unset, the previous progress message (if any) is still valid.
    pub message: Option<String>,

    /// Optional progress percentage to display (value 100 is considered 100%).
    /// If not provided infinite progress is assumed and clients are allowed to
    /// ignore the `percentage` value in subsequent in report notifications.
    ///
    /// The value should be steadily rising. Clients are free to ignore values
    /// that are not following this rule. The value range is [0, 100].
    pub percentage: Option<u32>,
}

impl_lsp_struct!(
    WorkDoneProgressBegin,
    required {
        title: String = "title"
    },
    optional {
        cancellable: bool = "cancellable",
        message: String = "message",
        percentage: u32 = "percentage",
    }
);

#[derive(Debug, PartialEq, Default, Clone)]
pub struct WorkDoneProgressReport {
    /// Controls if a cancel button should show to allow the user to cancel the
    /// long running operation. Clients that don't support cancellation are allowed
    /// to ignore the setting.
    pub cancellable: Option<bool>,

    /// Optional, more detailed associated progress message. Contains
    /// complementary information to the `title`.
    /// Examples: "3/25 files", "project/src/module2", "node_modules/some_dep".
    /// If unset, the previous progress message (if any) is still valid.
    pub message: Option<String>,

    /// Optional progress percentage to display (value 100 is considered 100%).
    /// If not provided infinite progress is assumed and clients are allowed to
    /// ignore the `percentage` value in subsequent in report notifications.
    ///
    /// The value should be steadily rising. Clients are free to ignore values
    /// that are not following this rule. The value range is [0, 100].
    pub percentage: Option<u32>,
}

impl_lsp_struct!(
    WorkDoneProgressReport,
    required {},
    optional {
        cancellable: bool = "cancellable",
        message: String = "message",
        percentage: u32 = "percentage",
    }
);

#[derive(Debug, PartialEq, Default, Clone)]
pub struct WorkDoneProgressEnd {
    /// Optional, more detailed associated progress message. Contains
    /// complementary information to the `title`.
    /// Examples: "3/25 files", "project/src/module2", "node_modules/some_dep".
    /// If unset, the previous progress message (if any) is still valid.
    pub message: Option<String>,
}

impl_lsp_struct!(
    WorkDoneProgressEnd,
    required {},
    optional {
        message: String = "message"
    }
);

#[derive(Debug, PartialEq, Clone)]
pub enum WorkDoneProgress {
    Begin(WorkDoneProgressBegin),
    Report(WorkDoneProgressReport),
    End(WorkDoneProgressEnd),
}

impl_tagged_enum!(WorkDoneProgress, tag = "kind" {
    Begin(WorkDoneProgressBegin) = "begin",
    Report(WorkDoneProgressReport) = "report",
    End(WorkDoneProgressEnd) = "end",
});
