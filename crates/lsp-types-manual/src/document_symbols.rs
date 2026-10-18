use crate::{
    Location, PartialResultParams, Range, SymbolKind, SymbolKindCapability, TextDocumentIdentifier,
    WorkDoneProgressParams,
};

use crate::{SymbolTag, TagSupport};

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct DocumentSymbolClientCapabilities {
    pub dynamic_registration: Option<bool>,
    pub symbol_kind: Option<SymbolKindCapability>,
    pub hierarchical_document_symbol_support: Option<bool>,
    pub tag_support: Option<TagSupport<SymbolTag>>,
}

impl serde::Serialize for DocumentSymbolClientCapabilities {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("DocumentSymbolClientCapabilities", 4)?;
        if let Some(ref v) = self.dynamic_registration { st.serialize_field("dynamicRegistration", v)?; }
        if let Some(ref v) = self.symbol_kind { st.serialize_field("symbolKind", v)?; }
        if let Some(ref v) = self.hierarchical_document_symbol_support { st.serialize_field("hierarchicalDocumentSymbolSupport", v)?; }
        if let Some(ref v) = self.tag_support { st.serialize_field("tagSupport", v)?; }
        st.end()
    }
}

impl<'de> serde::Deserialize<'de> for DocumentSymbolClientCapabilities {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let dynamic_registration = crate::deserialize_optional(map.remove("dynamicRegistration"))?;
        let symbol_kind = crate::deserialize_optional(map.remove("symbolKind"))?;
        let hierarchical_document_symbol_support =
            crate::deserialize_optional(map.remove("hierarchicalDocumentSymbolSupport"))?;
        let tag_support = match map.remove("tagSupport") {
            Some(value) => TagSupport::<SymbolTag>::deserialize_compat(
                serde::de::IntoDeserializer::into_deserializer(value),
            )
            .map_err(serde::de::Error::custom)?,
            None => None,
        };
        Ok(DocumentSymbolClientCapabilities {
            dynamic_registration,
            symbol_kind,
            hierarchical_document_symbol_support,
            tag_support,
        })
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum DocumentSymbolResponse {
    Flat(Vec<SymbolInformation>),
    Nested(Vec<DocumentSymbol>),
}

impl_untagged_enum!(DocumentSymbolResponse {
    Flat(Vec<SymbolInformation>),
    Nested(Vec<DocumentSymbol>),
});

impl From<Vec<SymbolInformation>> for DocumentSymbolResponse {
    fn from(info: Vec<SymbolInformation>) -> Self {
        DocumentSymbolResponse::Flat(info)
    }
}

impl From<Vec<DocumentSymbol>> for DocumentSymbolResponse {
    fn from(symbols: Vec<DocumentSymbol>) -> Self {
        DocumentSymbolResponse::Nested(symbols)
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentSymbolParams {
    /// The text document.
    pub text_document: TextDocumentIdentifier,

    pub work_done_progress_params: WorkDoneProgressParams,
    pub partial_result_params: PartialResultParams,
}

impl_lsp_struct!(DocumentSymbolParams,
    required { text_document: TextDocumentIdentifier = "textDocument" },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams,
        partial_result_params: PartialResultParams,
    }
);

/// Represents programming constructs like variables, classes, interfaces etc.
/// that appear in a document. Document symbols can be hierarchical and they have two ranges:
/// one that encloses its definition and one that points to its most interesting range,
/// e.g. the range of an identifier.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct DocumentSymbol {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    pub tags: Option<Vec<SymbolTag>>,
    #[deprecated(note = "Use tags instead")]
    pub deprecated: Option<bool>,
    pub range: Range,
    pub selection_range: Range,
    pub children: Option<Vec<DocumentSymbol>>,
}

impl_lsp_struct!(DocumentSymbol,
    required {
        name: String = "name",
        kind: SymbolKind = "kind",
        range: Range = "range",
        selection_range: Range = "selectionRange",
    },
    optional {
        detail: String = "detail",
        tags: Vec<SymbolTag> = "tags",
        deprecated: bool = "deprecated",
        children: Vec<DocumentSymbol> = "children",
    }
);

/// Represents information about programming constructs like variables, classes,
/// interfaces etc.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SymbolInformation {
    pub name: String,
    pub kind: SymbolKind,
    pub tags: Option<Vec<SymbolTag>>,
    #[deprecated(note = "Use tags instead")]
    pub deprecated: Option<bool>,
    pub location: Location,
    pub container_name: Option<String>,
}

impl_lsp_struct!(SymbolInformation,
    required {
        name: String = "name",
        kind: SymbolKind = "kind",
        location: Location = "location",
    },
    optional {
        tags: Vec<SymbolTag> = "tags",
        deprecated: bool = "deprecated",
        container_name: String = "containerName",
    }
);
