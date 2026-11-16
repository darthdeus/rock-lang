#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct MessageType(i32);
lsp_enum! {
impl MessageType {
    /// A warning message.
    pub const WARNING: MessageType = MessageType(2);
}
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct ShowMessageParams {
    pub typ: MessageType,
    pub message: String,
}

impl_lsp_struct!(
    out ShowMessageParams,
    required {
        typ: MessageType = "type",
        message: String = "message",
    },
    optional {}
);
