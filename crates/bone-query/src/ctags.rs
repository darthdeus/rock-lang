//! Extended-ctags file emission.
//!
//! The bone workspace regenerates a project's `.rock.tags` on save (driven from
//! `BoneWorkspace::write_ctags`). The format matches the old compile-path
//! generator (`rock-cli/src/ctags.rs`): a `!_TAG_*` header followed by one
//! `name<TAB>file<TAB>line;"<TAB>kind` line per symbol, sorted by name.
//! Kinds:
//!   `f` function   `d` decorator   `M` macro
//!   `s` struct     `m` struct field (field `struct:<Name>`)
//!   `e` enum       `g` enum variant (field `enum:<Name>`)
//!   `t` newtype    `v` global
//! Extended `key:value` attributes follow the kind, in declaration order:
//! `signature:(a: int)`, `returns:int`, `type:int`, `struct:Point`, `enum:Color`.

use std::io::Write;
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
/// of symbol lines written (excludes the header).
pub(crate) fn write_tags_file(path: &Path, mut entries: Vec<CtagsEntry>) -> std::io::Result<usize> {
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let file = fs_err::File::create(path)?;
    let mut w = std::io::BufWriter::new(file);

    writeln!(w, "!_TAG_FILE_FORMAT\t2\t/extended format/")?;
    writeln!(w, "!_TAG_FILE_SORTED\t1\t/0=unsorted, 1=sorted, 2=foldcase/")?;
    writeln!(w, "!_TAG_PROGRAM_AUTHOR\tRock Language\t//")?;
    writeln!(w, "!_TAG_PROGRAM_NAME\trock\t//")?;
    writeln!(w, "!_TAG_PROGRAM_VERSION\t{}\t//", env!("CARGO_PKG_VERSION"))?;

    for e in &entries {
        write!(w, "{}\t{}\t{};\"\t{}", e.name, e.file, e.line, e.kind)?;
        for (key, value) in &e.fields {
            write!(w, "\t{key}:{value}")?;
        }
        writeln!(w)?;
    }

    w.flush()?;
    Ok(entries.len())
}
