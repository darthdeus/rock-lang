use super::*;

use serde::{de::DeserializeOwned, Serialize};

pub trait Notification {
    type Params: DeserializeOwned + Serialize + Send + Sync + 'static;
    const METHOD: &'static str;
}

/// A notification to ask the server to exit its process.
/// The server should exit with success code 0 if the shutdown request has been received before;
/// otherwise with error code 1.
#[derive(Debug)]
pub enum Exit {}

impl Notification for Exit {
    type Params = ();
    const METHOD: &'static str = "exit";
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

/// The progress notification is sent from the server to the client to ask
/// the client to indicate progress.
#[derive(Debug)]
pub enum Progress {}

impl Notification for Progress {
    type Params = ProgressParams;
    const METHOD: &'static str = "$/progress";
}
