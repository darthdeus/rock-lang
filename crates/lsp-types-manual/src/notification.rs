use super::*;

use serde::{de::DeserializeOwned, Serialize};

pub trait Notification {
    type Params: DeserializeOwned + Serialize + Send + Sync + 'static;
    const METHOD: &'static str;
}

/// The show message notification is sent from a server to a client to ask the client to display a particular message
/// in the user interface.
#[derive(Debug)]
pub enum ShowMessage {}

impl Notification for ShowMessage {
    type Params = ShowMessageParams;
    const METHOD: &'static str = "window/showMessage";
}

/// Diagnostics notification are sent from the server to the client to signal results of validation runs.
#[derive(Debug)]
pub enum PublishDiagnostics {}

impl Notification for PublishDiagnostics {
    type Params = PublishDiagnosticsParams;
    const METHOD: &'static str = "textDocument/publishDiagnostics";
}
