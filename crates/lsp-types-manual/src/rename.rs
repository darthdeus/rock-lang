use crate::{Range, TextDocumentPositionParams};

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct RenameParams {
    /// Text Document and Position fields
    pub text_document_position: TextDocumentPositionParams,

    /// The new name of the symbol. If the given name is not valid the
    /// request must return a [ResponseError](#ResponseError) with an
    /// appropriate message set.
    pub new_name: String,
}

impl_lsp_struct!(
    RenameParams,
    required {
        new_name: String = "newName"
    },
    optional {},
    flatten {
        text_document_position: TextDocumentPositionParams,
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum PrepareRenameResponse {
    Range(Range),
    RangeWithPlaceholder { range: Range, placeholder: String },
    DefaultBehavior { default_behavior: bool },
}

impl serde::Serialize for PrepareRenameResponse {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::Error as _;
        match self {
            Self::Range(v) => v.serialize(s),
            Self::RangeWithPlaceholder { range, placeholder } => {
                let mut map = serde_json::Map::new();
                map.insert(
                    "range".to_string(),
                    serde_json::to_value(range).map_err(S::Error::custom)?,
                );
                map.insert(
                    "placeholder".to_string(),
                    serde_json::Value::String(placeholder.clone()),
                );
                serde_json::Value::Object(map).serialize(s)
            }
            Self::DefaultBehavior { default_behavior } => {
                let mut map = serde_json::Map::new();
                map.insert(
                    "defaultBehavior".to_string(),
                    serde_json::Value::Bool(*default_behavior),
                );
                serde_json::Value::Object(map).serialize(s)
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for PrepareRenameResponse {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let val = serde_json::Value::deserialize(d)?;
        if let Some(v) = val.get("defaultBehavior").and_then(|v| v.as_bool()) {
            return Ok(Self::DefaultBehavior {
                default_behavior: v,
            });
        }
        if val.get("placeholder").is_some() {
            let range: Range = serde_json::from_value(
                val.get("range").cloned().unwrap_or(serde_json::Value::Null),
            )
            .map_err(serde::de::Error::custom)?;
            let placeholder = val
                .get("placeholder")
                .and_then(|v| v.as_str())
                .ok_or_else(|| serde::de::Error::missing_field("placeholder"))?
                .to_owned();
            return Ok(Self::RangeWithPlaceholder { range, placeholder });
        }
        let range = serde_json::from_value::<Range>(val).map_err(serde::de::Error::custom)?;
        Ok(Self::Range(range))
    }
}
