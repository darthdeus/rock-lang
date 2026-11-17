use crate::common::{bone_rejections, emit_rejection, try_emit_module};

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

const EXEMPTS_ONLY_THAT_PARAM: &str = r#"
value P { x: float, y: float }

@nomangle inline(lua) fn take<T>(sink: [T], @repr(opaque) item: T) -> int = """
    sink._data[1] = item
    return 1
"""

fn main() {
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    print("{take(xs, P { x: 3.0, y: 4.0 })}")
}
"#;

const UNANNOTATED_PAYLOAD_REJECTS: &str = r#"
value P { x: float, y: float }

@nomangle inline(lua) fn take<T>(sink: [T], item: T) -> int = """
    sink._data[1] = item
    return 1
"""

fn main() {
    let outer: [[P]] = []
    let inner: [P] = [P { x: 1.0, y: 2.0 }]
    print("{take(outer, inner)}")
}
"#;

#[test]
fn representation_waivers_stay_local_and_do_not_bypass_nobox() {
    for source in [EXEMPTS_ONLY_THAT_PARAM, UNANNOTATED_PAYLOAD_REJECTS] {
        assert!(emit_rejection(source).contains("column-stored `[P]`"));
    }
    let source = r#"
nobox value NV { x: float, y: float }
@nomangle inline(lua) fn takes_nv(@repr(opaque) v: NV) -> int = "return 1"
fn main() { print(takes_nv(NV { x: 1.0, y: 2.0 })) }
"#;
    let outcome = |source| try_emit_module(source).map(|_| ()).map_err(|e| e.message);
    assert_eq!(
        outcome(source),
        outcome(&source.replace("@repr(opaque) ", ""))
    );
}
