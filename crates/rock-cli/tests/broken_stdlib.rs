//! A stdlib that does not parse is a reported error naming the file, never a
//! panic: the stdlib can be read from a directory a developer is editing.
use crate::support::{rock, text};
use std::path::Path;

/// A copy of the checkout's stdlib with `text` appended to `vec.rock`.
fn stdlib_with(dir: &Path, text: &str) {
    let source = crate::support::repo_root().join("stdlib");
    for name in rock_stdlib::STDLIB_NAMES {
        let mut contents = std::fs::read_to_string(source.join(name)).unwrap();
        if *name == "vec.rock" {
            contents.push_str(text);
        }
        std::fs::write(dir.join(name), contents).unwrap();
    }
}
