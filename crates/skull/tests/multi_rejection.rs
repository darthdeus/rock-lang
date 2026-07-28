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

    let all: Vec<&skull_parser::CompilerError> =
        std::iter::once(&err).chain(err.siblings.iter()).collect();
    assert_eq!(
        all.len(),
        3,
        "expected one rejection per sink function, got {}:\n{}",
        all.len(),
        all.iter().map(|e| e.message.as_str()).collect::<Vec<_>>().join("\n")
    );

    // Each names its own function: the point of reporting all of them is to get
    // a SITE TABLE, so three copies of one message would be no better than one.
    for want in ["sink_one", "sink_two", "sink_three"] {
        assert!(
            all.iter().any(|e| e.message.contains(want)),
            "no rejection names `{want}`:\n{}",
            all.iter().map(|e| e.message.as_str()).collect::<Vec<_>>().join("\n")
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
