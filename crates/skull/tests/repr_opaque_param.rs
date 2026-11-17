use crate::common::emit_rejection;

const ON_A_ROCK_BODY: &str = r#"
fn plain(@repr(opaque) v: int) -> int { v }
fn main() { print("{plain(1)}") }
"#;

const AT_ITEM_POSITION: &str = r#"
@repr(opaque) @nomangle inline(lua) fn f(v: any) -> int = """
    return 1
"""
fn main() { print("{f(1)}") }
"#;

const ON_A_STRUCT_FIELD: &str = r#"
struct Holder { @repr(opaque) a: int }
fn main() { print("{Holder { a: 1 }.a}") }
"#;

const NO_ARGUMENT: &str = r#"
@nomangle inline(lua) fn f(@repr v: any) -> int = """
    return 1
"""
fn main() { print("{f(1)}") }
"#;

const UNKNOWN_KIND: &str = r#"
@nomangle inline(lua) fn f(@repr(boxed) v: any) -> int = """
    return 1
"""
fn main() { print("{f(1)}") }
"#;

const NON_IDENT_KIND: &str = r#"
@nomangle inline(lua) fn f(@repr("opaque") v: any) -> int = """
    return 1
"""
fn main() { print("{f(1)}") }
"#;

const EXTRA_ARGUMENTS: &str = r#"
@nomangle inline(lua) fn f(@repr(opaque, opaque) v: any) -> int = """
    return 1
"""
fn main() { print("{f(1)}") }
"#;

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

const NOBOX_NOT_BYPASSED: &str = r#"
nobox value NV { x: float, y: float }

@nomangle inline(lua) fn takes_nv(@repr(opaque) v: NV) -> int = """
    return 1
"""

fn main() { print("{takes_nv(NV { x: 1.0, y: 2.0 })}") }
"#;

const NOBOX_CONTROL: &str = r#"
nobox value NV { x: float, y: float }

@nomangle inline(lua) fn takes_nv(v: NV) -> int = """
    return 1
"""

fn main() { print("{takes_nv(NV { x: 1.0, y: 2.0 })}") }
"#;

fn registry_rejection(src: &str) -> String {
    let msgs = crate::common::bone_rejections(src);
    assert!(!msgs.is_empty());
    msgs.join(" | ")
}

fn outcome(src: &str) -> Result<(), String> {
    crate::common::try_emit_module(src)
        .map(|_| ())
        .map_err(|e| e.message)
}

#[test]
fn repr_opaque_on_a_rock_body_rejects() {
    let msg = registry_rejection(ON_A_ROCK_BODY);
    assert!(msg.contains("@repr") && msg.contains("opaque"));
}

#[test]
fn repr_opaque_at_item_position_rejects() {
    assert!(registry_rejection(AT_ITEM_POSITION).contains("not allowed on a fn"));
}

#[test]
fn repr_opaque_on_a_struct_field_rejects() {
    assert!(registry_rejection(ON_A_STRUCT_FIELD).contains("not allowed on a struct field"));
}

#[test]
fn repr_without_an_argument_rejects() {
    assert!(registry_rejection(NO_ARGUMENT).contains("@repr"));
}

#[test]
fn repr_with_an_unknown_kind_rejects() {
    let msg = registry_rejection(UNKNOWN_KIND);
    assert!(msg.contains("boxed") || msg.contains("opaque"));
}

#[test]
fn repr_with_a_non_ident_kind_rejects() {
    assert!(registry_rejection(NON_IDENT_KIND).contains("@repr"));
}

#[test]
fn repr_with_extra_arguments_rejects() {
    assert!(registry_rejection(EXTRA_ARGUMENTS).contains("@repr"));
}

#[test]
fn the_waiver_does_not_reach_the_unannotated_parameter() {
    let msg = emit_rejection(EXEMPTS_ONLY_THAT_PARAM);
    assert!(msg.contains("column-stored `[P]`") && msg.contains("take"));
}

#[test]
fn the_same_payload_without_the_contract_rejects() {
    assert!(emit_rejection(UNANNOTATED_PAYLOAD_REJECTS).contains("column-stored `[P]`"));
}

#[test]
fn repr_opaque_does_not_change_the_nobox_verdict() {
    assert_eq!(outcome(NOBOX_NOT_BYPASSED), outcome(NOBOX_CONTROL));
}
