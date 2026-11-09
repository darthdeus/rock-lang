use crate::{DocumentSelector, DynamicRegistrationClientCapabilities};

use std::collections::HashMap;

pub type DocumentFormattingClientCapabilities = DynamicRegistrationClientCapabilities;
pub type DocumentRangeFormattingClientCapabilities = DynamicRegistrationClientCapabilities;
pub type DocumentOnTypeFormattingClientCapabilities = DynamicRegistrationClientCapabilities;

/// Format document on type options
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct DocumentOnTypeFormattingOptions {
    /// A character on which formatting should be triggered, like `}`.
    pub first_trigger_character: String,

    /// More trigger characters.
    pub more_trigger_character: Option<Vec<String>>,
}

impl_lsp_struct!(DocumentOnTypeFormattingOptions,
    required { first_trigger_character: String = "firstTriggerCharacter" },
    optional { more_trigger_character: Vec<String> = "moreTriggerCharacter" }
);

/// Value-object describing what options formatting should use.
///
/// The `properties` field captures any extra keys not listed here (the LSP
/// "Signature for further properties" extension point).
#[derive(Debug, PartialEq, Clone, Default)]
pub struct FormattingOptions {
    /// Size of a tab in spaces.
    pub tab_size: u32,

    /// Prefer spaces over tabs.
    pub insert_spaces: bool,

    /// Signature for further properties.
    pub properties: HashMap<String, FormattingProperty>,

    /// Trim trailing whitespace on a line.
    pub trim_trailing_whitespace: Option<bool>,

    /// Insert a newline character at the end of the file if one does not exist.
    pub insert_final_newline: Option<bool>,

    /// Trim all newlines after the final newline at the end of the file.
    pub trim_final_newlines: Option<bool>,
}

impl serde::Serialize for FormattingOptions {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = serde_json::Map::new();
        map.insert(
            "tabSize".to_string(),
            serde_json::to_value(self.tab_size).map_err(serde::ser::Error::custom)?,
        );
        map.insert(
            "insertSpaces".to_string(),
            serde_json::to_value(self.insert_spaces).map_err(serde::ser::Error::custom)?,
        );
        if let Some(v) = &self.trim_trailing_whitespace {
            map.insert(
                "trimTrailingWhitespace".to_string(),
                serde_json::to_value(v).map_err(serde::ser::Error::custom)?,
            );
        }
        if let Some(v) = &self.insert_final_newline {
            map.insert(
                "insertFinalNewline".to_string(),
                serde_json::to_value(v).map_err(serde::ser::Error::custom)?,
            );
        }
        if let Some(v) = &self.trim_final_newlines {
            map.insert(
                "trimFinalNewlines".to_string(),
                serde_json::to_value(v).map_err(serde::ser::Error::custom)?,
            );
        }
        for (k, v) in &self.properties {
            map.insert(
                k.clone(),
                serde_json::to_value(v).map_err(serde::ser::Error::custom)?,
            );
        }
        serde_json::Value::Object(map).serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for FormattingOptions {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let tab_size = map
            .remove("tabSize")
            .ok_or_else(|| serde::de::Error::missing_field("tabSize"))
            .and_then(|v| serde_json::from_value(v).map_err(serde::de::Error::custom))?;
        let insert_spaces = map
            .remove("insertSpaces")
            .ok_or_else(|| serde::de::Error::missing_field("insertSpaces"))
            .and_then(|v| serde_json::from_value(v).map_err(serde::de::Error::custom))?;
        let trim_trailing_whitespace =
            crate::deserialize_optional(map.remove("trimTrailingWhitespace"))?;
        let insert_final_newline = crate::deserialize_optional(map.remove("insertFinalNewline"))?;
        let trim_final_newlines = crate::deserialize_optional(map.remove("trimFinalNewlines"))?;
        let mut properties = HashMap::with_capacity(map.len());
        for (key, value) in map {
            let property = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            properties.insert(key, property);
        }
        Ok(FormattingOptions {
            tab_size,
            insert_spaces,
            properties,
            trim_trailing_whitespace,
            insert_final_newline,
            trim_final_newlines,
        })
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum FormattingProperty {
    Bool(bool),
    Number(i32),
    String(String),
}

impl_untagged_enum!(FormattingProperty {
    Bool(bool),
    Number(i32),
    String(String),
});

/// Extends TextDocumentRegistrationOptions
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct DocumentOnTypeFormattingRegistrationOptions {
    /// A document selector to identify the scope of the registration. If set to null
    /// the document selector provided on the client side will be used.
    pub document_selector: Option<DocumentSelector>,

    /// A character on which formatting should be triggered, like `}`.
    pub first_trigger_character: String,

    /// More trigger characters.
    pub more_trigger_character: Option<Vec<String>>,
}

impl_lsp_struct!(DocumentOnTypeFormattingRegistrationOptions,
    required { first_trigger_character: String = "firstTriggerCharacter" },
    nullable { document_selector: DocumentSelector = "documentSelector" },
    optional { more_trigger_character: Vec<String> = "moreTriggerCharacter" }
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_serialization;

    #[test]
    fn formatting_options() {
        test_serialization(
            &FormattingOptions {
                tab_size: 123,
                insert_spaces: true,
                properties: HashMap::new(),
                trim_trailing_whitespace: None,
                insert_final_newline: None,
                trim_final_newlines: None,
            },
            r#"{"insertSpaces":true,"tabSize":123}"#,
        );

        test_serialization(
            &FormattingOptions {
                tab_size: 123,
                insert_spaces: true,
                properties: vec![("prop".to_string(), FormattingProperty::Number(1))]
                    .into_iter()
                    .collect(),
                trim_trailing_whitespace: None,
                insert_final_newline: None,
                trim_final_newlines: None,
            },
            r#"{"insertSpaces":true,"prop":1,"tabSize":123}"#,
        );
    }
}
