use crate::{LSPObject, Uri};

pub use notification_params::*;

/// A notebook document.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct NotebookDocument {
    pub uri: Uri,
    pub notebook_type: String,
    pub version: i32,
    pub metadata: Option<LSPObject>,
    pub cells: Vec<NotebookCell>,
}

impl_lsp_struct!(NotebookDocument,
    required {
        uri: Uri = "uri",
        notebook_type: String = "notebookType",
        version: i32 = "version",
        cells: Vec<NotebookCell> = "cells",
    },
    optional { metadata: LSPObject = "metadata" }
);

/// A notebook cell.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct NotebookCell {
    pub kind: NotebookCellKind,
    pub document: Uri,
    pub metadata: Option<LSPObject>,
    pub execution_summary: Option<ExecutionSummary>,
}

impl_lsp_struct!(NotebookCell,
    required {
        kind: NotebookCellKind = "kind",
        document: Uri = "document",
    },
    optional {
        metadata: LSPObject = "metadata",
        execution_summary: ExecutionSummary = "executionSummary",
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct ExecutionSummary {
    pub execution_order: u32,
    pub success: Option<bool>,
}

impl_lsp_struct!(ExecutionSummary,
    required { execution_order: u32 = "executionOrder" },
    optional { success: bool = "success" }
);

#[derive(Debug, Eq, PartialEq, Clone)]
#[repr(u8)]
pub enum NotebookCellKind {
    /// A markup-cell is formatted source that is used for display.
    Markup = 1,
    /// A code-cell is source code.
    Code = 2,
}

impl serde::Serialize for NotebookCellKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(self.clone() as u8)
    }
}

impl<'de> serde::Deserialize<'de> for NotebookCellKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match u8::deserialize(d)? {
            1 => Ok(NotebookCellKind::Markup),
            2 => Ok(NotebookCellKind::Code),
            v => Err(serde::de::Error::custom(format!("unknown NotebookCellKind: {v}"))),
        }
    }
}

/// Capabilities specific to the notebook document support.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct NotebookDocumentClientCapabilities {
    pub synchronization: NotebookDocumentSyncClientCapabilities,
}

impl_lsp_struct!(NotebookDocumentClientCapabilities,
    required { synchronization: NotebookDocumentSyncClientCapabilities = "synchronization" },
    optional {}
);

/// Notebook specific client capabilities.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct NotebookDocumentSyncClientCapabilities {
    pub dynamic_registration: Option<bool>,
    pub execution_summary_report: Option<bool>,
}

impl_lsp_struct!(NotebookDocumentSyncClientCapabilities,
    required {},
    optional {
        dynamic_registration: bool = "dynamicRegistration",
        execution_summary_report: bool = "executionSummaryReport",
    }
);

/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct NotebookDocumentSyncOptions {
    pub notebook_selector: Vec<NotebookSelector>,
    pub save: Option<bool>,
}

impl_lsp_struct!(NotebookDocumentSyncOptions,
    required { notebook_selector: Vec<NotebookSelector> = "notebookSelector" },
    optional { save: bool = "save" }
);

/// Registration options specific to a notebook.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct NotebookDocumentSyncRegistrationOptions {
    pub notebook_selector: Vec<NotebookSelector>,
    pub save: Option<bool>,
    pub id: Option<String>,
}

impl_lsp_struct!(NotebookDocumentSyncRegistrationOptions,
    required { notebook_selector: Vec<NotebookSelector> = "notebookSelector" },
    optional {
        save: bool = "save",
        id: String = "id",
    }
);

/// A notebook cell text document filter denotes a cell text
/// document by different properties.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct NotebookCellTextDocumentFilter {
    pub notebook: Notebook,
    pub language: Option<String>,
}

impl_lsp_struct!(NotebookCellTextDocumentFilter,
    required { notebook: Notebook = "notebook" },
    optional { language: String = "language" }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum NotebookSelector {
    ByNotebook {
        notebook: Notebook,
        cells: Option<Vec<NotebookCellSelector>>,
    },
    ByCells {
        notebook: Option<Notebook>,
        cells: Vec<NotebookCellSelector>,
    },
}

impl serde::Serialize for NotebookSelector {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self {
            NotebookSelector::ByNotebook { notebook, cells } => {
                let n = if cells.is_some() { 2 } else { 1 };
                let mut st = s.serialize_struct("ByNotebook", n)?;
                st.serialize_field("notebook", notebook)?;
                if let Some(c) = cells { st.serialize_field("cells", c)?; }
                st.end()
            }
            NotebookSelector::ByCells { notebook, cells } => {
                let n = if notebook.is_some() { 2 } else { 1 };
                let mut st = s.serialize_struct("ByCells", n)?;
                if let Some(nb) = notebook { st.serialize_field("notebook", nb)?; }
                st.serialize_field("cells", cells)?;
                st.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for NotebookSelector {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let notebook = map.remove("notebook").and_then(|v| serde_json::from_value::<Notebook>(v).ok());
        let cells = map.remove("cells").and_then(|v| serde_json::from_value::<Vec<NotebookCellSelector>>(v).ok());
        if let Some(notebook) = notebook {
            Ok(NotebookSelector::ByNotebook { notebook, cells })
        } else {
            let cells = cells.ok_or_else(|| serde::de::Error::custom("NotebookSelector requires notebook or cells"))?;
            Ok(NotebookSelector::ByCells { notebook: None, cells })
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct NotebookCellSelector {
    pub language: String,
}

impl_lsp_struct!(NotebookCellSelector,
    required { language: String = "language" },
    optional {}
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum Notebook {
    String(String),
    NotebookDocumentFilter(NotebookDocumentFilter),
}

impl_untagged_enum!(Notebook {
    NotebookDocumentFilter(NotebookDocumentFilter),
    String(String),
});

/// A notebook document filter denotes a notebook document by
/// different properties.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum NotebookDocumentFilter {
    ByType {
        notebook_type: String,
        scheme: Option<String>,
        pattern: Option<String>,
    },
    ByScheme {
        notebook_type: Option<String>,
        scheme: String,
        pattern: Option<String>,
    },
    ByPattern {
        notebook_type: Option<String>,
        scheme: Option<String>,
        pattern: String,
    },
}

impl serde::Serialize for NotebookDocumentFilter {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self {
            NotebookDocumentFilter::ByType { notebook_type, scheme, pattern } => {
                let n = 1 + scheme.is_some() as usize + pattern.is_some() as usize;
                let mut st = s.serialize_struct("ByType", n)?;
                st.serialize_field("notebookType", notebook_type)?;
                if let Some(v) = scheme { st.serialize_field("scheme", v)?; }
                if let Some(v) = pattern { st.serialize_field("pattern", v)?; }
                st.end()
            }
            NotebookDocumentFilter::ByScheme { notebook_type, scheme, pattern } => {
                let n = 1 + notebook_type.is_some() as usize + pattern.is_some() as usize;
                let mut st = s.serialize_struct("ByScheme", n)?;
                if let Some(v) = notebook_type { st.serialize_field("notebookType", v)?; }
                st.serialize_field("scheme", scheme)?;
                if let Some(v) = pattern { st.serialize_field("pattern", v)?; }
                st.end()
            }
            NotebookDocumentFilter::ByPattern { notebook_type, scheme, pattern } => {
                let n = 1 + notebook_type.is_some() as usize + scheme.is_some() as usize;
                let mut st = s.serialize_struct("ByPattern", n)?;
                if let Some(v) = notebook_type { st.serialize_field("notebookType", v)?; }
                if let Some(v) = scheme { st.serialize_field("scheme", v)?; }
                st.serialize_field("pattern", pattern)?;
                st.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for NotebookDocumentFilter {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let notebook_type = map.remove("notebookType").and_then(|v| serde_json::from_value::<String>(v).ok());
        let scheme = map.remove("scheme").and_then(|v| serde_json::from_value::<String>(v).ok());
        let pattern = map.remove("pattern").and_then(|v| serde_json::from_value::<String>(v).ok());
        if let Some(nt) = notebook_type {
            Ok(NotebookDocumentFilter::ByType { notebook_type: nt, scheme, pattern })
        } else if let Some(s) = scheme {
            Ok(NotebookDocumentFilter::ByScheme { notebook_type: None, scheme: s, pattern })
        } else if let Some(p) = pattern {
            Ok(NotebookDocumentFilter::ByPattern { notebook_type: None, scheme: None, pattern: p })
        } else {
            Err(serde::de::Error::custom("NotebookDocumentFilter requires notebookType, scheme, or pattern"))
        }
    }
}

mod notification_params {
    use crate::{
        TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
        VersionedTextDocumentIdentifier,
    };

    use super::*;

    /// @since 3.17.0
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct DidOpenNotebookDocumentParams {
        pub notebook_document: NotebookDocument,
        pub cell_text_documents: Vec<TextDocumentItem>,
    }

    impl_lsp_struct!(DidOpenNotebookDocumentParams,
        required {
            notebook_document: NotebookDocument = "notebookDocument",
            cell_text_documents: Vec<TextDocumentItem> = "cellTextDocuments",
        },
        optional {}
    );

    /// @since 3.17.0
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct DidChangeNotebookDocumentParams {
        pub notebook_document: VersionedNotebookDocumentIdentifier,
        pub change: NotebookDocumentChangeEvent,
    }

    impl_lsp_struct!(DidChangeNotebookDocumentParams,
        required {
            notebook_document: VersionedNotebookDocumentIdentifier = "notebookDocument",
            change: NotebookDocumentChangeEvent = "change",
        },
        optional {}
    );

    /// @since 3.17.0
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct VersionedNotebookDocumentIdentifier {
        pub version: i32,
        pub uri: Uri,
    }

    impl_lsp_struct!(VersionedNotebookDocumentIdentifier,
        required {
            version: i32 = "version",
            uri: Uri = "uri",
        },
        optional {}
    );

    /// @since 3.17.0
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct NotebookDocumentChangeEvent {
        pub metadata: Option<LSPObject>,
        pub cells: Option<NotebookDocumentCellChange>,
    }

    impl_lsp_struct!(NotebookDocumentChangeEvent,
        required {},
        optional {
            metadata: LSPObject = "metadata",
            cells: NotebookDocumentCellChange = "cells",
        }
    );

    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct NotebookDocumentCellChange {
        pub structure: Option<NotebookDocumentCellChangeStructure>,
        pub data: Option<Vec<NotebookCell>>,
        pub text_content: Option<Vec<NotebookDocumentChangeTextContent>>,
    }

    impl_lsp_struct!(NotebookDocumentCellChange,
        required {},
        optional {
            structure: NotebookDocumentCellChangeStructure = "structure",
            data: Vec<NotebookCell> = "data",
            text_content: Vec<NotebookDocumentChangeTextContent> = "textContent",
        }
    );

    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct NotebookDocumentChangeTextContent {
        pub document: VersionedTextDocumentIdentifier,
        pub changes: Vec<TextDocumentContentChangeEvent>,
    }

    impl_lsp_struct!(NotebookDocumentChangeTextContent,
        required {
            document: VersionedTextDocumentIdentifier = "document",
            changes: Vec<TextDocumentContentChangeEvent> = "changes",
        },
        optional {}
    );

    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct NotebookDocumentCellChangeStructure {
        pub array: NotebookCellArrayChange,
        pub did_open: Option<Vec<TextDocumentItem>>,
        pub did_close: Option<Vec<TextDocumentIdentifier>>,
    }

    impl_lsp_struct!(NotebookDocumentCellChangeStructure,
        required { array: NotebookCellArrayChange = "array" },
        optional {
            did_open: Vec<TextDocumentItem> = "didOpen",
            did_close: Vec<TextDocumentIdentifier> = "didClose",
        }
    );

    /// @since 3.17.0
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct NotebookCellArrayChange {
        pub start: u32,
        pub delete_count: u32,
        pub cells: Option<Vec<NotebookCell>>,
    }

    impl_lsp_struct!(NotebookCellArrayChange,
        required {
            start: u32 = "start",
            delete_count: u32 = "deleteCount",
        },
        optional { cells: Vec<NotebookCell> = "cells" }
    );

    /// @since 3.17.0
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct DidSaveNotebookDocumentParams {
        pub notebook_document: NotebookDocumentIdentifier,
    }

    impl_lsp_struct!(DidSaveNotebookDocumentParams,
        required { notebook_document: NotebookDocumentIdentifier = "notebookDocument" },
        optional {}
    );

    /// @since 3.17.0
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct NotebookDocumentIdentifier {
        pub uri: Uri,
    }

    impl_lsp_struct!(NotebookDocumentIdentifier,
        required { uri: Uri = "uri" },
        optional {}
    );

    /// @since 3.17.0
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct DidCloseNotebookDocumentParams {
        pub notebook_document: NotebookDocumentIdentifier,
        pub cell_text_documents: Vec<TextDocumentIdentifier>,
    }

    impl_lsp_struct!(DidCloseNotebookDocumentParams,
        required {
            notebook_document: NotebookDocumentIdentifier = "notebookDocument",
            cell_text_documents: Vec<TextDocumentIdentifier> = "cellTextDocuments",
        },
        optional {}
    );
}
