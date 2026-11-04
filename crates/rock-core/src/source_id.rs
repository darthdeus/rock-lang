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
