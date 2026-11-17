//! A language server scaffold, exposing a synchronous crossbeam-channel based API.
//! This crate handles protocol handshaking and parsing messages, while you
//! control the message dispatch loop yourself.
//!
//! Run with `RUST_LOG=lsp_server=debug` to see all the messages.

#![warn(unused_lifetimes)]
#![allow(clippy::print_stdout, clippy::disallowed_types)]

mod error;
mod msg;
mod stdio;

use crossbeam_channel::{Receiver, RecvError, Sender};

pub use crate::{
    error::ProtocolError,
    msg::{ErrorCode, Message, Notification, Request, RequestId, Response, ResponseError},
    stdio::IoThreads,
};

/// Connection is just a pair of channels of LSP messages.
pub struct Connection {
    pub sender: Sender<Message>,
    pub receiver: Receiver<Message>,
}

impl Connection {
    pub fn stdio() -> (Connection, IoThreads) {
        let (sender, receiver, io_threads) = stdio::stdio_transport();
        (Connection { sender, receiver }, io_threads)
    }

    fn initialize_start(&self) -> Result<(RequestId, serde_json::Value), ProtocolError> {
        loop {
            match self.receiver.recv() {
                Ok(Message::Request(req)) if req.is_initialize() => {
                    return Ok((req.id, req.params));
                }
                Ok(Message::Request(req)) => {
                    let resp = Response::new_err(
                        req.id.clone(),
                        ErrorCode::ServerNotInitialized as i32,
                        format!("expected initialize request, got {req:?}"),
                    );
                    self.sender.send(resp.into()).unwrap();
                }
                Ok(Message::Notification(n)) if !n.is_exit() => {}
                Ok(msg) => {
                    return Err(ProtocolError::new(format!(
                        "expected initialize request, got {msg:?}"
                    )));
                }
                Err(RecvError) => return Err(ProtocolError::disconnected()),
            }
        }
    }

    fn initialize_finish(
        &self,
        initialize_id: RequestId,
        initialize_result: serde_json::Value,
    ) -> Result<(), ProtocolError> {
        let resp = Response::new_ok(initialize_id, initialize_result);
        self.sender.send(resp.into()).unwrap();
        match &self.receiver.recv() {
            Ok(Message::Notification(n)) if n.is_initialized() => Ok(()),
            Ok(msg) => Err(ProtocolError::new(format!(
                r#"expected initialized notification, got: {msg:?}"#
            ))),
            Err(RecvError) => Err(ProtocolError::disconnected()),
        }
    }

    pub fn initialize(
        &self,
        server_capabilities: serde_json::Value,
    ) -> Result<serde_json::Value, ProtocolError> {
        let (id, params) = self.initialize_start()?;
        let initialize_data = serde_json::json!({
            "capabilities": server_capabilities,
        });
        self.initialize_finish(id, initialize_data)?;
        Ok(params)
    }
}
