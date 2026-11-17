use bone_lints::{KNOWN_LINTS, LintOptions, lint_workspace};
use bone_semantic::prelude::*;
use rock_core::errors::CompilerErrorLevel;

const FILE: &str = "<catalog>";
const PROGRAM: &str = "fn dead() -> int { 1 }\nfn main() {\n    let unread = 5\n}";

#[test]
fn catalog_findings() {
    let fs = crate::lint(&[(FILE, PROGRAM)], &Default::default(), true);
    let mut names: Vec<_> = fs.iter().map(|f| f.lint).collect();
    names.sort_unstable();
    assert_eq!(names, ["unused_fn", "unused_var"]);
    for f in fs {
        assert_eq!(f.file().as_str(), FILE);
        assert!(KNOWN_LINTS.contains(&f.lint));
        assert!(matches!(
            f.level(),
            CompilerErrorLevel::Warning | CompilerErrorLevel::Advice
        ));
    }
    let ws =
        semantic_pass(&mut [skull_parser::parse_module_clean(PROGRAM, Ustr::from(FILE)).unwrap()])
            .unwrap();
    let warm = ws.source_modules().map(|m| m.id).collect();
    let sources = [(Ustr::from(FILE), PROGRAM)].into_iter().collect();
    assert!(lint_workspace(&ws, &warm, true, &sources, &Default::default()).is_empty());
    assert!(
        crate::lint(
            &[(FILE, "fn dead() -> int { 1 }\nfn main() {}")],
            &Default::default(),
            false
        )
        .is_empty()
    );
}

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

#[test]
fn backend_sibling_mismatch_is_opt_in() {
    let src = "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: float) -> int = \"return x;\"\nfn main() {}";
    let lint = bone_lints::BACKEND_SIBLING_MISMATCH;
    assert!(
        crate::lint(&[(FILE, src)], &Default::default(), true)
            .iter()
            .all(|f| f.lint != lint)
    );
    let fs = crate::lint(&[(FILE, src)], &LintOptions::only([lint.to_owned()]), true);
    assert!(fs.iter().any(|f| f.lint == lint));
}
