use crate::{OneOf, Uri};

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceFoldersServerCapabilities {
    /// The server has support for workspace folders
    pub supported: Option<bool>,

    /// Whether the server wants to receive workspace folder change notifications.
    ///
    /// If a string is provided, the string is treated as an ID under which the
    /// notification is registered on the client side. The ID can be used to
    /// unregister for these events using the `client/unregisterCapability` request.
    pub change_notifications: Option<OneOf<bool, String>>,
}

impl_lsp_struct!(WorkspaceFoldersServerCapabilities,
    required {},
    optional {
        supported: bool = "supported",
        change_notifications: OneOf<bool, String> = "changeNotifications",
    }
);

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Clone)]
pub struct WorkspaceFolder {
    /// The associated URI for this workspace folder.
    pub uri: Uri,
    /// The name of the workspace folder. Defaults to the uri's basename.
    pub name: String,
}

impl_lsp_struct!(
    WorkspaceFolder,
    required {
        uri: Uri = "uri",
        name: String = "name",
    },
    optional {}
);
