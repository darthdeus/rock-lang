use std::{
    fmt,
    io::{self, BufRead, Write},
};

use serde::de::DeserializeOwned;

use crate::error::ExtractError;

pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

// Manual Clone/Debug since we removed the derives
impl Clone for Message {
    fn clone(&self) -> Self {
        match self {
            Message::Request(r) => Message::Request(r.clone()),
            Message::Response(r) => Message::Response(r.clone()),
            Message::Notification(n) => Message::Notification(n.clone()),
        }
    }
}
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Message::Request(r) => f.debug_tuple("Request").field(r).finish(),
            Message::Response(r) => f.debug_tuple("Response").field(r).finish(),
            Message::Notification(n) => f.debug_tuple("Notification").field(n).finish(),
        }
    }
}

impl From<Request> for Message {
    fn from(request: Request) -> Message {
        Message::Request(request)
    }
}
impl From<Response> for Message {
    fn from(response: Response) -> Message {
        Message::Response(response)
    }
}
impl From<Notification> for Message {
    fn from(notification: Notification) -> Message {
        Message::Notification(notification)
    }
}

// --- RequestId / IdRepr ---

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(IdRepr);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum IdRepr {
    I32(i32),
    String(String),
}

impl From<i32> for RequestId {
    fn from(id: i32) -> RequestId {
        RequestId(IdRepr::I32(id))
    }
}
impl From<String> for RequestId {
    fn from(id: String) -> RequestId {
        RequestId(IdRepr::String(id))
    }
}
impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            IdRepr::I32(it) => fmt::Display::fmt(it, f),
            IdRepr::String(it) => fmt::Debug::fmt(it, f),
        }
    }
}

impl serde::Serialize for IdRepr {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            IdRepr::I32(n) => s.serialize_i32(*n),
            IdRepr::String(st) => s.serialize_str(st),
        }
    }
}
impl<'de> serde::Deserialize<'de> for IdRepr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = IdRepr;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "integer or string")
            }
            fn visit_i32<E: serde::de::Error>(self, v: i32) -> Result<IdRepr, E> {
                Ok(IdRepr::I32(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<IdRepr, E> {
                i32::try_from(v).map(IdRepr::I32).map_err(|_| {
                    E::invalid_value(serde::de::Unexpected::Signed(v), &"i32 within range")
                })
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<IdRepr, E> {
                i32::try_from(v).map(IdRepr::I32).map_err(|_| {
                    E::invalid_value(serde::de::Unexpected::Unsigned(v), &"i32 within range")
                })
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<IdRepr, E> {
                Ok(IdRepr::String(v.to_owned()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<IdRepr, E> {
                Ok(IdRepr::String(v))
            }
        }
        d.deserialize_any(Vis)
    }
}

impl serde::Serialize for RequestId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}
impl<'de> serde::Deserialize<'de> for RequestId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        IdRepr::deserialize(d).map(RequestId)
    }
}

// --- Request ---

#[derive(Debug, Clone)]
pub struct Request {
    pub id: RequestId,
    pub method: String,
    pub params: serde_json::Value,
}

impl serde::Serialize for Request {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let n = if self.params.is_null() { 2 } else { 3 };
        let mut m = s.serialize_map(Some(n))?;
        m.serialize_entry("id", &self.id)?;
        m.serialize_entry("method", &self.method)?;
        if !self.params.is_null() {
            m.serialize_entry("params", &self.params)?;
        }
        m.end()
    }
}
impl<'de> serde::Deserialize<'de> for Request {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = Request;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "struct Request")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Request, A::Error> {
                let mut id: Option<RequestId> = None;
                let mut method: Option<String> = None;
                let mut params = serde_json::Value::Null;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => {
                            id = Some(map.next_value()?);
                        }
                        "method" => {
                            method = Some(map.next_value()?);
                        }
                        "params" => {
                            params = map.next_value()?;
                        }
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                Ok(Request {
                    id: id.ok_or_else(|| serde::de::Error::missing_field("id"))?,
                    method: method.ok_or_else(|| serde::de::Error::missing_field("method"))?,
                    params,
                })
            }
        }
        d.deserialize_map(Vis)
    }
}

// --- ResponseError ---

#[derive(Debug, Clone)]
pub struct ResponseError {
    pub code: i32,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

impl serde::Serialize for ResponseError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let n = if self.data.is_some() { 3 } else { 2 };
        let mut m = s.serialize_map(Some(n))?;
        m.serialize_entry("code", &self.code)?;
        m.serialize_entry("message", &self.message)?;
        if let Some(ref d) = self.data {
            m.serialize_entry("data", d)?;
        }
        m.end()
    }
}
impl<'de> serde::Deserialize<'de> for ResponseError {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = ResponseError;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "struct ResponseError")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<ResponseError, A::Error> {
                let mut code: Option<i32> = None;
                let mut message: Option<String> = None;
                let mut data: Option<serde_json::Value> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "code" => {
                            code = Some(map.next_value()?);
                        }
                        "message" => {
                            message = Some(map.next_value()?);
                        }
                        "data" => {
                            data = map.next_value()?;
                        }
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                Ok(ResponseError {
                    code: code.ok_or_else(|| serde::de::Error::missing_field("code"))?,
                    message: message.ok_or_else(|| serde::de::Error::missing_field("message"))?,
                    data,
                })
            }
        }
        d.deserialize_map(Vis)
    }
}

// --- Response ---

#[derive(Debug, Clone)]
pub struct Response {
    pub id: RequestId,
    pub result: Option<serde_json::Value>,
    pub error: Option<ResponseError>,
}

impl serde::Serialize for Response {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut n = 1;
        if self.result.is_some() {
            n += 1;
        }
        if self.error.is_some() {
            n += 1;
        }
        let mut m = s.serialize_map(Some(n))?;
        m.serialize_entry("id", &self.id)?;
        if let Some(ref r) = self.result {
            m.serialize_entry("result", r)?;
        }
        if let Some(ref e) = self.error {
            m.serialize_entry("error", e)?;
        }
        m.end()
    }
}
impl<'de> serde::Deserialize<'de> for Response {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = Response;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "struct Response")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Response, A::Error> {
                let mut id: Option<RequestId> = None;
                let mut result: Option<serde_json::Value> = None;
                let mut error: Option<ResponseError> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => {
                            id = Some(map.next_value()?);
                        }
                        "result" => {
                            result = map.next_value()?;
                        }
                        "error" => {
                            error = map.next_value()?;
                        }
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                Ok(Response {
                    id: id.ok_or_else(|| serde::de::Error::missing_field("id"))?,
                    result,
                    error,
                })
            }
        }
        d.deserialize_map(Vis)
    }
}

// --- Notification ---

#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: serde_json::Value,
}

impl serde::Serialize for Notification {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let n = if self.params.is_null() { 1 } else { 2 };
        let mut m = s.serialize_map(Some(n))?;
        m.serialize_entry("method", &self.method)?;
        if !self.params.is_null() {
            m.serialize_entry("params", &self.params)?;
        }
        m.end()
    }
}
impl<'de> serde::Deserialize<'de> for Notification {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = Notification;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "struct Notification")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Notification, A::Error> {
                let mut method: Option<String> = None;
                let mut params = serde_json::Value::Null;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "method" => {
                            method = Some(map.next_value()?);
                        }
                        "params" => {
                            params = map.next_value()?;
                        }
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                Ok(Notification {
                    method: method.ok_or_else(|| serde::de::Error::missing_field("method"))?,
                    params,
                })
            }
        }
        d.deserialize_map(Vis)
    }
}

// --- Message (untagged enum) ---
// Dispatch by key presence: id+method -> Request, id only -> Response, method only -> Notification

impl serde::Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Message::Request(r) => r.serialize(s),
            Message::Response(r) => r.serialize(s),
            Message::Notification(n) => n.serialize(s),
        }
    }
}
impl<'de> serde::Deserialize<'de> for Message {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        let obj = v
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("expected JSON object for LSP message"))?;
        let has_id = obj.contains_key("id");
        let has_method = obj.contains_key("method");
        match (has_id, has_method) {
            (true, true) => serde_json::from_value::<Request>(v)
                .map(Message::Request)
                .map_err(serde::de::Error::custom),
            (true, false) => serde_json::from_value::<Response>(v)
                .map(Message::Response)
                .map_err(serde::de::Error::custom),
            (false, true) => serde_json::from_value::<Notification>(v)
                .map(Message::Notification)
                .map_err(serde::de::Error::custom),
            (false, false) => Err(serde::de::Error::custom(
                "LSP message must have 'id' or 'method' field",
            )),
        }
    }
}

// --- ErrorCode ---

#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum ErrorCode {
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
    ServerErrorStart = -32099,
    ServerErrorEnd = -32000,
    ServerNotInitialized = -32002,
    UnknownErrorCode = -32001,
    RequestCanceled = -32800,
    ContentModified = -32801,
    ServerCancelled = -32802,
    RequestFailed = -32803,
}

// --- I/O helpers ---

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

macro_rules! invalid_data {
    ($($tt:tt)*) => (invalid_data(format!($($tt)*)))
}

impl Message {
    pub fn read(r: &mut impl BufRead) -> io::Result<Option<Message>> {
        Message::_read(r)
    }
    fn _read(r: &mut dyn BufRead) -> io::Result<Option<Message>> {
        let text = match read_msg_text(r)? {
            None => return Ok(None),
            Some(text) => text,
        };
        let msg = serde_json::from_str(&text)
            .map_err(|e| invalid_data!("malformed LSP payload: {:?}", e))?;
        Ok(Some(msg))
    }
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        self._write(w)
    }
    fn _write(&self, w: &mut dyn Write) -> io::Result<()> {
        // Serialize as {"jsonrpc":"2.0", ...message fields...}
        let inner = serde_json::to_value(self).map_err(invalid_data)?;
        let mut map = serde_json::Map::new();
        map.insert(
            "jsonrpc".to_string(),
            serde_json::Value::String("2.0".to_string()),
        );
        if let serde_json::Value::Object(inner_map) = inner {
            map.extend(inner_map);
        }
        let text = serde_json::to_string(&serde_json::Value::Object(map)).map_err(invalid_data)?;
        write_msg_text(w, &text)
    }
}

impl Response {
    pub fn new_ok<R: serde::Serialize>(id: RequestId, result: R) -> Response {
        Response {
            id,
            result: Some(serde_json::to_value(result).unwrap()),
            error: None,
        }
    }
    pub fn new_err(id: RequestId, code: i32, message: String) -> Response {
        let error = ResponseError {
            code,
            message,
            data: None,
        };
        Response {
            id,
            result: None,
            error: Some(error),
        }
    }
}

impl Request {
    pub fn new<P: serde::Serialize>(id: RequestId, method: String, params: P) -> Request {
        Request {
            id,
            method,
            params: serde_json::to_value(params).unwrap(),
        }
    }
    pub fn extract<P: DeserializeOwned>(
        self,
        method: &str,
    ) -> Result<(RequestId, P), ExtractError<Request>> {
        if self.method != method {
            return Err(ExtractError::MethodMismatch(self));
        }
        match serde_json::from_value(self.params) {
            Ok(params) => Ok((self.id, params)),
            Err(error) => Err(ExtractError::JsonError {
                method: self.method,
                error,
            }),
        }
    }
    pub(crate) fn is_shutdown(&self) -> bool {
        self.method == "shutdown"
    }
    pub(crate) fn is_initialize(&self) -> bool {
        self.method == "initialize"
    }
}

impl Notification {
    pub fn new(method: String, params: impl serde::Serialize) -> Notification {
        Notification {
            method,
            params: serde_json::to_value(params).unwrap(),
        }
    }
    pub fn extract<P: DeserializeOwned>(
        self,
        method: &str,
    ) -> Result<P, ExtractError<Notification>> {
        if self.method != method {
            return Err(ExtractError::MethodMismatch(self));
        }
        match serde_json::from_value(self.params) {
            Ok(params) => Ok(params),
            Err(error) => Err(ExtractError::JsonError {
                method: self.method,
                error,
            }),
        }
    }
    pub(crate) fn is_exit(&self) -> bool {
        self.method == "exit"
    }
    pub(crate) fn is_initialized(&self) -> bool {
        self.method == "initialized"
    }
}

fn read_msg_text(inp: &mut dyn BufRead) -> io::Result<Option<String>> {
    let mut size = None;
    let mut buf = String::new();
    loop {
        buf.clear();
        if inp.read_line(&mut buf)? == 0 {
            return Ok(None);
        }
        if !buf.ends_with("\r\n") {
            return Err(invalid_data!("malformed header: {:?}", buf));
        }
        let buf = &buf[..buf.len() - 2];
        if buf.is_empty() {
            break;
        }
        let mut parts = buf.splitn(2, ": ");
        let header_name = parts.next().unwrap();
        let header_value = parts
            .next()
            .ok_or_else(|| invalid_data!("malformed header: {:?}", buf))?;
        if header_name.eq_ignore_ascii_case("Content-Length") {
            size = Some(header_value.parse::<usize>().map_err(invalid_data)?);
        }
    }
    let size: usize = size.ok_or_else(|| invalid_data!("no Content-Length"))?;
    let mut buf = buf.into_bytes();
    buf.resize(size, 0);
    inp.read_exact(&mut buf)?;
    let buf = String::from_utf8(buf).map_err(invalid_data)?;
    log::debug!("< {buf}");
    Ok(Some(buf))
}

fn write_msg_text(out: &mut dyn Write, msg: &str) -> io::Result<()> {
    log::debug!("> {msg}");
    write!(out, "Content-Length: {}\r\n\r\n", msg.len())?;
    out.write_all(msg.as_bytes())?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Message, Notification, Request, RequestId};

    #[test]
    fn shutdown_with_explicit_null() {
        let text = "{\"jsonrpc\": \"2.0\",\"id\": 3,\"method\": \"shutdown\", \"params\": null }";
        let msg: Message = serde_json::from_str(text).unwrap();
        assert!(
            matches!(msg, Message::Request(req) if req.id == 3.into() && req.method == "shutdown")
        );
    }

    #[test]
    fn shutdown_with_no_params() {
        let text = "{\"jsonrpc\": \"2.0\",\"id\": 3,\"method\": \"shutdown\"}";
        let msg: Message = serde_json::from_str(text).unwrap();
        assert!(
            matches!(msg, Message::Request(req) if req.id == 3.into() && req.method == "shutdown")
        );
    }

    #[test]
    fn notification_with_explicit_null() {
        let text = "{\"jsonrpc\": \"2.0\",\"method\": \"exit\", \"params\": null }";
        let msg: Message = serde_json::from_str(text).unwrap();
        assert!(matches!(msg, Message::Notification(not) if not.method == "exit"));
    }

    #[test]
    fn notification_with_no_params() {
        let text = "{\"jsonrpc\": \"2.0\",\"method\": \"exit\"}";
        let msg: Message = serde_json::from_str(text).unwrap();
        assert!(matches!(msg, Message::Notification(not) if not.method == "exit"));
    }

    #[test]
    fn serialize_request_with_null_params() {
        let msg = Message::Request(Request {
            id: RequestId::from(3),
            method: "shutdown".into(),
            params: serde_json::Value::Null,
        });
        let serialized = serde_json::to_string(&msg).unwrap();
        assert_eq!("{\"id\":3,\"method\":\"shutdown\"}", serialized);
    }

    #[test]
    fn serialize_notification_with_null_params() {
        let msg = Message::Notification(Notification {
            method: "exit".into(),
            params: serde_json::Value::Null,
        });
        let serialized = serde_json::to_string(&msg).unwrap();
        assert_eq!("{\"method\":\"exit\"}", serialized);
    }
}
