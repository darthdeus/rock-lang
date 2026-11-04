#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceFileOperationsClientCapabilities {
    /// Whether the client supports dynamic registration for file requests/notifications.
    pub dynamic_registration: Option<bool>,
    /// The client has support for sending didCreateFiles notifications.
    pub did_create: Option<bool>,
    /// The server is interested in receiving willCreateFiles requests.
    pub will_create: Option<bool>,
    /// The server is interested in receiving didRenameFiles requests.
    pub did_rename: Option<bool>,
    /// The server is interested in receiving willRenameFiles requests.
    pub will_rename: Option<bool>,
    /// The server is interested in receiving didDeleteFiles requests.
    pub did_delete: Option<bool>,
    /// The server is interested in receiving willDeleteFiles requests.
    pub will_delete: Option<bool>,
}

impl_lsp_struct!(
    WorkspaceFileOperationsClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        did_create: bool = "didCreate",
        will_create: bool = "willCreate",
        did_rename: bool = "didRename",
        will_rename: bool = "willRename",
        did_delete: bool = "didDelete",
        will_delete: bool = "willDelete",
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceFileOperationsServerCapabilities {
    /// The server is interested in receiving didCreateFiles notifications.
    pub did_create: Option<FileOperationRegistrationOptions>,
    /// The server is interested in receiving willCreateFiles requests.
    pub will_create: Option<FileOperationRegistrationOptions>,
    /// The server is interested in receiving didRenameFiles notifications.
    pub did_rename: Option<FileOperationRegistrationOptions>,
    /// The server is interested in receiving willRenameFiles requests.
    pub will_rename: Option<FileOperationRegistrationOptions>,
    /// The server is interested in receiving didDeleteFiles file notifications.
    pub did_delete: Option<FileOperationRegistrationOptions>,
    /// The server is interested in receiving willDeleteFiles file requests.
    pub will_delete: Option<FileOperationRegistrationOptions>,
}

impl_lsp_struct!(
    WorkspaceFileOperationsServerCapabilities,
    required {},
    optional {
        did_create: FileOperationRegistrationOptions = "didCreate",
        will_create: FileOperationRegistrationOptions = "willCreate",
        did_rename: FileOperationRegistrationOptions = "didRename",
        will_rename: FileOperationRegistrationOptions = "willRename",
        did_delete: FileOperationRegistrationOptions = "didDelete",
        will_delete: FileOperationRegistrationOptions = "willDelete",
    }
);

/// The options to register for file operations.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FileOperationRegistrationOptions {
    /// The actual filters.
    pub filters: Vec<FileOperationFilter>,
}

impl_lsp_struct!(FileOperationRegistrationOptions,
    required { filters: Vec<FileOperationFilter> = "filters" },
    optional {}
);

/// A filter to describe in which file operation requests or notifications
/// the server is interested in.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FileOperationFilter {
    /// A Uri like `file` or `untitled`.
    pub scheme: Option<String>,
    /// The actual file operation pattern.
    pub pattern: FileOperationPattern,
}

impl_lsp_struct!(
    FileOperationFilter,
    required {
        pattern: FileOperationPattern = "pattern"
    },
    nullable {
        scheme: String = "scheme"
    },
    optional {}
);

/// A pattern kind describing if a glob pattern matches a file a folder or both.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum FileOperationPatternKind {
    /// The pattern matches a file only.
    File,
    /// The pattern matches a folder only.
    Folder,
}

impl_str_enum!(FileOperationPatternKind { File = "file", Folder = "folder" });

/// Matching options for the file operation pattern.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FileOperationPatternOptions {
    /// The pattern should be matched ignoring casing.
    pub ignore_case: Option<bool>,
}

impl_lsp_struct!(
    FileOperationPatternOptions,
    required {},
    optional {
        ignore_case: bool = "ignoreCase"
    }
);

/// A pattern to describe in which file operation requests or notifications
/// the server is interested in.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FileOperationPattern {
    /// The glob pattern to match. Glob patterns can have the following syntax:
    /// - `*` to match one or more characters in a path segment
    /// - `?` to match on one character in a path segment
    /// - `**` to match any number of path segments, including none
    /// - `{}` to group conditions (e.g. `**​/*.{ts,js}` matches all TypeScript
    ///   and JavaScript files)
    /// - `[]` to declare a range of characters to match in a path segment
    ///   (e.g., `example.[0-9]` to match on `example.0`, `example.1`, …)
    /// - `[!...]` to negate a range of characters to match in a path segment
    ///   (e.g., `example.[!0-9]` to match on `example.a`, `example.b`, but
    ///   not `example.0`)
    pub glob: String,
    /// Whether to match files or folders with this pattern. Matches both if undefined.
    pub matches: Option<FileOperationPatternKind>,
    /// Additional options used during matching.
    pub options: Option<FileOperationPatternOptions>,
}

impl_lsp_struct!(
    FileOperationPattern,
    required {
        glob: String = "glob"
    },
    optional {
        matches: FileOperationPatternKind = "matches",
        options: FileOperationPatternOptions = "options",
    }
);

/// The parameters sent in notifications/requests for user-initiated creation of files.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct CreateFilesParams {
    /// An array of all files/folders created in this operation.
    pub files: Vec<FileCreate>,
}

impl_lsp_struct!(CreateFilesParams,
    required { files: Vec<FileCreate> = "files" },
    optional {}
);

/// Represents information on a file/folder create.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FileCreate {
    /// A file:// URI for the location of the file/folder being created.
    pub uri: String,
}

impl_lsp_struct!(
    FileCreate,
    required {
        uri: String = "uri"
    },
    optional {}
);

/// The parameters sent in notifications/requests for user-initiated renames of files.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct RenameFilesParams {
    /// An array of all files/folders renamed in this operation. When a folder
    /// is renamed, only the folder will be included, and not its children.
    pub files: Vec<FileRename>,
}

impl_lsp_struct!(RenameFilesParams,
    required { files: Vec<FileRename> = "files" },
    optional {}
);

/// Represents information on a file/folder rename.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FileRename {
    /// A file:// URI for the original location of the file/folder being renamed.
    pub old_uri: String,
    /// A file:// URI for the new location of the file/folder being renamed.
    pub new_uri: String,
}

impl_lsp_struct!(
    FileRename,
    required {
        old_uri: String = "oldUri",
        new_uri: String = "newUri",
    },
    optional {}
);

/// The parameters sent in notifications/requests for user-initiated deletes of files.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct DeleteFilesParams {
    /// An array of all files/folders deleted in this operation.
    pub files: Vec<FileDelete>,
}

impl_lsp_struct!(DeleteFilesParams,
    required { files: Vec<FileDelete> = "files" },
    optional {}
);

/// Represents information on a file/folder delete.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct FileDelete {
    /// A file:// URI for the location of the file/folder being deleted.
    pub uri: String,
}

impl_lsp_struct!(
    FileDelete,
    required {
        uri: String = "uri"
    },
    optional {}
);
