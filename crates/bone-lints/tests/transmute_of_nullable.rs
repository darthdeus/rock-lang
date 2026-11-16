//! transmute/cast-strips-nullability lint over bone HIR + inference. Unlike
//! unused_var this lint READS inference (arg + result nullability).
//! cast/transmute are defined locally (no stdlib needed) -- the lint matches
//! the callee name.

use crate::support::{lint_only, quoted};
use bone_lints::{Finding, TRANSMUTE_OF_NULLABLE};
use rock_core::errors::CompilerErrorLevel;

const PRELUDE: &str = r#"
inline(lua) fn cast<T>(value: any) -> T = "return value"
inline(lua) fn transmute<A, B>(x: A) -> B = "return x"

fn maybe_int() -> int? {
    return 1
}

fn always_int() -> int {
    return 1
}
"#;

fn findings(body: &str) -> Vec<Finding> {
    let src = format!("{PRELUDE}\n{body}\n");
    lint_only(&[("<transmute_of_nullable>", &src)], TRANSMUTE_OF_NULLABLE)
}

fn names(body: &str) -> Vec<String> {
    let mut ns: Vec<String> = findings(body).iter().map(quoted).collect();
    ns.sort();
    ns
}

#[test]
fn stripping_casts_are_flagged() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "transmute_of_nullable_int_is_flagged",
            "fn main() -> int {\n    let _x: int = transmute(maybe_int())\n    3\n}",
            "transmute",
        ),
        (
            "cast_strips_nullability_is_flagged",
            "fn main() -> int {\n    let _x: int = cast(maybe_int())\n    3\n}",
            "cast",
        ),
    ];
    for (name, body, callee) in cases {
        assert_eq!(names(body), [*callee], "{name}");
    }
}

#[test]
fn nullability_preserving_casts_are_not_flagged() {
    let cases: &[&str] = &[
        // cast<int?> preserves nullability, must not flag
        "fn main() -> int {\n    let _x: int? = cast(maybe_int())\n    3\n}",
        // non-nullable input has nothing to strip
        "fn main() -> int {\n    let _x: int = cast(always_int())\n    3\n}",
        // transmute of non-nullable must not flag
        "fn main() -> int {\n    let _x: int = transmute(always_int())\n    3\n}",
    ];
    for body in cases {
        let ns = names(body);
        assert!(ns.is_empty(), "{body}: {ns:?}");
    }
}

#[test]
fn nested_outer_strips_inner_preserves() {
    // inner cast target is `int?` (preserves) -> not flagged at the inner site;
    // outer cast target is `int` (strips) -> the single finding.
    let ns = names(
        "fn main() -> int {\n    let inner: int? = cast(maybe_int())\n    let _x: int = cast(inner)\n    3\n}",
    );
    assert_eq!(
        ns,
        vec!["cast".to_string()],
        "only the outermost strip is the bug: {ns:?}"
    );
}

#[test]
fn nullable_struct_transmute_is_flagged() {
    let fs = findings(
        "struct Mob { hp: int }\nfn maybe_mob() -> Mob? {\n    return Mob { hp: 10 }\n}\nfn main() -> int {\n    let _m: Mob = transmute(maybe_mob())\n    3\n}",
    );
    assert_eq!(fs.len(), 1, "expected one transmute(Mob?) finding: {fs:?}");
    assert!(
        fs[0].error.message.contains("from a `Mob?`"),
        "diagnostic names the inner type: {fs:?}"
    );
}

// Unlike every other lint in the catalog this one reports at `Error`: a
// laundered null-on-failure is a correctness hole, not a smell, so it fails the
// build rather than printing. The host coroutine library
// (`take_result`) is the standing proof that irreducible instances exist, which
// is why -- unlike `entity_across_yield` -- the allow comment stays legal.
#[test]
fn finding_reports_at_error_level() {
    let fs = findings("fn main() -> int {\n    let _x: int = cast(maybe_int())\n    3\n}");
    assert_eq!(fs.len(), 1, "expected the one cast finding: {fs:?}");
    assert_eq!(
        fs[0].level(),
        CompilerErrorLevel::Error,
        "transmute_of_nullable gates the build"
    );
}

#[test]
fn allow_comments_suppress() {
    let above = "fn main() -> int {\n    // rock: allow(transmute_of_nullable)\n    let _x: int = cast(maybe_int())\n    3\n}";
    assert_eq!(
        names(&above.replace("// rock: allow(transmute_of_nullable)", "")),
        ["cast"],
        "the finding exists without the comment"
    );
    assert!(names(above).is_empty(), "allow comment above silences it");
    let trailing = "fn main() -> int {\n    let _x: int = cast(maybe_int()) // rock: allow(transmute_of_nullable)\n    3\n}";
    assert!(
        names(trailing).is_empty(),
        "trailing allow comment silences it"
    );
}
