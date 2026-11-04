//! Removed global markers use the ordinary unknown-annotation rejection.

#[test]
fn removed_global_markers_are_unknown_item_annotations() {
    for (name, src) in [
        (
            "guarded",
            "@guarded global COUNTER: int = 0\nfn main() { COUNTER += 1 }",
        ),
        (
            "global_write",
            "global COUNTER: int = 0\n@global_write fn main() { COUNTER += 1 }",
        ),
    ] {
        let errors = crate::common::bone_rejections(src);
        assert!(
            errors
                .iter()
                .any(|error| error.contains(&format!("unknown annotation `@{name}`"))),
            "@{name} must reach annotation validation: {errors:?}"
        );
    }
}

#[test]
fn statement_annotation_prefixes_are_unknown() {
    for name in ["guarded", "global_write", "totally_unknown_anno"] {
        for write in ["COUNTER = 1", "COUNTER += 1"] {
            let src =
                format!("global COUNTER: int = 0\nfn main() {{ COUNTER = 0\n@{name} {write}\n}}");
            let error = skull_parser::parse_module_clean(&src, ustr::Ustr::from("test.rock"))
                .expect_err("statement annotation must reject");
            assert_eq!(
                error.message,
                format!("unknown annotation `@{name}` on a statement")
            );
        }
    }
}
