use lsp_types::Uri;

/// Why a `file:` URI and a filesystem path could not be converted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUriError(String);

impl std::fmt::Display for FileUriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FileUriError {}

/// Convert local editor file URIs and absolute native paths, including Windows drives.
pub trait FileUriExt {
    fn to_file_path(&self) -> Result<String, FileUriError>;
    fn from_file_path(path: &str) -> Result<Uri, FileUriError>;
}

impl FileUriExt for Uri {
    fn to_file_path(&self) -> Result<String, FileUriError> {
        to_file_path_on(self, cfg!(windows))
    }

    fn from_file_path(path: &str) -> Result<Uri, FileUriError> {
        from_file_path_on(path, cfg!(windows))
    }
}

fn to_file_path_on(uri: &Uri, windows: bool) -> Result<String, FileUriError> {
    let err = |why: &str| FileUriError(format!("{why}: {}", uri.as_str()));
    match uri.scheme() {
        Some(scheme) if scheme.eq_lowercase("file") => {}
        _ => return Err(err("not a file: URI")),
    }
    if let Some(auth) = uri.authority() {
        let host = auth.as_str();
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return Err(err("file: URI names a remote host"));
        }
    }
    if uri.query().is_some() || uri.fragment().is_some() {
        return Err(err("file: URI has a query or fragment"));
    }
    let path = uri
        .path()
        .as_estr()
        .decode()
        .into_string()
        .map_err(|_| err("file: URI path is not UTF-8 after decoding"))?;
    uri_path_to_native(&path, windows).ok_or_else(|| err("file: URI path is not absolute"))
}

fn from_file_path_on(path: &str, windows: bool) -> Result<Uri, FileUriError> {
    let uri_path =
        native_to_uri_path(path, windows).map_err(|why| FileUriError(format!("{why}: {path}")))?;
    let mut s = String::with_capacity(uri_path.len() + 7);
    s.push_str("file://");
    for &b in uri_path.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'.' | b'_' | b'~') {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s.parse::<Uri>()
        .map_err(|e| FileUriError(format!("{e}: {path}")))
}

fn starts_with_drive(path: &str, seps: &[u8]) -> bool {
    let b = path.as_bytes();
    b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || seps.contains(&b[2]))
}

/// On Windows the URI path `/C:/dir/f` (colon possibly percent-encoded by
/// the client) becomes `C:\dir\f`.
fn uri_path_to_native(path: &str, windows: bool) -> Option<String> {
    if !windows {
        return path.starts_with('/').then(|| path.to_string());
    }
    let rest = path
        .strip_prefix('/')
        .filter(|rest| starts_with_drive(rest, b"/"))?;
    let mut native = rest.replace('/', "\\");
    if native.len() == 2 {
        native.push('\\');
    }
    Some(native)
}

/// On Windows `C:\dir\f`, `C:/dir/f` or `\\?\C:\dir\f` becomes `/C:/dir/f`.
fn native_to_uri_path(path: &str, windows: bool) -> Result<String, &'static str> {
    if !windows {
        return if path.starts_with('/') {
            Ok(path.to_string())
        } else {
            Err("path is not absolute")
        };
    }
    let path = match path.strip_prefix(r"\\?\") {
        Some(rest)
            if rest
                .get(..4)
                .is_some_and(|p| p.eq_ignore_ascii_case(r"UNC\")) =>
        {
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
    use super::*;
    use std::str::FromStr;

    #[test]
    fn windows_drive_paths_convert_both_ways() {
        assert_eq!(
            uri_path_to_native("/C:/x/y.rock", true).as_deref(),
            Some(r"C:\x\y.rock")
        );
        assert_eq!(uri_path_to_native("/c:/x", true).as_deref(), Some(r"c:\x"));
        assert_eq!(uri_path_to_native("/D:", true).as_deref(), Some(r"D:\"));
        for bad in ["/x/y.rock", "C:/x", "/CC:/x", "/1:/x", "/C:x"] {
            assert_eq!(uri_path_to_native(bad, true), None, "{bad}");
        }
        assert_eq!(
            native_to_uri_path(r"C:\x\y.rock", true).as_deref(),
            Ok("/C:/x/y.rock")
        );
        assert_eq!(native_to_uri_path("c:/x", true).as_deref(), Ok("/c:/x"));
        for bad in [r"\\server\share\a", r"x\y", "/x/y", "C:x"] {
            assert!(native_to_uri_path(bad, true).is_err(), "{}", bad);
        }
    }

    #[test]
    fn windows_verbatim_drive_paths_convert_like_plain_drive_paths() {
        for path in [r"\\?\C:\proj\a.rock", r"\\?\c:\proj\a.rock"] {
            let uri = from_file_path_on(path, true).unwrap();
            assert_eq!(
                uri,
                from_file_path_on(&path[4..], true).unwrap(),
                "{}",
                path
            );
        }
        let uri = from_file_path_on(r"\\?\C:\my dir\a.rock", true).unwrap();
        assert_eq!(uri.as_str(), "file:///C%3A/my%20dir/a.rock");
        assert_eq!(to_file_path_on(&uri, true).unwrap(), r"C:\my dir\a.rock");
        // Unix has no verbatim prefix: the same string is a relative path.
        assert!(from_file_path_on(r"\\?\C:\proj\a.rock", false).is_err());
    }

    #[test]
    fn windows_verbatim_unc_and_non_drive_paths_are_rejected_by_name() {
        for path in [
            r"\\?\UNC\server\share\a.rock",
            r"\\?\unc\server\share\a.rock",
        ] {
            let e = from_file_path_on(path, true).unwrap_err().to_string();
            assert!(e.contains("verbatim UNC") && e.contains(path), "{}", e);
        }
        let path = r"\\?\Volume{0}\a.rock";
        let e = from_file_path_on(path, true).unwrap_err().to_string();
        assert!(
            e.contains("verbatim path without a drive") && e.contains(path),
            "{}",
            e
        );
        let path = r"\\server\share\a.rock";
        let e = from_file_path_on(path, true).unwrap_err().to_string();
        assert!(e.contains("UNC") && e.contains(path), "{}", e);
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
        for s in [
            "untitled:Untitled-1",
            "https://example.com/a.rock",
            "file://host/a.rock",
            "file:///a.rock?x",
            "file:///bad%FF.rock",
        ] {
            let uri = Uri::from_str(s).unwrap();
            let e = uri.to_file_path().unwrap_err();
            assert!(e.to_string().contains(s), "{}", e);
        }
        assert!(Uri::from_file_path("relative.rock").is_err());
    }
}
