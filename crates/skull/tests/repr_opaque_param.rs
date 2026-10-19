//! `@repr(opaque)`: the parameter-level representation contract
//!.
//!
//! The 2.4 rejection fires on a parameter whose declared semantic type, under
//! the call's substitution, is a container with a column-stored element --
//! because an opaque body written against `[T]` reads a `_data` that column
//! storage does not have. Some parameters make no such assumption: `vec_push`'s
//! `item` is stored and never indexed, so a pushed COLUMN vec is not a layout
//! violation. There is no TYPE that says this: retyping `item` to `any`/`any?`
//! drops the inference edge that gives `vec_new()` its element -- measured,
//! `tests/coroutines.rock` loses `Coroutine<..>` and fails at `resume()`. Hence
//! a contract carried beside the type rather than in it.
//!
//! Scope: it waives ONE parameter of ONE rule. It is not a nobox escape, it does
//! not touch inference, mangling or specialization, and it is rejected where the
//! body is not opaque, because there is no assumption there to waive.
//!
//! Every registry violation carries kind `Generic`, so `tests/errors/` cannot
//! tell them apart from "unknown annotation `@repr`". That is why these assert
//! MESSAGES.

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

/// `vec_push`'s shape. `sink: [T]` names the vec layout; `item: T` is stored and
/// never indexed. Called with `T = [P]`: the sink is `[[P]]`, whose element is a
/// Vec and therefore boxed, while the payload is a COLUMN vec.
const PAYLOAD_ADMITTED: &str = r#"
value P { x: float, y: float }

@nomangle inline(lua) fn take<T>(sink: [T], @repr(opaque) item: T) -> int = """
    sink._data[1] = item
    return 1
"""

fn main() {
    let outer: [[P]] = []
    let inner: [P] = [P { x: 1.0, y: 2.0 }]
    print("{take(outer, inner)}")
}
"#;

/// The SAME signature, with the column container at the UNANNOTATED parameter.
/// `T = P` makes `sink` a column `[P]`, and the contract on `item` must not
/// reach it: the waiver is per parameter, not per call.
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

/// Without the contract the same payload rejects. This is the fixture the
/// annotation exists to change, and it must keep rejecting when it is absent.
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

/// The contract is about physical LAYOUT, not about the `nobox` guarantee. A
/// nobox value crossing a boxed call boundary must still reject with the
/// annotation present.
const NOBOX_NOT_BYPASSED: &str = r#"
nobox value NV { x: float, y: float }

@nomangle inline(lua) fn takes_nv(@repr(opaque) v: NV) -> int = """
    return 1
"""

fn main() { print("{takes_nv(NV { x: 1.0, y: 2.0 })}") }
"#;

/// The same source without the annotation: whatever `takes_nv` does, the
/// annotation must not change it. Compared against `NOBOX_NOT_BYPASSED`.
const NOBOX_CONTROL: &str = r#"
nobox value NV { x: float, y: float }

@nomangle inline(lua) fn takes_nv(v: NV) -> int = """
    return 1
"""

fn main() { print("{takes_nv(NV { x: 1.0, y: 2.0 })}") }
"#;

/// A REGISTRY rejection: recorded by bone during lowering, before emit.
fn registry_rejection(src: &str) -> String {
    let msgs = crate::common::bone_rejections(src);
    assert!(!msgs.is_empty(), "must reject, but bone recorded nothing");
    msgs.join(" | ")
}

/// A SKULL rejection: the 2.4 layout rule, raised at emit.
fn rejection(src: &str) -> String {
    crate::common::try_emit_module(src).expect_err("must reject").message
}

fn run(src: &str, tag: &str) -> String {
    let lua = crate::common::try_emit_module(src).expect("must emit");
    crate::common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

/// The outcome of emitting, as a comparable value, so two sources can be
/// asserted IDENTICAL rather than each asserted separately.
fn outcome(src: &str) -> Result<(), String> {
    crate::common::try_emit_module(src).map(|_| ()).map_err(|e| e.message)
}

#[test]
fn repr_opaque_on_a_rock_body_rejects() {
    let msg = registry_rejection(ON_A_ROCK_BODY);
    assert!(
        msg.contains("@repr") && msg.contains("opaque"),
        "must say the contract needs an opaque boundary, got: {msg}"
    );
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
    assert!(
        msg.contains("boxed") || msg.contains("opaque"),
        "must name the offending kind, got: {msg}"
    );
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
fn an_opaque_payload_is_admitted_and_correct() {
    assert_eq!(run(PAYLOAD_ADMITTED, "repr_opaque_payload"), "1");
}

/// The waiver is PER PARAMETER. Without this, annotating any parameter could
/// silently disarm the rule for the whole call.
#[test]
fn the_waiver_does_not_reach_the_unannotated_parameter() {
    let msg = rejection(EXEMPTS_ONLY_THAT_PARAM);
    assert!(
        msg.contains("column-stored `[P]`") && msg.contains("take"),
        "the unannotated container parameter must still reject, got: {msg}"
    );
}

/// The fixture the annotation exists to change. If this stopped rejecting, the
/// admission above would prove nothing.
#[test]
fn the_same_payload_without_the_contract_rejects() {
    assert!(rejection(UNANNOTATED_PAYLOAD_REJECTS).contains("column-stored `[P]`"));
}

/// Not a nobox escape: the annotated and unannotated sources must reach the
/// SAME outcome, whichever it is.
#[test]
fn repr_opaque_does_not_change_the_nobox_verdict() {
    assert_eq!(
        outcome(NOBOX_NOT_BYPASSED),
        outcome(NOBOX_CONTROL),
        "the contract waives the column-container rule only"
    );
}
