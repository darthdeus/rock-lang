use crate::{
    LSPAny, Location, OneOf, PartialResultParams, SymbolInformation, SymbolKind,
    SymbolKindCapability, SymbolTag, TagSupport, Uri, WorkDoneProgressParams,
};

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceSymbolClientCapabilities {
    pub dynamic_registration: Option<bool>,
    pub symbol_kind: Option<SymbolKindCapability>,
    pub tag_support: Option<TagSupport<SymbolTag>>,
    pub resolve_support: Option<WorkspaceSymbolResolveSupportCapability>,
}

impl serde::Serialize for WorkspaceSymbolClientCapabilities {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("WorkspaceSymbolClientCapabilities", 4)?;
        if let Some(ref v) = self.dynamic_registration { st.serialize_field("dynamicRegistration", v)?; }
        if let Some(ref v) = self.symbol_kind { st.serialize_field("symbolKind", v)?; }
        if let Some(ref v) = self.tag_support { st.serialize_field("tagSupport", v)?; }
        if let Some(ref v) = self.resolve_support { st.serialize_field("resolveSupport", v)?; }
        st.end()
    }
}

impl<'de> serde::Deserialize<'de> for WorkspaceSymbolClientCapabilities {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let dynamic_registration = map.remove("dynamicRegistration")
            .and_then(|v| serde_json::from_value(v).ok());
        let symbol_kind = map.remove("symbolKind")
            .and_then(|v| serde_json::from_value(v).ok());
        let tag_support = map.remove("tagSupport").and_then(|v| match v {
            serde_json::Value::Bool(false) | serde_json::Value::Null => None,
            serde_json::Value::Bool(true) => Some(TagSupport { value_set: vec![] }),
            other => serde_json::from_value::<TagSupport<SymbolTag>>(other).ok(),
        });
        let resolve_support = map.remove("resolveSupport")
            .and_then(|v| serde_json::from_value(v).ok());
        Ok(WorkspaceSymbolClientCapabilities {
            dynamic_registration,
            symbol_kind,
            tag_support,
            resolve_support,
        })
    }
}

/// The parameters of a Workspace Symbol Request.
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceSymbolParams {
    pub partial_result_params: PartialResultParams,
    pub work_done_progress_params: WorkDoneProgressParams,

    /// A non-empty query string
    pub query: String,
}

impl_lsp_struct!(WorkspaceSymbolParams,
    required { query: String = "query" },
    optional {},
    flatten {
        partial_result_params: PartialResultParams,
        work_done_progress_params: WorkDoneProgressParams,
    }
);

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct WorkspaceSymbolResolveSupportCapability {
    pub properties: Vec<String>,
}

impl_lsp_struct!(WorkspaceSymbolResolveSupportCapability,
    required { properties: Vec<String> = "properties" },
    optional {}
);

/// A special workspace symbol that supports locations without a range
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct WorkspaceSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub tags: Option<Vec<SymbolTag>>,
    pub container_name: Option<String>,
    pub location: OneOf<Location, WorkspaceLocation>,
    pub data: Option<LSPAny>,
}

impl_lsp_struct!(WorkspaceSymbol,
    required {
        name: String = "name",
        kind: SymbolKind = "kind",
        location: OneOf<Location, WorkspaceLocation> = "location",
    },
    optional {
        tags: Vec<SymbolTag> = "tags",
        container_name: String = "containerName",
        data: LSPAny = "data",
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct WorkspaceLocation {
    pub uri: Uri,
}

impl_lsp_struct!(WorkspaceLocation,
    required { uri: Uri = "uri" },
    optional {}
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum WorkspaceSymbolResponse {
    Flat(Vec<SymbolInformation>),
    Nested(Vec<WorkspaceSymbol>),
}

impl_untagged_enum!(WorkspaceSymbolResponse {
    Flat(Vec<SymbolInformation>),
    Nested(Vec<WorkspaceSymbol>),
});
