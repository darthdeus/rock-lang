//! Emission reports EVERY rejection, not just the first one it reaches.
//!
//! Bodies are lowered in parallel and each result is stored, so all rejections
//! are already computed by the time one is returned -- `assemble` was simply
//! bailing at the first chunk it pulled that held an `Err`. Reading that one
//! error as "the program has one problem" has produced four wrong counts on
//! this branch (a three-sink list that was really twenty, a "zero rejections"
//! measured through a panic, a "clears three of five games" prediction, and a
//! whole class hidden behind another). Anyone counting sinks should be able to
//! read the count off the output instead of instrumenting the verifier.

mod common;
use common::try_emit_module;

/// Every reported rejection: the primary plus its siblings (the shape every
/// renderer expands).
fn messages(err: &skull_parser::CompilerError) -> Vec<&str> {
    std::iter::once(err.message.as_str())
        .chain(err.siblings.iter().map(|s| s.message.as_str()))
        .collect()
}

fn messages_owned(src: &str) -> Vec<String> {
    let err = try_emit_module(src).expect_err("must reject");
    messages(&err).iter().map(|m| m.to_string()).collect()
}

/// Three DISTINCT functions, each with its own nobox materialization. Distinct
/// fns matter: a whole-body diagnostic reports once per body, so three
/// violations in one body could pass a naive "more than one error" check while
/// still hiding two sites.
const THREE_SINKS: &str = r#"
nobox value NV2 { x: float, y: float }
value E5 { pos: NV2, a: float, b: float, c: float }
value Opt { pos: NV2?, a: float, b: float, c: float }

global G: Opt? = null
global H: Opt? = null
global I: Opt? = null

fn sink_one() { G = Opt { pos: NV2 { x: 1.0, y: 2.0 }, a: 1.0, b: 2.0, c: 3.0 } }
fn sink_two() { H = Opt { pos: NV2 { x: 3.0, y: 4.0 }, a: 4.0, b: 5.0, c: 6.0 } }
fn sink_three() { I = Opt { pos: NV2 { x: 5.0, y: 6.0 }, a: 7.0, b: 8.0, c: 9.0 } }

fn main() {
    sink_one()
    sink_two()
    sink_three()
}
"#;

#[test]
fn every_rejection_is_reported_not_just_the_first() {
    let err = try_emit_module(THREE_SINKS).expect_err("three nobox sinks must reject");

    let all = messages(&err);
    assert_eq!(
        all.len(),
        3,
        "expected one rejection per sink function, got {}:\n{}",
        all.len(),
        all.join("\n")
    );

    // Each names its own function: the point of reporting all of them is to get
    // a SITE TABLE, so three copies of one message would be no better than one.
    for want in ["sink_one", "sink_two", "sink_three"] {
        assert!(
            all.iter().any(|m| m.contains(want)),
            "no rejection names `{want}`:\n{}",
            all.join("\n")
        );
    }
}

/// Two independent SPECIALIZATIONS, each rejecting inside its own instantiated
/// body. The specialization worklist used to bail at the first one, so the
/// sibling was invisible until the first was fixed -- which is how all three
/// members of the column-read class were found one behind another.
///
/// The rejections have to land in the specialized copies, not in the caller: a
/// `co fn` never runs an inline pass, so each generic call there needs a
/// standalone specialized definition, and the `int` results keep the nobox value
/// out of the coroutine body itself.
const TWO_FAILING_SPECIALIZATIONS: &str = r#"
nobox value NV2 { x: float, y: float }
global XS: [NV2] = []

fn tuple_in_coll<T>(xs: [T], k: int) -> int {
    let ts: [(T, float)] = []
    ts.push((xs[k], 1.0))
    return ts.len
}

fn pair<T>(xs: [T], k: int) -> (T, float)? {
    if k < 0 { return null }
    return (xs[k], 1.0)
}

fn nullable_pair<T>(xs: [T], k: int) -> int {
    let p = pair(xs, k)
    if p != null { return 1 }
    return 0
}

co fn walker() -> Coroutine<int> {
    yield tuple_in_coll(XS, 0)
    yield nullable_pair(XS, 0)
}

fn main() { XS.push(NV2{x:1.0,y:2.0}); let c = walker(); print("{c.resume() ?? -1}") }
"#;

#[test]
fn every_failing_specialization_is_reported_not_just_the_first() {
    let err =
        try_emit_module(TWO_FAILING_SPECIALIZATIONS).expect_err("both specializations must reject");
    let all = messages(&err);
    // `pair` itself now SPLATS its `(T, float)?` return (presence bool then the
    // slots), so the nobox value no longer materializes there; the rejection moved
    // one frame out to `nullable_pair`, which packs the result to test it against
    // null. Two failing specializations either way -- which is the property.
    for want in ["specialized `tuple_in_coll", "specialized `nullable_pair"] {
        assert!(
            all.iter().any(|m| m.contains(want)),
            "no rejection names `{want}`:\n{}",
            all.join("\n")
        );
    }
}

/// The reported ORDER is fixed. Specializations come off a worklist and chunks
/// out of a hash map, so an unordered report would make any test asserting the
/// primary flaky rather than wrong.
#[test]
fn the_rejection_order_is_deterministic() {
    let first = messages_owned(TWO_FAILING_SPECIALIZATIONS);
    for _ in 0..4 {
        assert_eq!(first, messages_owned(TWO_FAILING_SPECIALIZATIONS));
    }
}

/// A rejection in a GLOBAL INITIALIZER used to abort before any function chunk
/// was pulled, so it masked every rejection in the program's functions -- a
/// different kind of definition hiding the whole rest of the census.
#[test]
fn a_failing_global_init_does_not_mask_a_failing_function() {
    const BOTH: &str = r#"
nobox value NV2 { x: float, y: float }
value Opt { pos: NV2?, a: float, b: float, c: float }

global G: Opt? = Opt { pos: NV2 { x: 1.0, y: 2.0 }, a: 1.0, b: 2.0, c: 3.0 }
global H: Opt? = null

fn sink_fn() { H = Opt { pos: NV2 { x: 3.0, y: 4.0 }, a: 4.0, b: 5.0, c: 6.0 } }

fn main() { sink_fn() }
"#;
    let err = try_emit_module(BOTH).expect_err("both definitions must reject");
    let all = messages(&err);
    for want in ["__skull_ginit_G", "sink_fn"] {
        assert!(
            all.iter().any(|m| m.contains(want)),
            "no rejection names `{want}`:\n{}",
            all.join("\n")
        );
    }
}

/// A single rejection must not grow a spurious sibling -- the common case still
/// reports exactly one error.
#[test]
fn a_lone_rejection_has_no_siblings() {
    const ONE_SINK: &str = r#"
nobox value NV2 { x: float, y: float }
value Opt { pos: NV2?, a: float, b: float, c: float }
global G: Opt? = null
fn sink_one() { G = Opt { pos: NV2 { x: 1.0, y: 2.0 }, a: 1.0, b: 2.0, c: 3.0 } }
fn main() { sink_one() }
"#;
    let err = try_emit_module(ONE_SINK).expect_err("the nobox sink must reject");
    assert!(
        err.siblings.is_empty(),
        "a lone rejection carries no siblings, got {}",
        err.siblings.len()
    );
}
