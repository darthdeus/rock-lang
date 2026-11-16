use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolError(String, bool);

impl ProtocolError {
    pub(crate) fn new(msg: impl Into<String>) -> Self {
        ProtocolError(msg.into(), false)
    }

    pub(crate) fn disconnected() -> ProtocolError {
        ProtocolError("disconnected channel".into(), true)
    }

    /// Whether this error occurred due to a disconnected channel.
    pub fn channel_is_disconnected(&self) -> bool {
        self.1
    }
}

impl std::error::Error for ProtocolError {}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
