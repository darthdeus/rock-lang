//! A language server scaffold, exposing a synchronous crossbeam-channel based API.
//! This crate handles protocol handshaking and parsing messages, while you
//! control the message dispatch loop yourself.
//!
//! Run with `RUST_LOG=lsp_server=debug` to see all the messages.
//!
//! Fork of lsp-server 0.7.9 with hand-rolled serde impls (no serde_derive dependency).

#![warn(unused_lifetimes)]
#![allow(clippy::print_stdout, clippy::disallowed_types)]

mod error;
mod msg;
mod req_queue;
mod socket;
mod stdio;

use std::{
    io,
    net::{TcpStream, ToSocketAddrs},
};

use crossbeam_channel::{Receiver, RecvError, RecvTimeoutError, Sender};

pub use crate::{
    error::{ExtractError, ProtocolError},
    msg::{ErrorCode, Message, Notification, Request, RequestId, Response, ResponseError},
    req_queue::{Incoming, Outgoing, ReqQueue},
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

    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<(Connection, IoThreads)> {
        let stream = TcpStream::connect(addr)?;
        let (sender, receiver, io_threads) = socket::socket_transport(stream);
        Ok((Connection { sender, receiver }, io_threads))
    }

    pub fn memory() -> (Connection, Connection) {
        let (s1, r1) = crossbeam_channel::unbounded();
        let (s2, r2) = crossbeam_channel::unbounded();
        (
            Connection {
                sender: s1,
                receiver: r2,
            },
            Connection {
                sender: s2,
                receiver: r1,
            },
        )
    }

    pub(crate) fn initialize_start(&self) -> Result<(RequestId, serde_json::Value), ProtocolError> {
        self.initialize_start_while(|| true)
    }

    pub(crate) fn initialize_start_while<C>(
        &self,
        running: C,
    ) -> Result<(RequestId, serde_json::Value), ProtocolError>
    where
        C: Fn() -> bool,
    {
        while running() {
            let msg = match self
                .receiver
                .recv_timeout(std::time::Duration::from_secs(1))
            {
                Ok(msg) => msg,
                Err(RecvTimeoutError::Timeout) => {
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return Err(ProtocolError::disconnected()),
            };

            match msg {
                Message::Request(req) if req.is_initialize() => return Ok((req.id, req.params)),
                Message::Request(req) => {
                    let resp = Response::new_err(
                        req.id.clone(),
                        ErrorCode::ServerNotInitialized as i32,
                        format!("expected initialize request, got {req:?}"),
                    );
                    self.sender.send(resp.into()).unwrap();
                    continue;
                }
                Message::Notification(n) if !n.is_exit() => {
                    continue;
                }
                msg => {
                    return Err(ProtocolError::new(format!(
                        "expected initialize request, got {msg:?}"
                    )));
                }
            };
        }

        Err(ProtocolError::new(String::from(
            "Initialization has been aborted during initialization",
        )))
    }

    pub(crate) fn initialize_finish(
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
