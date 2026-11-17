#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct MessageType(i32);
lsp_enum! {
impl MessageType {
    /// A warning message.
    pub const WARNING: MessageType = MessageType(2);
}
}

lsp_struct! {
    out
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct ShowMessageParams {
        required pub typ: MessageType = "type",
        required pub message: String = "message",}
}
