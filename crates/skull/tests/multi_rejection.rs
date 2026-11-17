use crate::common::try_emit_module;

fn messages(err: &skull_parser::CompilerError) -> Vec<&str> {
    std::iter::once(err.message.as_str())
        .chain(err.siblings.iter().map(|s| s.message.as_str()))
        .collect()
}

fn messages_owned(src: &str) -> Vec<String> {
    let err = try_emit_module(src).expect_err("must reject");
    messages(&err).iter().map(|m| m.to_string()).collect()
}

const THREE_SINKS: &str = r#"
nobox value NV2 { x: float, y: float }
global XS: [NV2] = []

fn sink_one() { let t = (XS[0], 1.0) print("{t}") }
fn sink_two() { let t = (XS[1], 2.0) print("{t}") }
fn sink_three() { let t = (XS[2], 3.0) print("{t}") }

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
    assert_eq!(all.len(), 3);

    // Each names its own function: the point of reporting all of them is to get
    // a SITE TABLE, so three copies of one message would be no better than one.
    for want in ["sink_one", "sink_two", "sink_three"] {
        assert!(all.iter().any(|m| m.contains(want)));
    }
}

const TWO_FAILING_SPECIALIZATIONS: &str = r#"
nobox value NV2 { x: float, y: float }
global XS: [NV2] = []

fn tuple_in_coll<T>(xs: [T], k: int) -> int {
    let t = (xs[k], 1.0)
    print("{t}")
    return 0
}

fn pair<T>(xs: [T], k: int) -> (T, float)? {
    if k < 0 { return null }
    return (xs[k], 1.0)
}

fn nullable_pair<T>(xs: [T], k: int) -> int {
    let p = pair(xs, k)
    print("{p}")
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
    // `pair` splats its `(T, float)?` return (presence bool then the slots) and
    // `nullable_pair` binds that wire, so neither materializes on the way. The
    // second rejection is the whole-tuple ESCAPE into the interpolation, which is
    // the guarantee firing by design rather than an unbuilt mechanism -- so it
    // stays a rejection, and the property under test (BOTH specializations get
    // reported, not just the first) keeps its two sites.
    for want in ["specialized `tuple_in_coll", "specialized `nullable_pair"] {
        assert!(all.iter().any(|m| m.contains(want)));
    }
}

#[test]
fn the_rejection_order_is_deterministic() {
    let first = messages_owned(TWO_FAILING_SPECIALIZATIONS);
    for _ in 0..4 {
        assert_eq!(first, messages_owned(TWO_FAILING_SPECIALIZATIONS));
    }
}

#[test]
fn a_failing_global_init_does_not_mask_a_failing_function() {
    const BOTH: &str = r#"
nobox value NV2 { x: float, y: float }
global XS: [NV2] = []
global G: string = "{(NV2 { x: 1.0, y: 2.0 }, 1.0)}"

fn sink_fn() { let t = (XS[0], 1.0) print("{t}") }

fn main() { sink_fn() print(G) }
"#;
    let err = try_emit_module(BOTH).expect_err("both definitions must reject");
    let all = messages(&err);
    for want in ["__skull_ginit_G", "sink_fn"] {
        assert!(all.iter().any(|m| m.contains(want)));
    }
}

#[test]
fn a_lone_rejection_has_no_siblings() {
    const ONE_SINK: &str = r#"
nobox value NV2 { x: float, y: float }
global XS: [NV2] = []
fn sink_one() { let t = (XS[0], 1.0) print("{t}") }
fn main() { sink_one() }
"#;
    let err = try_emit_module(ONE_SINK).expect_err("the nobox sink must reject");
    assert!(err.siblings.is_empty());
}

#[test]
fn a_lowering_rejection_points_at_the_expression() {
    // Generic, so type-check accepts it and only the `string` instance fails.
    let src = r#"
fn bump<T>(x: T) -> T {
    x++
    return x
}
fn main() { print(bump("a")) }
"#;
    let err = try_emit_module(src).expect_err("a string has no `++`");
    assert!(err.message.contains("inc/dec"));
    assert_eq!(err.location.offset, src.find("x++").expect("fixture line"));
}
