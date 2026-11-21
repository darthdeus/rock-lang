use bone_lints::{KNOWN_LINTS, lint_workspace};
use bone_semantic::prelude::*;
use rock_core::errors::CompilerErrorLevel;

const FILE: &str = "<catalog>";
const PROGRAM: &str = "fn dead() -> int { 1 }\nfn main() {\n    let unread = 5\n}";

#[test]
fn catalog_suppression() {
    let src = "// rock: allow(unused_fn)\nfn dead() -> int { 1 }\nfn main() {\n// rock: allow(unused_var)\nlet unread = 5\n}";
    assert!(crate::lint(&[(FILE, src)], &Default::default(), true).is_empty());
    for name in ["unused_var", "pure"] {
        let src = format!("// rock: allow({name})\nfn dead() -> int {{ 1 }}\nfn main() {{}}");
        let fs = crate::lint(&[(FILE, &src)], &Default::default(), true);
        assert_eq!(fs.iter().map(|f| f.lint).collect::<Vec<_>>(), ["unused_fn"]);
    }
}
