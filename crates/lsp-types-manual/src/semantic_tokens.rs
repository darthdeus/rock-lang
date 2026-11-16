use std::borrow::Cow;

use serde::Deserialize;

use crate::TextDocumentIdentifier;
/// A set of predefined token types. This set is not fixed
/// and clients can specify additional token types via the
/// corresponding client capabilities.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Hash, PartialOrd, Clone)]
pub struct SemanticTokenType(Cow<'static, str>);

impl serde::Serialize for SemanticTokenType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}
impl<'de> serde::Deserialize<'de> for SemanticTokenType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(SemanticTokenType(Cow::Owned(String::deserialize(d)?)))
    }
}

impl SemanticTokenType {
    pub const NAMESPACE: SemanticTokenType = SemanticTokenType::new("namespace");
    pub const TYPE: SemanticTokenType = SemanticTokenType::new("type");
    pub const CLASS: SemanticTokenType = SemanticTokenType::new("class");
    pub const ENUM: SemanticTokenType = SemanticTokenType::new("enum");
    pub const INTERFACE: SemanticTokenType = SemanticTokenType::new("interface");
    pub const STRUCT: SemanticTokenType = SemanticTokenType::new("struct");
    pub const TYPE_PARAMETER: SemanticTokenType = SemanticTokenType::new("typeParameter");
    pub const PARAMETER: SemanticTokenType = SemanticTokenType::new("parameter");
    pub const VARIABLE: SemanticTokenType = SemanticTokenType::new("variable");
    pub const PROPERTY: SemanticTokenType = SemanticTokenType::new("property");
    pub const ENUM_MEMBER: SemanticTokenType = SemanticTokenType::new("enumMember");
    pub const EVENT: SemanticTokenType = SemanticTokenType::new("event");
    pub const FUNCTION: SemanticTokenType = SemanticTokenType::new("function");
    pub const METHOD: SemanticTokenType = SemanticTokenType::new("method");
    pub const MACRO: SemanticTokenType = SemanticTokenType::new("macro");
    pub const KEYWORD: SemanticTokenType = SemanticTokenType::new("keyword");
    pub const MODIFIER: SemanticTokenType = SemanticTokenType::new("modifier");
    pub const COMMENT: SemanticTokenType = SemanticTokenType::new("comment");
    pub const STRING: SemanticTokenType = SemanticTokenType::new("string");
    pub const NUMBER: SemanticTokenType = SemanticTokenType::new("number");
    pub const REGEXP: SemanticTokenType = SemanticTokenType::new("regexp");
    pub const OPERATOR: SemanticTokenType = SemanticTokenType::new("operator");

    /// @since 3.17.0
    pub const DECORATOR: SemanticTokenType = SemanticTokenType::new("decorator");

    pub const fn new(tag: &'static str) -> Self {
        SemanticTokenType(Cow::Borrowed(tag))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SemanticTokenType {
    fn from(from: String) -> Self {
        SemanticTokenType(Cow::from(from))
    }
}

impl From<&'static str> for SemanticTokenType {
    fn from(from: &'static str) -> Self {
        SemanticTokenType::new(from)
    }
}

/// A set of predefined token modifiers. This set is not fixed
/// and clients can specify additional token types via the
/// corresponding client capabilities.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Hash, PartialOrd, Clone)]
pub struct SemanticTokenModifier(Cow<'static, str>);

impl serde::Serialize for SemanticTokenModifier {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}
impl<'de> serde::Deserialize<'de> for SemanticTokenModifier {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(SemanticTokenModifier(Cow::Owned(String::deserialize(d)?)))
    }
}

impl SemanticTokenModifier {
    pub const DECLARATION: SemanticTokenModifier = SemanticTokenModifier::new("declaration");
    pub const DEFINITION: SemanticTokenModifier = SemanticTokenModifier::new("definition");
    pub const READONLY: SemanticTokenModifier = SemanticTokenModifier::new("readonly");
    pub const STATIC: SemanticTokenModifier = SemanticTokenModifier::new("static");
    pub const DEPRECATED: SemanticTokenModifier = SemanticTokenModifier::new("deprecated");
    pub const ABSTRACT: SemanticTokenModifier = SemanticTokenModifier::new("abstract");
    pub const ASYNC: SemanticTokenModifier = SemanticTokenModifier::new("async");
    pub const MODIFICATION: SemanticTokenModifier = SemanticTokenModifier::new("modification");
    pub const DOCUMENTATION: SemanticTokenModifier = SemanticTokenModifier::new("documentation");
    pub const DEFAULT_LIBRARY: SemanticTokenModifier = SemanticTokenModifier::new("defaultLibrary");

    pub const fn new(tag: &'static str) -> Self {
        SemanticTokenModifier(Cow::Borrowed(tag))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SemanticTokenModifier {
    fn from(from: String) -> Self {
        SemanticTokenModifier(Cow::from(from))
    }
}

impl From<&'static str> for SemanticTokenModifier {
    fn from(from: &'static str) -> Self {
        SemanticTokenModifier::new(from)
    }
}

/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SemanticTokensLegend {
    /// The token types a server uses.
    pub token_types: Vec<SemanticTokenType>,

    /// The token modifiers a server uses.
    pub token_modifiers: Vec<SemanticTokenModifier>,
}

impl_lsp_struct!(SemanticTokensLegend,
    required {
        token_types: Vec<SemanticTokenType> = "tokenTypes",
        token_modifiers: Vec<SemanticTokenModifier> = "tokenModifiers",
    },
    optional {}
);

/// The actual tokens.
#[derive(Debug, Eq, PartialEq, Copy, Clone, Default)]
pub struct SemanticToken {
    pub delta_line: u32,
    pub delta_start: u32,
    pub length: u32,
    pub token_type: u32,
    pub token_modifiers_bitset: u32,
}

impl SemanticToken {
    fn deserialize_tokens<'de, D>(deserializer: D) -> Result<Vec<SemanticToken>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let data = Vec::<u32>::deserialize(deserializer)?;
        let (chunks, remainder) = data.as_chunks::<5>();

        if !remainder.is_empty() {
            return Result::Err(serde::de::Error::custom("Length is not divisible by 5"));
        }

        Result::Ok(
            chunks
                .iter()
                .map(|chunk| SemanticToken {
                    delta_line: chunk[0],
                    delta_start: chunk[1],
                    length: chunk[2],
                    token_type: chunk[3],
                    token_modifiers_bitset: chunk[4],
                })
                .collect(),
        )
    }
}

/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SemanticTokens {
    /// An optional result id. If provided and clients support delta updating
    /// the client will include the result id in the next semantic token request.
    /// A server can then instead of computing all semantic tokens again simply
    /// send a delta.
    pub result_id: Option<String>,

    /// The actual tokens.
    pub data: Vec<SemanticToken>,
}

impl serde::Serialize for SemanticTokens {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let flat: Vec<u32> = self
            .data
            .iter()
            .flat_map(|t| {
                [
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    t.token_type,
                    t.token_modifiers_bitset,
                ]
            })
            .collect();
        let mut st = s.serialize_struct("SemanticTokens", 2)?;
        if let Some(ref v) = self.result_id {
            st.serialize_field("resultId", v)?;
        }
        st.serialize_field("data", &flat)?;
        st.end()
    }
}

impl<'de> serde::Deserialize<'de> for SemanticTokens {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let result_id = crate::deserialize_optional(map.remove("resultId"))?;
        let data_val = map
            .remove("data")
            .ok_or_else(|| serde::de::Error::missing_field("data"))?;
        let data = SemanticToken::deserialize_tokens(data_val).map_err(serde::de::Error::custom)?;
        Ok(SemanticTokens { result_id, data })
    }
}

/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SemanticTokensEdit {
    pub start: u32,
    pub delete_count: u32,
    pub data: Option<Vec<SemanticToken>>,
}

impl serde::Serialize for SemanticTokensEdit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let n = if self.data.is_some() { 3 } else { 2 };
        let mut st = s.serialize_struct("SemanticTokensEdit", n)?;
        st.serialize_field("start", &self.start)?;
        st.serialize_field("deleteCount", &self.delete_count)?;
        if let Some(ref tokens) = self.data {
            let flat: Vec<u32> = tokens
                .iter()
                .flat_map(|t| {
                    [
                        t.delta_line,
                        t.delta_start,
                        t.length,
                        t.token_type,
                        t.token_modifiers_bitset,
                    ]
                })
                .collect();
            st.serialize_field("data", &flat)?;
        }
        st.end()
    }
}

impl<'de> serde::Deserialize<'de> for SemanticTokensEdit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let start = map
            .remove("start")
            .ok_or_else(|| serde::de::Error::missing_field("start"))
            .and_then(|v| serde_json::from_value::<u32>(v).map_err(serde::de::Error::custom))?;
        let delete_count = map
            .remove("deleteCount")
            .ok_or_else(|| serde::de::Error::missing_field("deleteCount"))
            .and_then(|v| serde_json::from_value::<u32>(v).map_err(serde::de::Error::custom))?;
        let data = match map.remove("data") {
            None => None,
            Some(v) => {
                Some(SemanticToken::deserialize_tokens(v).map_err(serde::de::Error::custom)?)
            }
        };
        Ok(SemanticTokensEdit {
            start,
            delete_count,
            data,
        })
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum SemanticTokensFullDeltaResult {
    Tokens(SemanticTokens),
    TokensDelta(SemanticTokensDelta),
}

impl serde::Serialize for SemanticTokensFullDeltaResult {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Tokens(v) => v.serialize(s),
            Self::TokensDelta(v) => v.serialize(s),
        }
    }
}

impl<'de> serde::Deserialize<'de> for SemanticTokensFullDeltaResult {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let val = serde_json::Value::deserialize(d)?;
        if let Ok(v) = serde_json::from_value::<SemanticTokens>(val.clone()) {
            return Ok(Self::Tokens(v));
        }
        if let Ok(v) = serde_json::from_value::<SemanticTokensDelta>(val.clone()) {
            return Ok(Self::TokensDelta(v));
        }
        Err(serde::de::Error::custom(
            "data did not match any variant of SemanticTokensFullDeltaResult",
        ))
    }
}

impl From<SemanticTokens> for SemanticTokensFullDeltaResult {
    fn from(from: SemanticTokens) -> Self {
        SemanticTokensFullDeltaResult::Tokens(from)
    }
}

impl From<SemanticTokensDelta> for SemanticTokensFullDeltaResult {
    fn from(from: SemanticTokensDelta) -> Self {
        SemanticTokensFullDeltaResult::TokensDelta(from)
    }
}

/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct SemanticTokensDelta {
    pub result_id: Option<String>,
    /// For a detailed description how these edits are structured please see
    /// <https://github.com/microsoft/vscode-extension-samples/blob/5ae1f7787122812dcc84e37427ca90af5ee09f14/semantic-tokens-sample/vscode.proposed.d.ts#L131>
    pub edits: Vec<SemanticTokensEdit>,
}

impl_lsp_struct!(SemanticTokensDelta,
    required { edits: Vec<SemanticTokensEdit> = "edits" },
    optional { result_id: String = "resultId" }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SemanticTokensDeltaParams {
    /// The text document.
    pub text_document: TextDocumentIdentifier,

    /// The result id of a previous response.
    pub previous_result_id: String,
}

impl_lsp_struct!(
    SemanticTokensDeltaParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        previous_result_id: String = "previousResultId",
    },
    optional {}
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{test_deserialization, test_serialization};

    #[test]
    fn test_semantic_tokens_support_serialization() {
        test_serialization(
            &SemanticTokens {
                result_id: None,
                data: vec![],
            },
            r#"{"data":[]}"#,
        );

        test_serialization(
            &SemanticTokens {
                result_id: None,
                data: vec![SemanticToken {
                    delta_line: 2,
                    delta_start: 5,
                    length: 3,
                    token_type: 0,
                    token_modifiers_bitset: 3,
                }],
            },
            r#"{"data":[2,5,3,0,3]}"#,
        );

        test_serialization(
            &SemanticTokens {
                result_id: None,
                data: vec![
                    SemanticToken {
                        delta_line: 2,
                        delta_start: 5,
                        length: 3,
                        token_type: 0,
                        token_modifiers_bitset: 3,
                    },
                    SemanticToken {
                        delta_line: 0,
                        delta_start: 5,
                        length: 4,
                        token_type: 1,
                        token_modifiers_bitset: 0,
                    },
                ],
            },
            r#"{"data":[2,5,3,0,3,0,5,4,1,0]}"#,
        );
    }

    #[test]
    fn test_semantic_tokens_support_deserialization() {
        test_deserialization(
            r#"{"data":[]}"#,
            &SemanticTokens {
                result_id: None,
                data: vec![],
            },
        );

        test_deserialization(
            r#"{"data":[2,5,3,0,3]}"#,
            &SemanticTokens {
                result_id: None,
                data: vec![SemanticToken {
                    delta_line: 2,
                    delta_start: 5,
                    length: 3,
                    token_type: 0,
                    token_modifiers_bitset: 3,
                }],
            },
        );

        test_deserialization(
            r#"{"data":[2,5,3,0,3,0,5,4,1,0]}"#,
            &SemanticTokens {
                result_id: None,
                data: vec![
                    SemanticToken {
                        delta_line: 2,
                        delta_start: 5,
                        length: 3,
                        token_type: 0,
                        token_modifiers_bitset: 3,
                    },
                    SemanticToken {
                        delta_line: 0,
                        delta_start: 5,
                        length: 4,
                        token_type: 1,
                        token_modifiers_bitset: 0,
                    },
                ],
            },
        );
    }

    #[test]
    #[should_panic]
    fn test_semantic_tokens_support_deserialization_err() {
        test_deserialization(
            r#"{"data":[1]}"#,
            &SemanticTokens {
                result_id: None,
                data: vec![],
            },
        );
    }

    #[test]
    fn test_semantic_tokens_edit_support_deserialization() {
        test_deserialization(
            r#"{"start":0,"deleteCount":1,"data":[2,5,3,0,3,0,5,4,1,0]}"#,
            &SemanticTokensEdit {
                start: 0,
                delete_count: 1,
                data: Some(vec![
                    SemanticToken {
                        delta_line: 2,
                        delta_start: 5,
                        length: 3,
                        token_type: 0,
                        token_modifiers_bitset: 3,
                    },
                    SemanticToken {
                        delta_line: 0,
                        delta_start: 5,
                        length: 4,
                        token_type: 1,
                        token_modifiers_bitset: 0,
                    },
                ]),
            },
        );

        test_deserialization(
            r#"{"start":0,"deleteCount":1}"#,
            &SemanticTokensEdit {
                start: 0,
                delete_count: 1,
                data: None,
            },
        );
    }

    #[test]
    fn test_semantic_tokens_edit_support_serialization() {
        test_serialization(
            &SemanticTokensEdit {
                start: 0,
                delete_count: 1,
                data: Some(vec![
                    SemanticToken {
                        delta_line: 2,
                        delta_start: 5,
                        length: 3,
                        token_type: 0,
                        token_modifiers_bitset: 3,
                    },
                    SemanticToken {
                        delta_line: 0,
                        delta_start: 5,
                        length: 4,
                        token_type: 1,
                        token_modifiers_bitset: 0,
                    },
                ]),
            },
            r#"{"start":0,"deleteCount":1,"data":[2,5,3,0,3,0,5,4,1,0]}"#,
        );

        test_serialization(
            &SemanticTokensEdit {
                start: 0,
                delete_count: 1,
                data: None,
            },
            r#"{"start":0,"deleteCount":1}"#,
        );
    }
}
