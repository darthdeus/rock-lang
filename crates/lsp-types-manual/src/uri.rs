use std::{hash::Hash, ops::Deref, str::FromStr};

use serde::{de::Error, Deserialize, Serialize};

/// Newtype struct around `fluent_uri::Uri<String>` with serialization implementations that use `as_str()` and 'from_str()' respectively.
#[derive(Debug, Clone)]
pub struct Uri(fluent_uri::Uri<String>);

impl Serialize for Uri {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.as_str().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Uri {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let string = String::deserialize(deserializer)?;
        fluent_uri::Uri::<String>::parse_from(string)
            .map(Uri)
            .map_err(|(_, error)| Error::custom(error.to_string()))
    }
}

impl Ord for Uri {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl PartialOrd for Uri {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl FromStr for Uri {
    type Err = fluent_uri::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // TOUCH-UP:
        // Use upstream `FromStr` implementation if and when
        // https://github.com/yescallop/fluent-uri-rs/pull/10
        // gets merged.
        // fluent_uri::Uri::from_str(s).map(Self)
        fluent_uri::Uri::parse(s).map(|uri| Self(uri.to_owned()))
    }
}

impl Deref for Uri {
    type Target = fluent_uri::Uri<String>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/*
    TOUCH-UP: `PartialEq`, `Eq` and `Hash` could all be derived
    if and when the respective implementations get merged upstream:
    https://github.com/yescallop/fluent-uri-rs/pull/9
*/
impl PartialEq for Uri {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Uri {}

impl Hash for Uri {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

/// Why a `file:` URI and a filesystem path could not be converted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUriError(String);

impl std::fmt::Display for FileUriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FileUriError {}

impl Uri {
    /// The absolute filesystem path a `file:` URI names, percent-decoded.
    /// Any other scheme, a remote host, a query or fragment, or a path that
    /// does not decode to UTF-8 is an error.
    pub fn to_file_path(&self) -> Result<String, FileUriError> {
        let err = |why: &str| FileUriError(format!("{why}: {}", self.as_str()));
        match self.scheme() {
            Some(scheme) if scheme.eq_lowercase("file") => {}
            _ => return Err(err("not a file: URI")),
        }
        if let Some(auth) = self.authority() {
            let host = auth.as_str();
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return Err(err("file: URI names a remote host"));
            }
        }
        if self.query().is_some() || self.fragment().is_some() {
            return Err(err("file: URI has a query or fragment"));
        }
        let path = self
            .path()
            .as_estr()
            .decode()
            .into_string()
            .map_err(|_| err("file: URI path is not UTF-8 after decoding"))?;
        if !path.starts_with('/') {
            return Err(err("file: URI path is not absolute"));
        }
        Ok(path.into_owned())
    }

    /// The `file:` URI for an absolute filesystem path, percent-encoding every
    /// byte outside the unreserved set and `/`.
    pub fn from_file_path(path: &str) -> Result<Uri, FileUriError> {
        if !path.starts_with('/') {
            return Err(FileUriError(format!("path is not absolute: {path}")));
        }
        let mut s = String::with_capacity(path.len() + 7);
        s.push_str("file://");
        for &b in path.as_bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'.' | b'_' | b'~') {
                s.push(b as char);
            } else {
                s.push_str(&format!("%{b:02X}"));
            }
        }
        s.parse()
            .map_err(|e: fluent_uri::ParseError| FileUriError(format!("{e}: {path}")))
    }
}

#[cfg(test)]
mod tests {
    use super::Uri;
    use std::str::FromStr;

    #[test]
    fn file_path_round_trips_spaces_and_non_ascii() {
        let path = "/tmp/my dir/caf\u{e9} #1%.rock";
        let uri = Uri::from_file_path(path).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/my%20dir/caf%C3%A9%20%231%25.rock");
        assert_eq!(uri.to_file_path().unwrap(), path);
    }

    #[test]
    fn to_file_path_decodes_client_uris() {
        let uri = Uri::from_str("file:///srv/u/my%20dir/caf%C3%A9.rock").unwrap();
        assert_eq!(uri.to_file_path().unwrap(), "/srv/u/my dir/caf\u{e9}.rock");
        let uri = Uri::from_str("file://localhost/a/b.rock").unwrap();
        assert_eq!(uri.to_file_path().unwrap(), "/a/b.rock");
    }

    #[test]
    fn to_file_path_rejects_non_file_uris() {
        for s in ["untitled:Untitled-1", "https://example.com/a.rock", "file://host/a.rock",
            "file:///a.rock?x", "file:///bad%FF.rock"]
        {
            let uri = Uri::from_str(s).unwrap();
            let e = uri.to_file_path().unwrap_err();
            assert!(e.to_string().contains(s), "{}", e);
        }
        assert!(Uri::from_file_path("relative.rock").is_err());
    }
}
