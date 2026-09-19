//! `@derived` marks a rebuildable cache field that no state walker captures.
//! The compiler's whole contribution is the flag on the field's
//! `_G.__ROCK_TYPE_TABLE` entry: every runtime walker keys that table by the
//! value's `__ty`, so the flag is the channel the skip reads. These tests pin
//! that the flag lands on exactly the annotated field and that an ordinary
//! field's entry is untouched.

use crate::common::emit_module;

const SRC: &str = r#"
struct BoardIndex {
    count: int = 0,
}

struct Board {
    width: int = 8,
    @derived cache: BoardIndex? = null,
}

fn main() {
    let b = Board { }
    print("{b.width}")
}
"#;

fn board_entry(lua: &str) -> String {
    lua.lines()
        .find(|line| line.contains("_G.__ROCK_TYPE_TABLE[\"Board\"]"))
        .unwrap_or_else(|| panic!("no Board type table entry in:\n{lua}"))
        .to_string()
}

#[test]
fn a_derived_field_is_flagged_in_the_type_table() {
    let entry = board_entry(&emit_module(SRC));
    let cache = entry
        .split("{ name = ")
        .find(|field| field.starts_with("\"cache\""))
        .unwrap_or_else(|| panic!("no `cache` field in: {entry}"));
    assert!(
        cache.contains("derived = true"),
        "the annotated field must carry the flag walkers read: {cache}"
    );
    assert!(
        cache.contains("nullable = true"),
        "a derived field is nullable, so an absent key reads as null: {cache}"
    );
}

#[test]
fn an_ordinary_field_carries_no_flag() {
    let entry = board_entry(&emit_module(SRC));
    let width = entry
        .split("{ name = ")
        .find(|field| field.starts_with("\"width\""))
        .unwrap_or_else(|| panic!("no `width` field in: {entry}"));
    assert!(
        !width.contains("derived"),
        "only the annotated field is skipped, so only it carries the flag: {width}"
    );
}

/// A struct with no derived field must emit exactly what it emitted before the
/// annotation existed: the walkers hoist one lookup per type, so a stray key
/// here would be a per-object cost on every capture.
#[test]
fn an_unannotated_struct_emits_an_unchanged_entry() {
    let lua = emit_module(
        r#"
struct Plain {
    width: int = 8,
    slot: int? = null,
}

fn main() {
    let p = Plain { }
    print("{p.width}")
}
"#,
    );
    let entry = lua
        .lines()
        .find(|line| line.contains("_G.__ROCK_TYPE_TABLE[\"Plain\"]"))
        .unwrap_or_else(|| panic!("no Plain entry in:\n{lua}"));
    assert!(!entry.contains("derived"), "unannotated entry gained a key: {entry}");
}
