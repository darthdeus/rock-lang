//! Why a rename must not run. A rename either rewrites every site that names
//! the symbol or none: a site Rock cannot see or rewrite (an opaque inline
//! body, a macro expansion, an external caller of a `@nomangle` name) turns a
//! partial edit into a silently broken program, so the rename is refused with
//! a reason the editor shows instead.

use rock_tokenizer::{TokenKind, Tokenizer};

use crate::query::{RefSite, TextOf};

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

/// `file:line` of byte `offset` in `text`, naming the file by its last
/// component so the message stays short.
pub(super) fn site_label(file: &str, text: &str, offset: usize) -> String {
    let line = text
        .get(..offset)
        .map_or(0, |t| t.bytes().filter(|b| *b == b'\n').count());
    let name = std::path::Path::new(file)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(file);
    format!("{name}:{}", line + 1)
}

/// `Err` when a site does not spell `old` in the source. Macro expansion
/// retags the code it splices onto the invocation, so a use written inside a
/// macro body (`^count`, `_G.SCORE`) surfaces as a site over the call text;
/// rewriting it would corrupt the call and still leave the body stale.
pub(super) fn check_spellings<'t>(
    sites: &[RefSite],
    old: &str,
    text_of: TextOf<'_, 't>,
) -> Result<(), RenameRefusal> {
    for site in sites {
        let file = site.span.file.as_str();
        let Some(text) = text_of(file) else {
            continue;
        };
        let (start, end) = site.span.offset_range;
        if text.get(start..end) != Some(old) {
            return Err(RenameRefusal(format!(
                "cannot rename `{old}`: it is used through a macro expansion at {} \
                 that rename cannot rewrite",
                site_label(file, text, start)
            )));
        }
    }
    Ok(())
}
