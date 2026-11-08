//! Why a rename must not run. A rename either rewrites every site that names
//! the symbol or none: a site Rock cannot see or rewrite (an opaque inline
//! body, a macro expansion, an external caller of a `@nomangle` name) turns a
//! partial edit into a silently broken program, so the rename is refused with
//! a reason the editor shows instead.

use rock_tokenizer::{TokenKind, Tokenizer};

/// A rename the server declines, with the reason shown to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameRefusal(pub String);

impl std::fmt::Display for RenameRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RenameRefusal {}

/// `Err` unless `new_name` lexes as exactly one identifier, so a rename never
/// writes a keyword or several tokens into every site.
pub(super) fn check_new_name(new_name: &str) -> Result<(), RenameRefusal> {
    let kinds = Tokenizer::from_code("<rename>", new_name).tokenize_kinds();
    match kinds.as_deref() {
        Ok([TokenKind::Ident(name)]) if name.as_str() == new_name => Ok(()),
        _ => Err(RenameRefusal(format!(
            "cannot rename to `{new_name}`: not an identifier"
        ))),
    }
}
