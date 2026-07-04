// Offset-stability sweep for references / rename / type-definition across the
// parser's synthetic braceless blocks (lambda `=> expr` bodies,
// `@`-trailing-lambda bodies, match-arm `=> expr` bodies).
//
// bone-lower used to derive a Block's two "brace marker" lapper intervals
// arithmetically from the block span (`start..start+1`, `stop-1..stop`) --
// valid only for spans that begin with `{` and end with `}`. Synthetic blocks
// span the inner expression instead, so the markers landed 1-wide on the FIRST
// and LAST characters of the body expression and won every plain
// `min_by_key(size)` node lookup: references/rename resolved the phantom Block
// and returned NOTHING at those offsets. Fixed by recording real `{`/`}` token
// spans on `BlockData` (`braces: Option<(Span, Span)>`) and emitting markers
// only from them (`alloc_block_expr`); synthetic blocks emit none. Every
// identifier offset must resolve identically to mid-identifier.

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/probe/main.rock";

fn pos_of(src: &str, needle: &str, extra: usize) -> Position {
    let idx = src.rfind(needle).expect("needle") + extra;
    let before = &src[..idx];
    let line = before.matches('\n').count() as u32;
    let col = (idx - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    Position::new(line, col)
}

struct Probe {
    refs: [usize; 3],
    renames: [usize; 3],
    type_defs: [bool; 3],
}

/// references / rename-edit / type-definition counts at the first char,
/// mid-identifier, and last char of `ident`'s last occurrence in `src`.
fn probe(src: &str, ident: &str) -> Probe {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());

    let at = [
        pos_of(src, ident, 0),
        pos_of(src, ident, 1),
        pos_of(src, ident, ident.len() - 1),
    ];

    let rn = |p| {
        bone.rename(PATH, p, "renamed")
            .and_then(|we| we.changes)
            .map(|c| c.values().map(|v| v.len()).sum::<usize>())
            .unwrap_or(0)
    };

    Probe {
        refs: at.map(|p| bone.references(PATH, p).len()),
        renames: at.map(rn),
        type_defs: at.map(|p| bone.type_definition(PATH, p).is_some()),
    }
}

const PREAMBLE: &str = "struct MarkerS { f: int }\nenum MarkerE { AAA, BBB }\n";

#[test]
fn phantom_block_markers_break_first_and_last_char() {
    // (context, source body, expected mid-identifier reference count)
    let cases: &[(&str, &str, usize)] = &[
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
    ];

    for (name, body, mid_refs) in cases {
        let full = format!("{PREAMBLE}{body}");
        let p = probe(&full, "handle");

        // Mid-identifier is unaffected by the phantom -- normal assertions.
        assert_eq!(p.refs[1], *mid_refs, "[{name}] mid-identifier references");
        assert_eq!(p.renames[1], *mid_refs, "[{name}] mid-identifier rename edits");
        assert!(p.type_defs[1], "[{name}] mid-identifier type-definition");

        // First/last char must resolve exactly like mid-identifier -- these
        // offsets used to hit the phantom Block marker and return nothing.
        assert_eq!(p.refs[0], *mid_refs, "[{name}] references at FIRST char");
        assert_eq!(p.refs[2], *mid_refs, "[{name}] references at LAST char");
        assert_eq!(p.renames[0], *mid_refs, "[{name}] rename at FIRST char");
        assert_eq!(p.renames[2], *mid_refs, "[{name}] rename at LAST char");
    }

    // The lambda body's synthetic-`return` block types as `never`, so the
    // phantom ALSO broke type-definition at the ends; now offset-stable.
    let lambda = format!(
        "{PREAMBLE}fn f(handle: MarkerS) {{\n  let g = () => handle\n}}\n"
    );
    let p = probe(&lambda, "handle");
    assert!(
        p.type_defs[0] && p.type_defs[2],
        "lambda-body type-definition resolves at first/last char"
    );

    // Control: a REAL braced body -- markers land on the braces, every
    // identifier offset resolves. Normal assertion; must never regress.
    let braced = format!(
        "{PREAMBLE}fn f(handle: MarkerS) {{\n  let g = () => {{ handle }}\n}}\n"
    );
    let p = probe(&braced, "handle");
    assert_eq!(p.refs, [2, 2, 2], "braced control: references offset-stable");
    assert_eq!(p.renames, [2, 2, 2], "braced control: rename offset-stable");
    assert_eq!(p.type_defs, [true, true, true], "braced control: type-def offset-stable");
}
