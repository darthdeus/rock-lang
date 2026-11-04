//! Foundation crate for the rock compiler: source positions (`Span`,
//! `LineCol`), diagnostics (`CompilerError`), the type representation
//! (`TypeRef`), symbols, scopes, node-id newtypes, and compile stats.
//! Deliberately holds nothing AST-shaped — the parser (`skull-parser`)
//! and `rock-tokenizer` both build on top of this.

pub mod code_repr;
pub mod compile_stats;
pub mod errors;
pub mod module_id;
pub mod nesting;
pub mod node_id;
pub mod scope;
pub mod shuffle;
pub mod source_code;
pub mod source_files;
pub mod source_id;
pub mod symbols;
pub mod types;
pub mod utils;

pub use code_repr::*;
pub use compile_stats::*;
pub use errors::*;
pub use module_id::*;
pub use node_id::*;
pub use scope::*;
pub use shuffle::*;
pub use source_code::*;
pub use source_files::*;
pub use source_id::*;
pub use symbols::*;
pub use types::*;
pub use utils::*;

mod prelude;

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::LazyLock;

    use regex_lite::Regex;

    static ANSI_SGR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\x1b\[[0-9;]*m").expect("valid ANSI SGR regex")
    });

    pub(crate) fn strip_ansi_sgr(text: &str) -> String {
        ANSI_SGR.replace_all(text, "").into_owned()
    }

    #[test]
    fn strips_sgr_without_changing_unicode_or_layout() {
        let rendered = "\x1b[31m\u{e9}\n   \x1b[38;5;246m\u{2502}\x1b[0m\n";
        assert_eq!(strip_ansi_sgr(rendered), "\u{e9}\n   \u{2502}\n");
    }

    #[test]
    fn leaves_plain_text_unchanged() {
        let rendered = "[E00] Error: plain text\n 1 \u{2502} let x = 1;\n";
        assert_eq!(strip_ansi_sgr(rendered), rendered);
    }
}
