//! Why a rename must not run. A rename either rewrites every site that names
//! the symbol or none: a site Rock cannot see or rewrite (an opaque inline
//! body, a macro expansion, an external caller of a `@nomangle` name) turns a
//! partial edit into a silently broken program, so the rename is refused with
//! a reason the editor shows instead.

use rock_tokenizer::{TokenKind, Tokenizer};

use bone_semantic::prelude::{Span, Workspace};
use skull_parser::ast::FnBackend;

use crate::query::{RefSite, RenameSubject, TextOf};

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

/// `Err` when code Rock cannot rewrite spells the old name. Rename edits only
/// Rock source, and the Lua or JS emitted for a program keeps some names as
/// written, so:
///
/// - a `@nomangle` or `extern` function, or a `@nomangle` struct, is named
///   by code outside the program (the host, other inline code); it is
///   refused outright;
/// - an `inline(lua)` / `inline(js)` body sees its own parameters by name,
///   and globals, struct fields and struct or enum names (runtime type tags)
///   from anywhere, so a body that spells the old name as a word refuses the
///   rename. Other functions are emitted under mangled names, which inline
///   code cannot call, so their renames go ahead.
///
/// The word match is textual: it cannot tell a Lua local that happens to
/// share the name from a real reference, and it refuses either way.
pub(super) fn check_opaque<'t>(
    ws: &Workspace,
    subject: RenameSubject,
    sites: &[RefSite],
    old: &str,
    text_of: TextOf<'_, 't>,
) -> Result<(), RenameRefusal> {
    let label = |span: &Span| {
        let file = span.file.as_str();
        text_of(file).map_or_else(
            || file.to_string(),
            |t| site_label(file, t, span.offset_range.0),
        )
    };
    let user_modules = ws
        .modules
        .iter()
        .filter(|m| !rock_stdlib::is_stdlib_module_file(m.file.as_str()));
    if let RenameSubject::Function(name) = subject {
        for m in ws.modules.iter() {
            for (_, f) in m.functions.iter().filter(|(_, f)| f.name == name) {
                let why = if f.annotations.iter().any(|a| a.is_builtin("nomangle")) {
                    "`@nomangle` keeps its emitted name, which code outside Rock calls"
                } else if f.is_extern {
                    "it is `extern`, bound to the external name it spells"
                } else {
                    continue;
                };
                return Err(RenameRefusal(format!(
                    "cannot rename `{old}`: {why} ({})",
                    label(&f.name_span)
                )));
            }
        }
    }
    if let RenameSubject::Type(Some(name)) = subject {
        for m in ws.modules.iter() {
            if let Some(s) = m.struct_by_name.get(&name).map(|i| &m.structs[*i]) {
                if s.nomangle {
                    return Err(RenameRefusal(format!(
                        "cannot rename `{old}`: `@nomangle` keeps its emitted type name, \
                         which code outside Rock uses ({})",
                        label(&s.name_span)
                    )));
                }
            }
        }
    }
    let reaches_any_body = matches!(
        subject,
        RenameSubject::Global(_) | RenameSubject::Field | RenameSubject::Type(_)
    );
    for m in user_modules {
        for (_, f) in m.functions.iter() {
            let Some((backend, body)) = &f.inline_body else {
                continue;
            };
            let (start, end) = f.span.offset_range;
            let in_decl = sites.iter().any(|s| {
                s.span.file == f.span.file
                    && start <= s.span.offset_range.0
                    && s.span.offset_range.0 < end
            });
            if !(in_decl || reaches_any_body) || !contains_word(body.as_str(), old) {
                continue;
            }
            let lang = match backend {
                FnBackend::Js => "js",
                _ => "lua",
            };
            return Err(RenameRefusal(format!(
                "cannot rename `{old}`: the opaque inline({lang}) body of `{}` ({}) \
                 names it, and rename cannot rewrite inline code",
                f.name,
                label(&f.name_span)
            )));
        }
    }
    Ok(())
}

/// Whether `word` occurs in `text` with no identifier character on either side.
fn contains_word(text: &str, word: &str) -> bool {
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let b = text.as_bytes();
    text.match_indices(word).any(|(i, _)| {
        let j = i + word.len();
        (i == 0 || !is_ident(b[i - 1])) && (j == b.len() || !is_ident(b[j]))
    })
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
