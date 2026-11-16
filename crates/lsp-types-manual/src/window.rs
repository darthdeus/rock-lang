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
