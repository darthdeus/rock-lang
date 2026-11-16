//! Workspace fixtures and position helpers shared by the query tests.

#![allow(dead_code)]

use bone_query::BoneWorkspace;
use lsp_types::{Diagnostic, HoverContents, Position, Range, SemanticToken, TextEdit};

/// Delta-decoded semantic tokens as `(line, start column, token)`.
pub fn decode_tokens(data: &[SemanticToken]) -> Vec<(u32, u32, SemanticToken)> {
    let (mut line, mut col) = (0, 0);
    data.iter()
        .map(|t| {
            if t.delta_line != 0 {
                line += t.delta_line;
                col = t.delta_start;
            } else {
                col += t.delta_start;
            }
            (line, col, *t)
        })
        .collect()
}

#[cfg(not(windows))]
pub const PATH: &str = "/tmp/rock_lsp_bone_test_main.rock";
#[cfg(windows)]
pub const PATH: &str = r"C:\scratch\rock_lsp_bone_test_main.rock";

/// A workspace holding `src` at `path`, outside any project.
pub fn workspace_at(path: &str, src: &str) -> BoneWorkspace {
    let mut bone = BoneWorkspace::new();
    bone.update(path, src.to_string());
    bone
}

/// A workspace holding `src` at [`PATH`].
pub fn workspace(src: &str) -> BoneWorkspace {
    workspace_at(PATH, src)
}

/// The diagnostics `update` returned for `path`.
pub fn diagnostics_in(per_file: Vec<(String, Vec<Diagnostic>)>, path: &str) -> Vec<Diagnostic> {
    per_file
        .into_iter()
        .filter(|(p, _)| p == path)
        .flat_map(|(_, d)| d)
        .collect()
}

/// The diagnostics a fresh workspace publishes for `src` at [`PATH`].
pub fn published(src: &str) -> Vec<Diagnostic> {
    diagnostics_in(BoneWorkspace::new().update(PATH, src.to_string()), PATH)
}

/// A project written to a temporary directory; files may sit in subdirectories.
pub struct Project {
    pub dir: tempfile::TempDir,
}

impl Project {
    pub fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for (rel, text) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Self { dir }
    }

    /// The workspace's canonical key for `rel`. Per-file diagnostics are keyed by
    /// `canonicalize_lenient`, which differs from `std::fs::canonicalize` on
    /// Windows.
    pub fn path(&self, rel: &str) -> String {
        bone_query::canonicalize_lenient(&self.dir.path().join(rel).to_string_lossy())
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(rel)).unwrap()
    }

    /// Opens `rel` in a fresh workspace with its on-disk text, as didOpen does.
    pub fn open(&self, rel: &str) -> (BoneWorkspace, Vec<(String, Vec<Diagnostic>)>) {
        let mut bone = BoneWorkspace::new();
        let per_file = bone.update(&self.path(rel), self.read(rel));
        (bone, per_file)
    }
}

/// The position of byte offset `idx` in `src`, in UTF-16 columns as LSP counts.
pub fn position_of_offset(src: &str, idx: usize) -> Position {
    let before = &src[..idx];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Position::new(
        before.matches('\n').count() as u32,
        src[line_start..idx].encode_utf16().count() as u32,
    )
}

/// The position `extra` bytes into the first occurrence of `needle`.
#[track_caller]
pub fn pos(src: &str, needle: &str, extra: usize) -> Position {
    let idx = src
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not in source"));
    position_of_offset(src, idx + extra)
}

/// The byte offset of an LSP position in `src`.
pub fn offset_of(src: &str, p: Position) -> usize {
    let line_start: usize = src
        .split_inclusive('\n')
        .take(p.line as usize)
        .map(str::len)
        .sum();
    let line = src[line_start..].split_inclusive('\n').next().unwrap_or("");
    let mut units = 0;
    for (i, c) in line.char_indices() {
        if units >= p.character as usize {
            return line_start + i;
        }
        units += c.len_utf16();
    }
    line_start + line.len()
}

/// The text `range` covers in `src`.
pub fn covered(src: &str, range: Range) -> &str {
    &src[offset_of(src, range.start)..offset_of(src, range.end)]
}

/// `src` with `edits` applied.
pub fn apply_edits(src: &str, mut edits: Vec<TextEdit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse((e.range.start.line, e.range.start.character)));
    let mut out = src.to_string();
    for e in edits {
        let (a, b) = (offset_of(&out, e.range.start), offset_of(&out, e.range.end));
        out.replace_range(a..b, &e.new_text);
    }
    out
}

/// The markdown of the hover at `pos`, if any.
#[track_caller]
pub fn hover_markup(bone: &BoneWorkspace, path: &str, pos: Position) -> Option<String> {
    bone.hover(path, pos).map(|h| match h.contents {
        HoverContents::Markup(m) => m.value,
    })
}
