use std::str::FromStr;
use ustr::Ustr;

use crate::ScopeId;

/// Represents a unique module identifier based on the file path
/// We use an Option internally to allow for const construction
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleId(pub Option<Ustr>);

impl ModuleId {
    /// A special module ID for builtin/well-known types
    pub const BUILTIN: ModuleId = ModuleId(None);

    pub fn new(path: Ustr) -> Self {
        Self(Some(path))
    }

    pub fn from_path(path: &str) -> Self {
        Self(Some(Ustr::from(path)))
    }

    pub fn as_str(&self) -> &str {
        match self.0 {
            Some(s) => s.as_str(),
            None => "__builtin__",
        }
    }

    pub fn as_ustr(&self) -> Option<Ustr> {
        self.0
    }

    pub fn is_builtin(&self) -> bool {
        self.0.is_none()
    }

    pub fn is_stdlib(&self) -> bool {
        self.as_str().contains("stdlib")
    }

    pub fn scope(&self) -> ScopeId {
        ScopeId::Root(*self)
    }
}

impl std::fmt::Display for ModuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for ModuleId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Some(Ustr::from(s))))
    }
}
