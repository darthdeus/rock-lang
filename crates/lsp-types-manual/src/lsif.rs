//! Types of Language Server Index Format (LSIF). LSIF is a standard format
//! for language servers or other programming tools to dump their knowledge
//! about a workspace.
//!
//! Based on <https://microsoft.github.io/language-server-protocol/specifications/lsif/0.6.0/specification/>

use crate::{Range, Uri};

pub type Id = crate::NumberOrString;

#[derive(Debug, PartialEq)]
pub enum LocationOrRangeId {
    Location(crate::Location),
    RangeId(Id),
}

impl_untagged_enum!(LocationOrRangeId {
    Location(crate::Location),
    RangeId(Id),
});

#[derive(Debug, PartialEq)]
pub struct Entry {
    pub id: Id,
    pub data: Element,
}

impl_lsp_struct!(Entry,
    required { id: Id = "id" },
    optional {},
    flatten { data: Element }
);

#[derive(Debug, PartialEq)]
pub enum Element {
    Vertex(Vertex),
    Edge(Edge),
}

impl_tagged_enum!(Element, tag = "type" {
    Vertex(Vertex) = "vertex",
    Edge(Edge) = "edge",
});

#[derive(Debug, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub args: Vec<String>,
    pub version: Option<String>,
}

impl serde::Serialize for ToolInfo {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let n = 1 + (!self.args.is_empty()) as usize + self.version.is_some() as usize;
        let mut st = s.serialize_struct("ToolInfo", n)?;
        st.serialize_field("name", &self.name)?;
        if !self.args.is_empty() { st.serialize_field("args", &self.args)?; }
        if let Some(ref v) = self.version { st.serialize_field("version", v)?; }
        st.end()
    }
}

impl<'de> serde::Deserialize<'de> for ToolInfo {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let name = map.remove("name")
            .ok_or_else(|| serde::de::Error::missing_field("name"))
            .and_then(|v| serde_json::from_value::<String>(v).map_err(serde::de::Error::custom))?;
        let args = map.remove("args")
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default();
        let version = map.remove("version").and_then(|v| serde_json::from_value(v).ok());
        Ok(ToolInfo { name, args, version })
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Encoding {
    /// Currently only 'utf-16' is supported due to the limitations in LSP.
    Utf16,
}

impl_str_enum!(Encoding { Utf16 = "utf-16" });

#[derive(Debug, PartialEq)]
pub struct RangeBasedDocumentSymbol {
    pub id: Id,
    pub children: Vec<RangeBasedDocumentSymbol>,
}

impl serde::Serialize for RangeBasedDocumentSymbol {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let n = 1 + (!self.children.is_empty()) as usize;
        let mut st = s.serialize_struct("RangeBasedDocumentSymbol", n)?;
        st.serialize_field("id", &self.id)?;
        if !self.children.is_empty() { st.serialize_field("children", &self.children)?; }
        st.end()
    }
}

impl<'de> serde::Deserialize<'de> for RangeBasedDocumentSymbol {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let id = map.remove("id")
            .ok_or_else(|| serde::de::Error::missing_field("id"))
            .and_then(|v| serde_json::from_value::<Id>(v).map_err(serde::de::Error::custom))?;
        let children = map.remove("children")
            .and_then(|v| serde_json::from_value::<Vec<RangeBasedDocumentSymbol>>(v).ok())
            .unwrap_or_default();
        Ok(RangeBasedDocumentSymbol { id, children })
    }
}

#[derive(Debug, PartialEq)]
pub enum DocumentSymbolOrRangeBasedVec {
    DocumentSymbol(Vec<crate::DocumentSymbol>),
    RangeBased(Vec<RangeBasedDocumentSymbol>),
}

impl_untagged_enum!(DocumentSymbolOrRangeBasedVec {
    DocumentSymbol(Vec<crate::DocumentSymbol>),
    RangeBased(Vec<RangeBasedDocumentSymbol>),
});

#[derive(Debug, PartialEq)]
pub struct DefinitionTag {
    text: String,
    kind: crate::SymbolKind,
    deprecated: bool,
    full_range: Range,
    detail: Option<String>,
}

impl serde::Serialize for DefinitionTag {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let n = 3 + self.deprecated as usize + self.detail.is_some() as usize;
        let mut st = s.serialize_struct("DefinitionTag", n)?;
        st.serialize_field("text", &self.text)?;
        st.serialize_field("kind", &self.kind)?;
        if self.deprecated { st.serialize_field("deprecated", &self.deprecated)?; }
        st.serialize_field("fullRange", &self.full_range)?;
        if let Some(ref v) = self.detail { st.serialize_field("detail", v)?; }
        st.end()
    }
}

impl<'de> serde::Deserialize<'de> for DefinitionTag {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let text = map.remove("text")
            .ok_or_else(|| serde::de::Error::missing_field("text"))
            .and_then(|v| serde_json::from_value::<String>(v).map_err(serde::de::Error::custom))?;
        let kind = map.remove("kind")
            .ok_or_else(|| serde::de::Error::missing_field("kind"))
            .and_then(|v| serde_json::from_value::<crate::SymbolKind>(v).map_err(serde::de::Error::custom))?;
        let deprecated = map.remove("deprecated")
            .and_then(|v| serde_json::from_value::<bool>(v).ok())
            .unwrap_or_default();
        let full_range = map.remove("fullRange")
            .ok_or_else(|| serde::de::Error::missing_field("fullRange"))
            .and_then(|v| serde_json::from_value::<Range>(v).map_err(serde::de::Error::custom))?;
        let detail = map.remove("detail").and_then(|v| serde_json::from_value(v).ok());
        Ok(DefinitionTag { text, kind, deprecated, full_range, detail })
    }
}

#[derive(Debug, PartialEq)]
pub struct DeclarationTag {
    text: String,
    kind: crate::SymbolKind,
    deprecated: bool,
    full_range: Range,
    detail: Option<String>,
}

impl serde::Serialize for DeclarationTag {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let n = 4 + self.detail.is_some() as usize;
        let mut st = s.serialize_struct("DeclarationTag", n)?;
        st.serialize_field("text", &self.text)?;
        st.serialize_field("kind", &self.kind)?;
        st.serialize_field("deprecated", &self.deprecated)?;
        st.serialize_field("fullRange", &self.full_range)?;
        if let Some(ref v) = self.detail { st.serialize_field("detail", v)?; }
        st.end()
    }
}

impl<'de> serde::Deserialize<'de> for DeclarationTag {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let text = map.remove("text")
            .ok_or_else(|| serde::de::Error::missing_field("text"))
            .and_then(|v| serde_json::from_value::<String>(v).map_err(serde::de::Error::custom))?;
        let kind = map.remove("kind")
            .ok_or_else(|| serde::de::Error::missing_field("kind"))
            .and_then(|v| serde_json::from_value::<crate::SymbolKind>(v).map_err(serde::de::Error::custom))?;
        let deprecated = map.remove("deprecated")
            .and_then(|v| serde_json::from_value::<bool>(v).ok())
            .unwrap_or_default();
        let full_range = map.remove("fullRange")
            .ok_or_else(|| serde::de::Error::missing_field("fullRange"))
            .and_then(|v| serde_json::from_value::<Range>(v).map_err(serde::de::Error::custom))?;
        let detail = map.remove("detail").and_then(|v| serde_json::from_value(v).ok());
        Ok(DeclarationTag { text, kind, deprecated, full_range, detail })
    }
}

#[derive(Debug, PartialEq)]
pub struct ReferenceTag {
    text: String,
}

impl_lsp_struct!(ReferenceTag,
    required { text: String = "text" },
    optional {}
);

#[derive(Debug, PartialEq)]
pub struct UnknownTag {
    text: String,
}

impl_lsp_struct!(UnknownTag,
    required { text: String = "text" },
    optional {}
);

#[derive(Debug, PartialEq)]
pub enum RangeTag {
    Definition(DefinitionTag),
    Declaration(DeclarationTag),
    Reference(ReferenceTag),
    Unknown(UnknownTag),
}

impl_tagged_enum!(RangeTag, tag = "type" {
    Definition(DefinitionTag) = "definition",
    Declaration(DeclarationTag) = "declaration",
    Reference(ReferenceTag) = "reference",
    Unknown(UnknownTag) = "unknown",
});

#[derive(Debug, PartialEq)]
pub enum Vertex {
    MetaData(MetaData),
    /// <https://github.com/Microsoft/language-server-protocol/blob/master/indexFormat/specification.md#the-project-vertex>
    Project(Project),
    Document(Document),
    /// <https://github.com/Microsoft/language-server-protocol/blob/master/indexFormat/specification.md#ranges>
    Range {
        range: Range,
        tag: Option<RangeTag>,
    },
    /// <https://github.com/Microsoft/language-server-protocol/blob/master/indexFormat/specification.md#result-set>
    ResultSet(ResultSet),
    Moniker(crate::Moniker),
    PackageInformation(PackageInformation),
    Event(Event),
    DefinitionResult,
    DeclarationResult,
    TypeDefinitionResult,
    ReferenceResult,
    ImplementationResult,
    FoldingRangeResult {
        result: Vec<crate::FoldingRange>,
    },
    HoverResult {
        result: crate::Hover,
    },
    DocumentSymbolResult {
        result: DocumentSymbolOrRangeBasedVec,
    },
    DocumentLinkResult {
        result: Vec<crate::DocumentLink>,
    },
    DiagnosticResult {
        result: Vec<crate::Diagnostic>,
    },
}

impl serde::Serialize for Vertex {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::Error as _;
        let mut map = serde_json::Map::new();
        macro_rules! label {
            ($lbl:expr) => {
                map.insert("label".to_string(), serde_json::Value::String($lbl.to_string()));
            };
        }
        macro_rules! merge {
            ($v:expr) => {
                if let serde_json::Value::Object(inner) =
                    serde_json::to_value($v).map_err(S::Error::custom)?
                {
                    map.extend(inner);
                }
            };
        }
        match self {
            Vertex::MetaData(v) => { label!("metaData"); merge!(v); }
            Vertex::Project(v) => { label!("project"); merge!(v); }
            Vertex::Document(v) => { label!("document"); merge!(v); }
            Vertex::Range { range, tag } => {
                label!("range");
                merge!(range);
                if let Some(t) = tag {
                    map.insert("tag".to_string(), serde_json::to_value(t).map_err(S::Error::custom)?);
                }
            }
            Vertex::ResultSet(v) => { label!("resultSet"); merge!(v); }
            Vertex::Moniker(v) => { label!("moniker"); merge!(v); }
            Vertex::PackageInformation(v) => { label!("packageInformation"); merge!(v); }
            Vertex::Event(v) => { label!("$event"); merge!(v); }
            Vertex::DefinitionResult => { label!("definitionResult"); }
            Vertex::DeclarationResult => { label!("declarationResult"); }
            Vertex::TypeDefinitionResult => { label!("typeDefinitionResult"); }
            Vertex::ReferenceResult => { label!("referenceResult"); }
            Vertex::ImplementationResult => { label!("implementationResult"); }
            Vertex::FoldingRangeResult { result } => {
                label!("foldingRangeResult");
                map.insert("result".to_string(), serde_json::to_value(result).map_err(S::Error::custom)?);
            }
            Vertex::HoverResult { result } => {
                label!("hoverResult");
                map.insert("result".to_string(), serde_json::to_value(result).map_err(S::Error::custom)?);
            }
            Vertex::DocumentSymbolResult { result } => {
                label!("documentSymbolResult");
                map.insert("result".to_string(), serde_json::to_value(result).map_err(S::Error::custom)?);
            }
            Vertex::DocumentLinkResult { result } => {
                label!("documentLinkResult");
                map.insert("result".to_string(), serde_json::to_value(result).map_err(S::Error::custom)?);
            }
            Vertex::DiagnosticResult { result } => {
                label!("diagnosticResult");
                map.insert("result".to_string(), serde_json::to_value(result).map_err(S::Error::custom)?);
            }
        }
        serde_json::Value::Object(map).serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for Vertex {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let label = map
            .get("label")
            .and_then(|v| v.as_str())
            .ok_or_else(|| D::Error::missing_field("label"))?
            .to_owned();
        let val = serde_json::Value::Object(map);
        match label.as_str() {
            "metaData" => Ok(Vertex::MetaData(serde_json::from_value(val).map_err(D::Error::custom)?)),
            "project" => Ok(Vertex::Project(serde_json::from_value(val).map_err(D::Error::custom)?)),
            "document" => Ok(Vertex::Document(serde_json::from_value(val).map_err(D::Error::custom)?)),
            "range" => {
                let range: Range = serde_json::from_value(val.clone()).map_err(D::Error::custom)?;
                let tag: Option<RangeTag> = val
                    .get("tag")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                Ok(Vertex::Range { range, tag })
            }
            "resultSet" => Ok(Vertex::ResultSet(serde_json::from_value(val).map_err(D::Error::custom)?)),
            "moniker" => Ok(Vertex::Moniker(serde_json::from_value(val).map_err(D::Error::custom)?)),
            "packageInformation" => Ok(Vertex::PackageInformation(serde_json::from_value(val).map_err(D::Error::custom)?)),
            "$event" => Ok(Vertex::Event(serde_json::from_value(val).map_err(D::Error::custom)?)),
            "definitionResult" => Ok(Vertex::DefinitionResult),
            "declarationResult" => Ok(Vertex::DeclarationResult),
            "typeDefinitionResult" => Ok(Vertex::TypeDefinitionResult),
            "referenceResult" => Ok(Vertex::ReferenceResult),
            "implementationResult" => Ok(Vertex::ImplementationResult),
            "foldingRangeResult" => {
                let result = val.get("result").ok_or_else(|| D::Error::missing_field("result"))?.clone();
                Ok(Vertex::FoldingRangeResult { result: serde_json::from_value(result).map_err(D::Error::custom)? })
            }
            "hoverResult" => {
                let result = val.get("result").ok_or_else(|| D::Error::missing_field("result"))?.clone();
                Ok(Vertex::HoverResult { result: serde_json::from_value(result).map_err(D::Error::custom)? })
            }
            "documentSymbolResult" => {
                let result = val.get("result").ok_or_else(|| D::Error::missing_field("result"))?.clone();
                Ok(Vertex::DocumentSymbolResult { result: serde_json::from_value(result).map_err(D::Error::custom)? })
            }
            "documentLinkResult" => {
                let result = val.get("result").ok_or_else(|| D::Error::missing_field("result"))?.clone();
                Ok(Vertex::DocumentLinkResult { result: serde_json::from_value(result).map_err(D::Error::custom)? })
            }
            "diagnosticResult" => {
                let result = val.get("result").ok_or_else(|| D::Error::missing_field("result"))?.clone();
                Ok(Vertex::DiagnosticResult { result: serde_json::from_value(result).map_err(D::Error::custom)? })
            }
            _ => Err(D::Error::unknown_variant(
                &label,
                &[
                    "metaData", "project", "document", "range", "resultSet", "moniker",
                    "packageInformation", "$event", "definitionResult", "declarationResult",
                    "typeDefinitionResult", "referenceResult", "implementationResult",
                    "foldingRangeResult", "hoverResult", "documentSymbolResult",
                    "documentLinkResult", "diagnosticResult",
                ],
            )),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum EventKind {
    Begin,
    End,
}

impl_str_enum!(EventKind { Begin = "begin", End = "end" });

#[derive(Debug, PartialEq)]
pub enum EventScope {
    Document,
    Project,
}

impl_str_enum!(EventScope { Document = "document", Project = "project" });

#[derive(Debug, PartialEq)]
pub struct Event {
    pub kind: EventKind,
    pub scope: EventScope,
    pub data: Id,
}

impl_lsp_struct!(Event,
    required {
        kind: EventKind = "kind",
        scope: EventScope = "scope",
        data: Id = "data",
    },
    optional {}
);

#[derive(Debug, PartialEq)]
pub enum Edge {
    Contains(EdgeDataMultiIn),
    Moniker(EdgeData),
    NextMoniker(EdgeData),
    Next(EdgeData),
    PackageInformation(EdgeData),
    Item(Item),
    Definition(EdgeData),
    Declaration(EdgeData),
    Hover(EdgeData),
    References(EdgeData),
    Implementation(EdgeData),
    TypeDefinition(EdgeData),
    FoldingRange(EdgeData),
    DocumentLink(EdgeData),
    DocumentSymbol(EdgeData),
    Diagnostic(EdgeData),
}

impl_tagged_enum!(Edge, tag = "label" {
    Contains(EdgeDataMultiIn) = "contains",
    Moniker(EdgeData) = "moniker",
    NextMoniker(EdgeData) = "nextMoniker",
    Next(EdgeData) = "next",
    PackageInformation(EdgeData) = "packageInformation",
    Item(Item) = "item",
    Definition(EdgeData) = "textDocument/definition",
    Declaration(EdgeData) = "textDocument/declaration",
    Hover(EdgeData) = "textDocument/hover",
    References(EdgeData) = "textDocument/references",
    Implementation(EdgeData) = "textDocument/implementation",
    TypeDefinition(EdgeData) = "textDocument/typeDefinition",
    FoldingRange(EdgeData) = "textDocument/foldingRange",
    DocumentLink(EdgeData) = "textDocument/documentLink",
    DocumentSymbol(EdgeData) = "textDocument/documentSymbol",
    Diagnostic(EdgeData) = "textDocument/diagnostic",
});

#[derive(Debug, PartialEq)]
pub struct EdgeData {
    pub in_v: Id,
    pub out_v: Id,
}

impl_lsp_struct!(EdgeData,
    required {
        in_v: Id = "inV",
        out_v: Id = "outV",
    },
    optional {}
);

#[derive(Debug, PartialEq)]
pub struct EdgeDataMultiIn {
    pub in_vs: Vec<Id>,
    pub out_v: Id,
}

impl_lsp_struct!(EdgeDataMultiIn,
    required {
        in_vs: Vec<Id> = "inVs",
        out_v: Id = "outV",
    },
    optional {}
);

#[derive(Debug, PartialEq)]
pub enum DefinitionResultType {
    Scalar(LocationOrRangeId),
    Array(LocationOrRangeId),
}

impl_untagged_enum!(DefinitionResultType {
    Scalar(LocationOrRangeId),
    Array(LocationOrRangeId),
});

#[derive(Debug, PartialEq)]
pub enum ItemKind {
    Declarations,
    Definitions,
    References,
    ReferenceResults,
    ImplementationResults,
}

impl_str_enum!(ItemKind {
    Declarations = "declarations",
    Definitions = "definitions",
    References = "references",
    ReferenceResults = "referenceResults",
    ImplementationResults = "implementationResults",
});

#[derive(Debug, PartialEq)]
pub struct Item {
    pub document: Id,
    pub property: Option<ItemKind>,
    pub edge_data: EdgeDataMultiIn,
}

impl_lsp_struct!(Item,
    required { document: Id = "document" },
    optional { property: ItemKind = "property" },
    flatten { edge_data: EdgeDataMultiIn }
);

#[derive(Debug, PartialEq)]
pub struct Document {
    pub uri: Uri,
    pub language_id: String,
}

impl_lsp_struct!(Document,
    required {
        uri: Uri = "uri",
        language_id: String = "languageId",
    },
    optional {}
);

#[derive(Debug, PartialEq)]
pub struct ResultSet {
    pub key: Option<String>,
}

impl_lsp_struct!(ResultSet,
    required {},
    optional { key: String = "key" }
);

#[derive(Debug, PartialEq)]
pub struct Project {
    pub resource: Option<Uri>,
    pub content: Option<String>,
    pub kind: String,
}

impl_lsp_struct!(Project,
    required { kind: String = "kind" },
    optional {
        resource: Uri = "resource",
        content: String = "content",
    }
);

#[derive(Debug, PartialEq)]
pub struct MetaData {
    pub version: String,
    pub project_root: Uri,
    pub position_encoding: Encoding,
    pub tool_info: Option<ToolInfo>,
}

impl_lsp_struct!(MetaData,
    required {
        version: String = "version",
        project_root: Uri = "projectRoot",
        position_encoding: Encoding = "positionEncoding",
    },
    optional { tool_info: ToolInfo = "toolInfo" }
);

#[derive(Debug, PartialEq)]
pub struct Repository {
    pub r#type: String,
    pub url: String,
    pub commit_id: Option<String>,
}

impl_lsp_struct!(Repository,
    required {
        r#type: String = "type",
        url: String = "url",
    },
    optional { commit_id: String = "commitId" }
);

#[derive(Debug, PartialEq)]
pub struct PackageInformation {
    pub name: String,
    pub manager: String,
    pub uri: Option<Uri>,
    pub content: Option<String>,
    pub repository: Option<Repository>,
    pub version: Option<String>,
}

impl_lsp_struct!(PackageInformation,
    required {
        name: String = "name",
        manager: String = "manager",
    },
    optional {
        uri: Uri = "uri",
        content: String = "content",
        repository: Repository = "repository",
        version: String = "version",
    }
);
