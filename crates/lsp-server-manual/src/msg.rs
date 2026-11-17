use std::{
    fmt,
    io::{self, BufRead, Write},
};

#[derive(Clone, Debug)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Notification),
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

// Accept absent or null params and omit them on output. An explicit null
// response result, on the other hand, is a valid successful response.
macro_rules! message_fields {
    ($name:ident { $($mode:ident $field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone)]
        pub struct $name {
            $(pub $field: message_fields!(@type $mode $ty),)*
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;
                $(let $field = message_fields!(@value $mode self.$field);)*
                let n = 0 $(+ usize::from($field.is_some()))*;
                let mut map = s.serialize_map(Some(n))?;
                $(if let Some(value) = $field {
                    map.serialize_entry(stringify!($field), value)?;
                })*
                map.end()
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl<'de> serde::de::Visitor<'de> for Visitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        write!(f, concat!("struct ", stringify!($name)))
                    }
                    fn visit_map<A: serde::de::MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> Result<Self::Value, A::Error> {
                        $(let mut $field: Option<$ty> = None;)*
                        while let Some(key) = map.next_key::<String>()? {
                            match key.as_str() {
                                $(stringify!($field) => $field = message_fields!(@read $mode map),)*
                                _ => { let _ = map.next_value::<serde_json::Value>()?; }
                            }
                        }
                        Ok($name {
                            $($field: message_fields!(@finish $mode $field),)*
                        })
                    }
                }
                d.deserialize_map(Visitor)
            }
        }
    };
    (@type required $ty:ty) => { $ty };
    (@type params $ty:ty) => { $ty };
    (@type optional $ty:ty) => { Option<$ty> };
    (@value required $value:expr) => { Some(&$value) };
    (@value params $value:expr) => { (!$value.is_null()).then_some(&$value) };
    (@value optional $value:expr) => { $value.as_ref() };
    (@read required $map:ident) => { Some($map.next_value()?) };
    (@read params $map:ident) => { Some($map.next_value()?) };
    (@read optional $map:ident) => { $map.next_value()? };
    (@finish required $field:ident) => {
        $field.ok_or_else(|| serde::de::Error::missing_field(stringify!($field)))?
    };
    (@finish params $field:ident) => { $field.unwrap_or(serde_json::Value::Null) };
    (@finish optional $field:ident) => { $field };
}

message_fields! {
    Request {
        required id: RequestId,
        required method: String,
        params params: serde_json::Value,
    }
}

message_fields! {
    ResponseError {
        required code: i32,
        required message: String,
        optional data: serde_json::Value,
    }
}

message_fields! {
    Response {
        required id: RequestId,
        optional result: serde_json::Value,
        optional error: ResponseError,
    }
}

message_fields! {
    Notification {
        required method: String,
        params params: serde_json::Value,
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
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
    ServerNotInitialized = -32002,
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
    pub fn read(r: &mut dyn BufRead) -> io::Result<Option<Message>> {
        let text = match read_msg_text(r)? {
            None => return Ok(None),
            Some(text) => text,
        };
        let msg = serde_json::from_str(&text)
            .map_err(|e| invalid_data!("malformed LSP payload: {:?}", e))?;
        Ok(Some(msg))
    }
    pub fn write(&self, w: &mut dyn Write) -> io::Result<()> {
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
    pub(crate) fn is_initialize(&self) -> bool {
        self.method == "initialize"
    }
}

impl Notification {
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
