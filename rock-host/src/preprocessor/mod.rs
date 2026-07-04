use anyhow::Result;
use ustr::Ustr;

pub mod lexer;
pub mod parser;
pub mod processor;

pub use processor::preprocess;

/// Preprocessor context — carries the backend identifier for `#if backend == "..."`
/// conditional compilation. The identifier is a string rather than an enum so the
/// preprocessor is decoupled from the compiler's Backend type (Rock currently only
/// targets Lua, but the preprocessor syntax allows other targets).
#[derive(Debug, Clone)]
pub struct PreprocessorContext {
    pub backend: Ustr,
}

impl PreprocessorContext {
    pub fn new(backend: impl Into<Ustr>) -> Self {
        Self {
            backend: backend.into(),
        }
    }

    /// Check if a condition is true for the current backend.
    /// Conditions are backend names compared case-sensitively.
    pub fn evaluate_condition(&self, condition: &str) -> bool {
        self.backend.as_str() == condition
    }
}

/// Preprocessor directive types
#[derive(Debug, Clone, PartialEq)]
pub enum DirectiveType {
    If,
    ElseIf,
    Else,
    End,
    Script,
    Test,
    TestUtil,
    Use,
}

/// A preprocessor directive found in the source
#[derive(Debug, Clone)]
pub struct Directive {
    pub directive_type: DirectiveType,
    pub condition: Option<String>,
    pub line: usize,
    pub column: usize,
}

/// Result of preprocessing a file
#[derive(Debug)]
pub struct PreprocessedSource {
    pub content: String,
    pub is_script: bool,
    pub is_test: bool,
    pub is_test_util: bool,
    pub used_packages: Vec<String>,  // Package names from #use directives
}
