use crate::TextDocumentIdentifier;
/// A set of predefined token types. This set is not fixed
/// and clients can specify additional token types via the
/// corresponding client capabilities.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Hash, PartialOrd, Clone)]
pub struct SemanticTokenType(&'static str);

impl serde::Serialize for SemanticTokenType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
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
    pub const FUNCTION: SemanticTokenType = SemanticTokenType::new("function");
    pub const METHOD: SemanticTokenType = SemanticTokenType::new("method");
    pub const MACRO: SemanticTokenType = SemanticTokenType::new("macro");
    pub const KEYWORD: SemanticTokenType = SemanticTokenType::new("keyword");
    pub const COMMENT: SemanticTokenType = SemanticTokenType::new("comment");
    pub const STRING: SemanticTokenType = SemanticTokenType::new("string");
    pub const NUMBER: SemanticTokenType = SemanticTokenType::new("number");
    pub const REGEXP: SemanticTokenType = SemanticTokenType::new("regexp");
    pub const OPERATOR: SemanticTokenType = SemanticTokenType::new("operator");

    /// @since 3.17.0
    pub const DECORATOR: SemanticTokenType = SemanticTokenType::new("decorator");

    pub const fn new(tag: &'static str) -> Self {
        SemanticTokenType(tag)
    }

    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

/// A set of predefined token modifiers. This set is not fixed
/// and clients can specify additional token types via the
/// corresponding client capabilities.
///
/// @since 3.16.0
#[derive(Debug, Eq, PartialEq, Hash, PartialOrd, Clone)]
pub struct SemanticTokenModifier(&'static str);

impl serde::Serialize for SemanticTokenModifier {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
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
        SemanticTokenModifier(tag)
    }

    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

lsp_struct! {
    out
    /// @since 3.16.0
    #[derive(Debug, Eq, PartialEq, Clone, Default)]
    pub struct SemanticTokensLegend {
        required pub token_types: Vec<SemanticTokenType> = "tokenTypes",
        required pub token_modifiers: Vec<SemanticTokenModifier> = "tokenModifiers",
    }
}

/// The actual tokens.
#[derive(Debug, Eq, PartialEq, Copy, Clone, Default)]
pub struct SemanticToken {
    pub delta_line: u32,
    pub delta_start: u32,
    pub length: u32,
    pub token_type: u32,
    pub token_modifiers_bitset: u32,
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

/// Tokens go on the wire as one flat array of integer quintuples.
fn flat(tokens: &[SemanticToken]) -> Vec<u32> {
    tokens
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
        .collect()
}

impl serde::Serialize for SemanticTokens {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("SemanticTokens", 2)?;
        if let Some(ref v) = self.result_id {
            st.serialize_field("resultId", v)?;
        }
        st.serialize_field("data", &flat(&self.data))?;
        st.end()
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
            st.serialize_field("data", &flat(tokens))?;
        }
        st.end()
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum SemanticTokensFullDeltaResult {
    Tokens(SemanticTokens),
    TokensDelta(SemanticTokensDelta),
}

impl_untagged_enum!(SemanticTokensFullDeltaResult {
    Tokens,
    TokensDelta
});

lsp_struct! {
    out
    /// @since 3.16.0
    #[derive(Debug, Eq, PartialEq, Clone, Default)]
    pub struct SemanticTokensDelta {
        optional pub result_id: String = "resultId",
        required pub edits: Vec<SemanticTokensEdit> = "edits",
    }
}

lsp_struct! {
    in
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct SemanticTokensDeltaParams {
        required pub text_document: TextDocumentIdentifier = "textDocument",
        required pub previous_result_id: String = "previousResultId",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::assert_json;

    const TOKENS: [SemanticToken; 2] = [
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
    ];

    #[test]
    fn tokens_serialize_as_flat_quintuples() {
        let tokens = |data: &[SemanticToken]| SemanticTokens {
            result_id: None,
            data: data.to_vec(),
        };
        assert_json(&tokens(&[]), r#"{"data":[]}"#);
        assert_json(&tokens(&TOKENS[..1]), r#"{"data":[2,5,3,0,3]}"#);
        assert_json(&tokens(&TOKENS), r#"{"data":[2,5,3,0,3,0,5,4,1,0]}"#);
    }

    #[test]
    fn edits_serialize_their_tokens_flat() {
        let edit = |data| SemanticTokensEdit {
            start: 0,
            delete_count: 1,
            data,
        };
        assert_json(
            &edit(Some(TOKENS.to_vec())),
            r#"{"start":0,"deleteCount":1,"data":[2,5,3,0,3,0,5,4,1,0]}"#,
        );
        assert_json(&edit(None), r#"{"start":0,"deleteCount":1}"#);
    }
}
