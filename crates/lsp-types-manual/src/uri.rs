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
    /// does not decode to UTF-8 is an error. On Windows the path must start
    /// with a drive (`file:///C:/x` or `file:///c%3A/x`) and comes back as
    /// `C:\x`.
    pub fn to_file_path(&self) -> Result<String, FileUriError> {
        self.to_file_path_on(cfg!(windows))
    }

    fn to_file_path_on(&self, windows: bool) -> Result<String, FileUriError> {
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
        uri_path_to_native(&path, windows).ok_or_else(|| err("file: URI path is not absolute"))
    }

    /// The `file:` URI for an absolute filesystem path, percent-encoding every
    /// byte outside the unreserved set and `/`. On Windows the path must start
    /// with a drive (`C:\x`, `C:/x`, or the verbatim `\\?\C:\x` that
    /// `std::fs::canonicalize` returns); UNC paths are not supported.
    pub fn from_file_path(path: &str) -> Result<Uri, FileUriError> {
        Self::from_file_path_on(path, cfg!(windows))
    }

    fn from_file_path_on(path: &str, windows: bool) -> Result<Uri, FileUriError> {
        let uri_path = native_to_uri_path(path, windows).map_err(|why| FileUriError(format!("{why}: {path}")))?;
        let mut s = String::with_capacity(uri_path.len() + 7);
        s.push_str("file://");
        for &b in uri_path.as_bytes() {
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


/// Whether a Windows path starts `X:` followed by the end or a separator in
/// `seps`.
fn starts_with_drive(path: &str, seps: &[u8]) -> bool {
    let b = path.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b.len() == 2 || seps.contains(&b[2]))
}

/// A decoded `file:` URI path as a native absolute path. On Windows the URI
/// path is `/C:/dir/f` (the client may have percent-encoded the colon; it is
/// decoded by now) and becomes `C:\dir\f`; elsewhere it is used as is.
fn uri_path_to_native(path: &str, windows: bool) -> Option<String> {
    if !windows {
        return path.starts_with('/').then(|| path.to_string());
    }
    let rest = path.strip_prefix('/').filter(|rest| starts_with_drive(rest, b"/"))?;
    let mut native = rest.replace('/', "\\");
    if native.len() == 2 {
        native.push('\\');
    }
    Some(native)
}

/// The unencoded URI path for a native absolute path: `C:\dir\f`, `C:/dir/f`
/// or `\\?\C:\dir\f` becomes `/C:/dir/f` on Windows; elsewhere the path must
/// start with `/` and is used as is. The error says why the path has no URI.
fn native_to_uri_path(path: &str, windows: bool) -> Result<String, &'static str> {
    if !windows {
        return if path.starts_with('/') { Ok(path.to_string()) } else { Err("path is not absolute") };
    }
    let path = match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.get(..4).is_some_and(|p| p.eq_ignore_ascii_case(r"UNC\")) => {
            return Err("verbatim UNC paths are not supported");
        }
        Some(rest) if starts_with_drive(rest, b"\\") => rest,
        Some(_) => return Err("verbatim path without a drive is not supported"),
        None if path.starts_with(r"\\") => return Err("UNC paths are not supported"),
        None => path,
    };
    if !starts_with_drive(path, b"/\\") {
        return Err("path is not absolute");
    }
    Ok(format!("/{}", path.replace('\\', "/")))
}

#[cfg(test)]
mod tests {
    use super::{native_to_uri_path, uri_path_to_native, Uri};
    use std::str::FromStr;

    #[test]
    fn windows_drive_paths_convert_both_ways() {
        assert_eq!(uri_path_to_native("/C:/x/y.rock", true).as_deref(), Some(r"C:\x\y.rock"));
        assert_eq!(uri_path_to_native("/c:/x", true).as_deref(), Some(r"c:\x"));
        assert_eq!(uri_path_to_native("/D:", true).as_deref(), Some(r"D:\"));
        for bad in ["/x/y.rock", "C:/x", "/CC:/x", "/1:/x", "/C:x"] {
            assert_eq!(uri_path_to_native(bad, true), None, "{bad}");
        }
        assert_eq!(native_to_uri_path(r"C:\x\y.rock", true).as_deref(), Ok("/C:/x/y.rock"));
        assert_eq!(native_to_uri_path("c:/x", true).as_deref(), Ok("/c:/x"));
        for bad in [r"\\server\share\a", r"x\y", "/x/y", "C:x"] {
            assert!(native_to_uri_path(bad, true).is_err(), "{}", bad);
        }
    }

    #[test]
    fn unix_paths_keep_drive_like_names_verbatim() {
        assert_eq!(uri_path_to_native("/C:/x", false).as_deref(), Some("/C:/x"));
        assert_eq!(uri_path_to_native("C:/x", false), None);
        assert!(native_to_uri_path(r"C:\x", false).is_err());
        assert_eq!(native_to_uri_path("/a b/c", false).as_deref(), Ok("/a b/c"));
    }

    #[test]
    fn windows_file_uris_round_trip_both_colon_spellings() {
        for s in ["file:///C:/x/a.rock", "file:///C%3A/x/a.rock", "file:///C%3a/x/a.rock"] {
            assert_eq!(Uri::from_str(s).unwrap().to_file_path_on(true).unwrap(), r"C:\x\a.rock", "{s}");
        }
        let uri = Uri::from_file_path_on(r"C:\my dir\a.rock", true).unwrap();
        assert_eq!(uri.as_str(), "file:///C%3A/my%20dir/a.rock");
        assert_eq!(uri.to_file_path_on(true).unwrap(), r"C:\my dir\a.rock");
        assert!(Uri::from_str("file:///x/a.rock").unwrap().to_file_path_on(true).is_err());
        assert!(Uri::from_file_path_on("/x/a.rock", true).is_err());
        // The same drive-shaped URI is an ordinary absolute path elsewhere.
        assert_eq!(Uri::from_str("file:///C%3A/x").unwrap().to_file_path_on(false).unwrap(), "/C:/x");
    }

    #[test]
    fn windows_verbatim_drive_paths_convert_like_plain_drive_paths() {
        for path in [r"\\?\C:\proj\a.rock", r"\\?\c:\proj\a.rock"] {
            let uri = Uri::from_file_path_on(path, true).unwrap();
            assert_eq!(uri, Uri::from_file_path_on(&path[4..], true).unwrap(), "{}", path);
        }
        let uri = Uri::from_file_path_on(r"\\?\C:\my dir\a.rock", true).unwrap();
        assert_eq!(uri.as_str(), "file:///C%3A/my%20dir/a.rock");
        assert_eq!(uri.to_file_path_on(true).unwrap(), r"C:\my dir\a.rock");
        // Unix has no verbatim prefix: the same string is a relative path.
        assert!(Uri::from_file_path_on(r"\\?\C:\proj\a.rock", false).is_err());
    }

    #[test]
    fn windows_verbatim_unc_and_non_drive_paths_are_rejected_by_name() {
        for path in [r"\\?\UNC\server\share\a.rock", r"\\?\unc\server\share\a.rock"] {
            let e = Uri::from_file_path_on(path, true).unwrap_err().to_string();
            assert!(e.contains("verbatim UNC") && e.contains(path), "{}", e);
        }
        let path = r"\\?\Volume{0}\a.rock";
        let e = Uri::from_file_path_on(path, true).unwrap_err().to_string();
        assert!(e.contains("verbatim path without a drive") && e.contains(path), "{}", e);
        let path = r"\\server\share\a.rock";
        let e = Uri::from_file_path_on(path, true).unwrap_err().to_string();
        assert!(e.contains("UNC") && e.contains(path), "{}", e);
    }

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
