use std::collections::HashMap;

use serde_json::Value;

#[derive(Eq, PartialEq, Clone, Copy)]
pub struct MessageType(i32);
lsp_enum! {
impl MessageType {
    /// An error message.
    pub const ERROR: MessageType = MessageType(1);
    /// A warning message.
    pub const WARNING: MessageType = MessageType(2);
    /// An information message;
    pub const INFO: MessageType = MessageType(3);
    /// A log message.
    pub const LOG: MessageType = MessageType(4);
}
}

/// Window specific client capabilities.
#[derive(Debug, PartialEq, Clone, Default)]
pub struct WindowClientCapabilities {
    pub work_done_progress: Option<bool>,
    pub show_message: Option<ShowMessageRequestClientCapabilities>,
    pub show_document: Option<ShowDocumentClientCapabilities>,
}

impl_lsp_struct!(
    WindowClientCapabilities,
    required {},
    optional {
        work_done_progress: bool = "workDoneProgress",
        show_message: ShowMessageRequestClientCapabilities = "showMessage",
        show_document: ShowDocumentClientCapabilities = "showDocument",
    }
);

/// Show message request client capabilities
#[derive(Debug, PartialEq, Clone, Default)]
pub struct ShowMessageRequestClientCapabilities {
    pub message_action_item: Option<MessageActionItemCapabilities>,
}

impl_lsp_struct!(
    ShowMessageRequestClientCapabilities,
    required {},
    optional {
        message_action_item: MessageActionItemCapabilities = "messageActionItem"
    }
);

#[derive(Debug, PartialEq, Clone, Default)]
pub struct MessageActionItemCapabilities {
    pub additional_properties_support: Option<bool>,
}

impl_lsp_struct!(
    MessageActionItemCapabilities,
    required {},
    optional {
        additional_properties_support: bool = "additionalPropertiesSupport"
    }
);

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct MessageActionItem {
    /// A short title like 'Retry', 'Open Log' etc.
    pub title: String,

    /// Additional attributes that the client preserves and
    /// sends back to the server. This depends on the client
    /// capability window.messageActionItem.additionalPropertiesSupport
    pub properties: HashMap<String, MessageActionItemProperty>,
}

impl serde::Serialize for MessageActionItem {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = serde_json::Map::new();
        map.insert(
            "title".to_string(),
            serde_json::to_value(&self.title).map_err(serde::ser::Error::custom)?,
        );
        for (k, v) in &self.properties {
            map.insert(
                k.clone(),
                serde_json::to_value(v).map_err(serde::ser::Error::custom)?,
            );
        }
        serde_json::Value::Object(map).serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for MessageActionItem {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let title = map
            .remove("title")
            .ok_or_else(|| serde::de::Error::missing_field("title"))
            .and_then(|v| serde_json::from_value(v).map_err(serde::de::Error::custom))?;
        let properties = map
            .into_iter()
            .map(|(k, v)| {
                serde_json::from_value(v)
                    .map(|p| (k, p))
                    .map_err(serde::de::Error::custom)
            })
            .collect::<Result<HashMap<_, _>, D::Error>>()?;
        Ok(MessageActionItem { title, properties })
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum MessageActionItemProperty {
    String(String),
    Boolean(bool),
    Integer(i32),
    Object(Value),
}

impl_untagged_enum!(MessageActionItemProperty {
    Boolean(bool),
    Integer(i32),
    String(String),
    Object(Value),
});

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct ShowMessageParams {
    pub typ: MessageType,
    pub message: String,
}

impl_lsp_struct!(
    ShowMessageParams,
    required {
        typ: MessageType = "type",
        message: String = "message",
    },
    optional {}
);

/// Client capabilities for the show document request.
#[derive(Debug, PartialEq, Clone, Default)]
pub struct ShowDocumentClientCapabilities {
    pub support: bool,
}

impl_lsp_struct!(
    ShowDocumentClientCapabilities,
    required {
        support: bool = "support"
    },
    optional {}
);
