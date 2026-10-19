//! transmute/cast-strips-nullability lint over bone HIR + inference. Unlike
//! unused_var this lint READS inference (arg + result nullability), so the
//! harness runs a full `semantic_pass` and reads `ws.inference`. cast/transmute
//! are defined locally (no stdlib needed) -- the lint matches the callee name.

use bone_lints::{filter_suppressed, transmute_of_nullable, LintFinding, TransmuteOfNullable};
use bone_semantic::prelude::*;
use rock_core::errors::CompilerErrorLevel;

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module_clean(src, Ustr::from("<transmute_of_nullable>")).expect("parse")
}

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

fn findings(body: &str) -> Vec<TransmuteOfNullable> {
    let src = format!("{PRELUDE}\n{body}\n");
    let ws = semantic_pass(&mut [parse(&src)]).expect("clean program typechecks");
    transmute_of_nullable(&ws)
}

fn names(body: &str) -> Vec<String> {
    let mut ns: Vec<String> = findings(body).iter().map(|f| f.fn_name.to_string()).collect();
    ns.sort();
    ns
}

#[test]
fn transmute_of_nullable_int_is_flagged() {
    assert_eq!(names("fn main() -> int {\n    let _x: int = transmute(maybe_int())\n    3\n}"), vec!["transmute".to_string()]);
}

#[test]
fn cast_strips_nullability_is_flagged() {
    assert_eq!(names("fn main() -> int {\n    let _x: int = cast(maybe_int())\n    3\n}"), vec!["cast".to_string()]);
}

#[test]
fn cast_preserving_nullability_is_not_flagged() {
    let ns = names("fn main() -> int {\n    let _x: int? = cast(maybe_int())\n    3\n}");
    assert!(ns.is_empty(), "cast<int?> preserves nullability, must not flag: {ns:?}");
}

#[test]
fn cast_on_non_nullable_is_not_flagged() {
    let ns = names("fn main() -> int {\n    let _x: int = cast(always_int())\n    3\n}");
    assert!(ns.is_empty(), "non-nullable input has nothing to strip: {ns:?}");
}

#[test]
fn transmute_on_non_nullable_is_not_flagged() {
    let ns = names("fn main() -> int {\n    let _x: int = transmute(always_int())\n    3\n}");
    assert!(ns.is_empty(), "transmute of non-nullable must not flag: {ns:?}");
}

#[test]
fn nested_outer_strips_inner_preserves() {
    // inner cast target is `int?` (preserves) -> not flagged at the inner site;
    // outer cast target is `int` (strips) -> the single finding.
    let ns = names(
        "fn main() -> int {\n    let inner: int? = cast(maybe_int())\n    let _x: int = cast(inner)\n    3\n}",
    );
    assert_eq!(ns, vec!["cast".to_string()], "only the outermost strip is the bug: {ns:?}");
}

#[test]
fn nullable_struct_transmute_is_flagged() {
    let src = format!(
        "{PRELUDE}\nstruct Mob {{ hp: int }}\nfn maybe_mob() -> Mob? {{\n    return Mob {{ hp: 10 }}\n}}\nfn main() -> int {{\n    let _m: Mob = transmute(maybe_mob())\n    3\n}}\n"
    );
    let ws = semantic_pass(&mut [parse(&src)]).expect("typechecks");
    let fs = transmute_of_nullable(&ws);
    assert_eq!(fs.len(), 1, "expected one transmute(Mob?) finding: {fs:?}");
    assert_eq!(fs[0].inner_type, "Mob?", "diagnostic names the inner type: {fs:?}");
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
        fs[0].to_diagnostic().level,
        CompilerErrorLevel::Error,
        "transmute_of_nullable gates the build"
    );
}

#[test]
fn allow_comment_above_suppresses() {
    let body = "fn main() -> int {\n    // rock: allow(transmute_of_nullable)\n    let _x: int = cast(maybe_int())\n    3\n}";
    assert_eq!(names(body), vec!["cast".to_string()], "raw finding still exists");
    let src = format!("{PRELUDE}\n{body}\n");
    let kept = filter_suppressed(findings(body), &src);
    assert!(kept.is_empty(), "allow comment above silences it: {:?}", kept.iter().map(|f| f.fn_name.to_string()).collect::<Vec<_>>());
}

#[test]
fn trailing_allow_comment_suppresses() {
    let body = "fn main() -> int {\n    let _x: int = cast(maybe_int()) // rock: allow(transmute_of_nullable)\n    3\n}";
    let src = format!("{PRELUDE}\n{body}\n");
    let kept = filter_suppressed(findings(body), &src);
    assert!(kept.is_empty(), "trailing allow comment silences it: {kept:?}");
}
