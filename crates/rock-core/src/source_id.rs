/// The one spelling of a standard-library source-file id.
const STDLIB_FILE_ID_PREFIX: &str = "<stdlib>/";

pub fn stdlib_file_id(name: &str) -> String {
    format!("{STDLIB_FILE_ID_PREFIX}{name}")
}

/// The stdlib file name inside a stdlib source-file id, or `None` for any
/// other id. A real path that merely has a `stdlib` directory in it is not a
/// stdlib id: ownership comes from the loader, never from a path component.
pub fn stdlib_file_name(file: &str) -> Option<&str> {
    file.strip_prefix(STDLIB_FILE_ID_PREFIX)
        .filter(|name| !name.is_empty())
}

pub fn is_stdlib_module_file(file: &str) -> bool {
    stdlib_file_name(file).is_some()
}
