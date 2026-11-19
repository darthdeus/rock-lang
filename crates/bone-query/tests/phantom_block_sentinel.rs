//! Synthetic braceless blocks must not win the node lookup at their body's first and last char.

use crate::fixture::{PATH, position_of_offset, workspace};

const PREAMBLE: &str = "struct MarkerS { f: int }\nenum MarkerE { AAA, BBB }\n";

#[test]
fn identifier_ends_resolve_like_its_middle_in_braceless_bodies() {
    // (case, body, references and rename edits)
    let cases: &[(&str, &str, usize)] = &[
        // The synthetic-`return` block types as `never`, so type-definition is checked at the ends too.
        (
            "braceless lambda body",
            "fn f(handle: MarkerS) {\n  let g = () => handle\n}\n",
            2,
        ),
        (
            "braceless match-arm body",
            "fn f(e: MarkerE, handle: MarkerS) -> MarkerS {\n  return match e {\n    MarkerE::AAA => handle\n    MarkerE::BBB => handle\n  }\n}\n",
            3,
        ),
        (
            "@-trailing-lambda body",
            "fn run(g: fn() -> MarkerS) -> MarkerS { g() }\nfn f(handle: MarkerS) {\n  let s = run() @ handle\n}\n",
            2,
        ),
        (
            "braced control",
            "fn f(handle: MarkerS) {\n  let g = () => { handle }\n}\n",
            2,
        ),
    ];
    for (case, body, count) in cases {
        let src = format!("{PREAMBLE}{body}");
        let bone = workspace(&src);
        let last = src.rfind("handle").unwrap();
        for (at, extra) in [("first", 0), ("mid", 1), ("last", 5)] {
            let p = position_of_offset(&src, last + extra);
            let renamed = bone
                .rename(PATH, p, "renamed")
                .expect("no refusal")
                .and_then(|we| we.changes)
                .map_or(0, |c| c.values().map(Vec::len).sum::<usize>());
            assert_eq!(
                bone.references(PATH, p).len(),
                *count,
                "{case}: references at {at}"
            );
            assert_eq!(renamed, *count, "{case}: rename edits at {at}");
            assert!(
                bone.type_definition(PATH, p).is_some(),
                "{case}: type-definition at {at}"
            );
        }
    }
}
