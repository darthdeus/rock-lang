//! Extended-ctags file emission.
//!
//! The bone workspace regenerates `.rock.tags` from the live session -- the
//! edited file's root on open (`write_ctags`), every open root on save
//! (`write_all_ctags`). The format matches the old compile-path
//! generator (`rock-cli/src/ctags.rs`): a `!_TAG_*` header followed by one
//! `name<TAB>file<TAB>line;"<TAB>kind` line per symbol, sorted by name.
//! Kinds:
//!   `f` function   `d` decorator   `M` macro
//!   `s` struct     `m` struct field (field `struct:<Name>`)
//!   `e` enum       `g` enum variant (field `enum:<Name>`)
//!   `t` newtype    `v` global
//! Extended `key:value` attributes follow the kind, in declaration order:
//! `signature:(a: int)`, `returns:int`, `type:int`, `struct:Point`, `enum:Color`.

use std::path::Path;

pub(crate) struct CtagsEntry {
    pub name: String,
    pub file: String,
    pub line: usize,
    pub kind: char,
    /// Extended `key:value` attributes appended after the kind, in order
    /// (e.g. `("signature", "(a: int)")`, `("returns", "int")`,
    /// `("type", "int")`, `("struct", "Point")`, `("enum", "Color")`).
    pub fields: Vec<(String, String)>,
}

/// Write `entries` to `path` in sorted extended-ctags format. Returns the number
/// of symbol lines written (excludes the header). Skips the write when the
/// rendered bytes match what's already on disk: the all-roots save sweep
/// re-renders every open package, but only the edited one's bytes actually
/// change, so rewriting the rest would churn mtime for nothing.
pub(crate) fn write_tags_file(path: &Path, mut entries: Vec<CtagsEntry>) -> std::io::Result<usize> {
    use std::fmt::Write as _;
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = String::new();
    out.push_str("!_TAG_FILE_FORMAT\t2\t/extended format/\n");
    out.push_str("!_TAG_FILE_SORTED\t1\t/0=unsorted, 1=sorted, 2=foldcase/\n");
    out.push_str("!_TAG_PROGRAM_AUTHOR\tRock Language\t//\n");
    out.push_str("!_TAG_PROGRAM_NAME\trock\t//\n");
    let _ = writeln!(out, "!_TAG_PROGRAM_VERSION\t{}\t//", env!("CARGO_PKG_VERSION"));

    for e in &entries {
        let _ = write!(out, "{}\t{}\t{};\"\t{}", e.name, e.file, e.line, e.kind);
        for (key, value) in &e.fields {
            let _ = write!(out, "\t{key}:{value}");
        }
        out.push('\n');
    }

    if fs_err::read_to_string(path).map(|cur| cur == out).unwrap_or(false) {
        return Ok(entries.len());
    }
    fs_err::write(path, out)?;
    Ok(entries.len())
}
