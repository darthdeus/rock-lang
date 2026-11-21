use crate::common::bone_rejections;

#[test]
fn invalid_representation_annotations_preserve_diagnostics() {
    for (source, message) in [
        (
            "fn plain(@repr(opaque) v: int) -> int { v }".to_string(),
            "opaque",
        ),
        (
            "@repr(opaque) inline(lua) fn f(v: any) -> int = \"return 1\"".to_string(),
            "not allowed on a fn",
        ),
        (
            "struct Holder { @repr(opaque) a: int }".to_string(),
            "not allowed on a struct field",
        ),
        (
            "inline(lua) fn f(@repr v: any) -> int = \"return 1\"".to_string(),
            "@repr",
        ),
        (
            "inline(lua) fn f(@repr(boxed) v: any) -> int = \"return 1\"".to_string(),
            "opaque",
        ),
        (
            "inline(lua) fn f(@repr(\"opaque\") v: any) -> int = \"return 1\"".to_string(),
            "@repr",
        ),
        (
            "inline(lua) fn f(@repr(opaque, opaque) v: any) -> int = \"return 1\"".to_string(),
            "@repr",
        ),
    ] {
        let errors = bone_rejections(&source);
        assert!(errors.iter().any(|e| e.contains(message)), "{errors:?}");
    }
}
