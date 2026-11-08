use ustr::Ustr;

/// Lightweight identifier for source files, used instead of heavy Arc<AtomicRefCell<SourceFile>>
/// The actual SourceFile data is stored in a centralized SourceRegistry for lookup during error rendering
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub Ustr);

impl SourceId {
    pub fn new(path: impl Into<Ustr>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &str {
        self.0.as_str()
    }

    pub fn as_ustr(&self) -> Ustr {
        self.0
    }
}

impl From<&str> for SourceId {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}

impl From<String> for SourceId {
    fn from(path: String) -> Self {
        Self::new(path)
    }
}

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The one spelling of a standard-library source-file id. The stdlib loader
/// builds every stdlib id with [`stdlib_file_id`], and every consumer (std
/// ownership in semantic analysis, lowering, emission, the editor, the module
/// tools) reads it back with [`is_stdlib_module_file`] or [`stdlib_file_name`].
const STDLIB_FILE_ID_PREFIX: &str = "<stdlib>/";

/// The source-file id of stdlib file `name` (for example `unified.rock`).
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

/// Whether `file` is a stdlib source-file id built by [`stdlib_file_id`].
pub fn is_stdlib_module_file(file: &str) -> bool {
    stdlib_file_name(file).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdlib_file_id_round_trips_through_the_predicate_and_name() {
        for name in ["unified.rock", "arena.rock", "scene.rock"] {
            let id = stdlib_file_id(name);
            assert!(is_stdlib_module_file(&id), "{id}");
            assert_eq!(stdlib_file_name(&id), Some(name));
        }
    }

    #[test]
    fn only_the_loader_spelling_is_a_stdlib_file() {
        for file in [
            "<stdlib:unified.rock>",
            "<bone-stdlib:unified.rock>",
            "<embedded-stdlib>/unified.rock",
            "stdlib/unified.rock",
            "/project/stdlib/unified.rock",
            "C:\\project\\stdlib\\unified.rock",
            "<stdlib>/",
            "<test>",
            "",
        ] {
            assert!(!is_stdlib_module_file(file), "{file}");
            assert_eq!(stdlib_file_name(file), None, "{file}");
        }
    }
}
