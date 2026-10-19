//! Seeded adversarial mutation harness for the `semantic_tokens` surface.
//!
//! The legacy rock-lsp `adversarial_semantic_tokens` fuzz module died with the
//! old completion code (docs/unfuck-backlog/compiler-codegen-lsp.md #24). This
//! is its bone-side replacement: instead of spawning a `rock lsp` subprocess, it
//! drives `BoneWorkspace::semantic_tokens` (and its range form) in-process while
//! a committed, printed LCG applies a bounded number of source edits, and checks
//! every emitted token range against an independent line oracle plus the
//! production `LineIndex`.
//!
//! Contract (docs/span-position-units.md): 0-based lines, char-counted columns.
//! A mutation may take the token count to zero -- parse errors are fine -- but a
//! crash, an out-of-bounds range or a range that disagrees with the source is a
//! bug. A failing step prints the case, RNG seed and the exact edit sequence,
//! then rewrites the case's seed source into the fixture before re-raising, so a
//! failed mutation sequence cannot leak into the next case.

use std::fmt::Write as _;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

use bone_query::{
    offset_at_line_col, offset_to_position, semantic_tokens_legend, BoneWorkspace, LineIndex,
};
use lsp_types::{Position, Range};

use super::{decode_tokens, decode_workspace, DecodedToken, PATH};

/// Fixed bound on the mutations applied to one corpus case. Bounded so the
/// harness stays in the `just lsp-fuzz` time envelope; the seed, not the count,
/// is what a repro needs.
const MUTATIONS: usize = 32;

/// One committed corpus case: display name, committed RNG seed and the source
/// every edit sequence starts from. Seeds are literals (never read from the
/// environment) so a failure reproduces from the checkout alone.
struct Case {
    name: &'static str,
    rng_seed: u64,
    source: &'static str,
}

/// Cases chosen for the shapes range arithmetic gets wrong: multibyte scalars,
/// CRLF endings, comment text (including the magic-directive recolor), partial
/// syntax, and the interpolation/name-classification paths.
const CASES: &[Case] = &[
    Case {
        name: "multibyte-comments-strings",
        rng_seed: 0x5EED_0001,
        source: concat!(
            "// \u{2014} \u{4e2d} \u{e9} comment\n",
            "struct FixVec2 { x: int, y: int }\n",
            "fn run(v: FixVec2) -> int {\n",
            "  let s = \"h\u{e9}llo \u{4e16}\u{754c} \u{2014}\"\n",
            "  return v.x\n",
            "}\n",
        ),
    },
    Case {
        name: "crlf-line-endings",
        rng_seed: 0x5EED_0002,
        source: concat!(
            "fn run(x: int) -> int {\r\n",
            "  // \u{e9} comment\r\n",
            "  let s = \"h\u{e9}llo\"\r\n",
            "  return x\r\n",
            "}\r\n",
        ),
    },
    Case {
        name: "comments-and-directives",
        rng_seed: 0x5EED_0003,
        source: concat!(
            "// rock: allow(unused_var)\n",
            "// plain \u{2014} comment \u{4e2d}\n",
            "fn run() {\n",
            "  let x = 1 /* inline */\n",
            "  /* block\n",
            "     comment */\n",
            "  // trailing without newline",
        ),
    },
    Case {
        name: "incomplete-syntax",
        rng_seed: 0x5EED_0004,
        source: concat!(
            "fn f(\n",
            "struct S {\n",
            "  field: \n",
            "enum E { A,\n",
            "fn g() { let s = \"unterminated\n",
            "  if x { \n",
            "}\n",
        ),
    },
    Case {
        name: "interpolation-and-names",
        rng_seed: 0x5EED_0005,
        source: concat!(
            "global SPEED: int = 10\n",
            "const LIMIT: int = 20\n",
            "enum Color { Red, Green }\n",
            "struct Mob { health: int, attack_co: int? }\n",
            "fn hit(m: Mob, n: int) -> int {\n",
            "  let s = \"hit {n} \u{2014} {m.health}\"\n",
            "  return n + SPEED\n",
            "}\n",
        ),
    },
    Case {
        name: "generics-macros-decorators",
        rng_seed: 0x5EED_0006,
        source: concat!(
            "macro twice(x: int) -> int {\n  x * 2\n}\n",
            "deco init capture {\n}\n",
            "@capture\n",
            "fn id<T>(a: T) -> T {\n  return a\n}\n",
            "fn use_it() -> int {\n  return twice(3)\n}\n",
        ),
    },
];

/// A tiny deterministic LCG (the legacy adversarial test's generator) so the
/// harness needs no dependency and a printed seed fully determines the run.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    fn range(&mut self, max: usize) -> usize {
        if max == 0 {
            0
        } else {
            (self.next() as usize) % max
        }
    }

    fn choose<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(items.len())]
    }
}

/// Snippets chosen to land exactly where ranges are fragile: multibyte scalars,
/// CR/CRLF endings, comment openers and terminators, and partial syntax.
const INSERTIONS: &[&str] = &[
    "\u{e9}",
    "\u{4e2d}",
    "\u{2014}",
    "\r",
    "\r\n",
    "\n",
    "// c\n",
    "/* c */",
    "\"",
    "\"open",
    "{",
    "}",
    "(",
    ")",
    "[",
    "]",
    ":",
    "::",
    ".",
    "@",
    "?",
    "=",
    "fn f(",
    "let x = ",
    "{name}",
    "} else {",
];

/// One recorded edit: enough to replay the sequence by hand.
#[derive(Clone, Debug)]
struct Edit {
    op: &'static str,
    at: usize,
    removed: String,
    inserted: String,
}

/// Escape a snippet so a printed edit script survives one line of log.
fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Render the edit sequence compactly enough to paste into a repro command.
fn replay(edits: &[Edit]) -> String {
    let mut out = String::new();
    for (i, e) in edits.iter().enumerate() {
        let _ = write!(out, "\n  {i}: {}@{}", e.op, e.at);
        if !e.removed.is_empty() {
            let _ = write!(out, "-{:?}", escape(&e.removed));
        }
        if !e.inserted.is_empty() {
            let _ = write!(out, "+{:?}", escape(&e.inserted));
        }
    }
    out
}

/// Apply one bounded random edit; the returned `Edit` records enough to replay
/// it. Edits always land on char boundaries, so the source stays valid UTF-8.
fn mutate(src: &str, rng: &mut Rng) -> (String, Edit) {
    let mut chars: Vec<char> = src.chars().collect();
    let len = chars.len();
    match rng.range(3) {
        0 => {
            let at = rng.range(len + 1);
            let snippet = *rng.choose(INSERTIONS);
            let tail = chars.split_off(at);
            chars.extend(snippet.chars());
            chars.extend(tail);
            let edit = Edit {
                op: "insert",
                at,
                removed: String::new(),
                inserted: snippet.to_string(),
            };
            (chars.into_iter().collect(), edit)
        }
        1 => {
            if len == 0 {
                return (
                    String::new(),
                    Edit {
                        op: "noop",
                        at: 0,
                        removed: String::new(),
                        inserted: String::new(),
                    },
                );
            }
            let at = rng.range(len);
            let n = 1 + rng.range(3.min(len - at));
            let removed: String = chars.splice(at..at + n, std::iter::empty()).collect();
            let edit = Edit {
                op: "delete",
                at,
                removed,
                inserted: String::new(),
            };
            (chars.into_iter().collect(), edit)
        }
        _ => {
            if len == 0 {
                return (
                    String::new(),
                    Edit {
                        op: "noop",
                        at: 0,
                        removed: String::new(),
                        inserted: String::new(),
                    },
                );
            }
            let at = rng.range(len);
            let n = 1 + rng.range(3.min(len - at));
            let snippet = *rng.choose(INSERTIONS);
            let removed: String = chars.splice(at..at + n, snippet.chars()).collect();
            let edit = Edit {
                op: "replace",
                at,
                removed,
                inserted: snippet.to_string(),
            };
            (chars.into_iter().collect(), edit)
        }
    }
}

/// Independent line oracle: line bounds and per-line UTF-16 lengths read
/// straight off `src`, with no production position helper in the loop. Token
/// columns are char counts (docs/span-position-units.md); the corpus is BMP-only
/// (asserted below), where char and UTF-16 counts coincide, so one oracle pins
/// both readings.
struct LineOracle<'a> {
    src: &'a str,
    /// Byte offset of each line's first char.
    starts: Vec<usize>,
    /// Byte offset just past each line's content (a trailing '\n' excluded).
    ends: Vec<usize>,
    /// UTF-16 length of each line's content.
    len16: Vec<usize>,
}

impl<'a> LineOracle<'a> {
    fn new(src: &'a str) -> Self {
        assert!(
            src.chars().all(|c| c.len_utf16() == 1),
            "corpus must stay BMP-only; char and UTF-16 columns coincide here"
        );
        let mut starts = Vec::new();
        let mut ends = Vec::new();
        let mut len16 = Vec::new();
        let mut start = 0usize;
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                starts.push(start);
                ends.push(i);
                len16.push(src[start..i].encode_utf16().count());
                start = i + 1;
            }
        }
        starts.push(start);
        ends.push(src.len());
        len16.push(src[start..].encode_utf16().count());
        LineOracle {
            src,
            starts,
            ends,
            len16,
        }
    }

    fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Byte offset of char column `col` on `line`, clamped to the line end.
    fn byte_at_col(&self, line: usize, col: usize) -> usize {
        let (s, e) = (self.starts[line], self.ends[line]);
        let mut seen = 0usize;
        for (bi, ch) in self.src[s..e].char_indices() {
            if seen == col {
                return s + bi;
            }
            seen += ch.len_utf16();
        }
        e
    }

    /// Byte span of a token at `(line, col)` measuring `len` columns, or `None`
    /// when it does not fit its line.
    fn span(&self, line: u32, col: u32, len: u32) -> Option<(usize, usize)> {
        let l = line as usize;
        if l >= self.starts.len() {
            return None;
        }
        let end_col = col.checked_add(len)?;
        if end_col as usize > self.len16[l] {
            return None;
        }
        Some((
            self.byte_at_col(l, col as usize),
            self.byte_at_col(l, end_col as usize),
        ))
    }
}

/// Check one workspace's full token stream (and its whole-file range form)
/// against the oracle and `LineIndex`; panics with `repro` on the first
/// violation and answers how many tokens were checked.
fn check_stream(bone: &BoneWorkspace, src: &str, repro: &str) -> usize {
    let oracle = LineOracle::new(src);
    let index = LineIndex::new(src);
    let legend = semantic_tokens_legend();

    // `LineIndex` must agree with the O(offset) conversion on every offset of
    // this source, including mid-char and past-EOF offsets.
    for offset in 0..=src.len() {
        let slow = offset_to_position(src, offset);
        let fast = index.position(src, offset);
        assert_eq!(slow, fast, "{repro}: LineIndex disagreed at byte {offset}");
    }

    let data = bone.semantic_tokens(PATH).data;
    let tokens: Vec<DecodedToken> = decode_workspace(bone, PATH);
    assert_eq!(tokens.len(), data.len(), "{repro}: decoder dropped tokens");

    // The whole-file range request must reproduce the full stream exactly.
    let whole = Range::new(
        Position::new(0, 0),
        Position::new(oracle.line_count() as u32, 0),
    );
    let ranged = decode_tokens(bone.semantic_tokens_range(PATH, whole).data);
    assert_eq!(ranged, tokens, "{repro}: whole-file range diverged from full");

    let mut previous: Option<(u32, u32)> = None; // (line, end column)
    for (i, t) in tokens.iter().enumerate() {
        assert!(
            (t.token_type as usize) < legend.len(),
            "{repro}: token #{i} type {} outside the legend",
            t.token_type
        );
        assert!(t.length > 0, "{repro}: token #{i} has zero length");
        if let Some((pline, pend)) = previous {
            assert!(
                (t.line, t.col) >= (pline, pend),
                "{repro}: token #{i} at line {} col {} overlaps or precedes the previous token ending at line {pline} col {pend}",
                t.line,
                t.col
            );
        }
        let (bs, be) = oracle.span(t.line, t.col, t.length).unwrap_or_else(|| {
            panic!(
                "{repro}: token #{i} at line {} col {} len {} is out of bounds (line has {} columns)",
                t.line,
                t.col,
                t.length,
                oracle.len16.get(t.line as usize).copied().unwrap_or(0)
            )
        });
        let slice = &src[bs..be];
        assert_eq!(
            slice.chars().count(),
            t.length as usize,
            "{repro}: token #{i} char length disagrees with the source slice {slice:?}"
        );
        assert_eq!(
            slice.encode_utf16().count(),
            t.length as usize,
            "{repro}: token #{i} is not {} UTF-16 units in the source slice {slice:?}",
            t.length
        );
        assert!(
            !slice.contains('\n'),
            "{repro}: token #{i} slice crosses a line break: {slice:?}"
        );
        // Position conversions must land on the same byte span. Both token
        // endpoints are inside the line (length > 0, span fits), so the
        // char-column -> byte mapping is unambiguous.
        assert_eq!(
            offset_at_line_col(src, t.line, t.col),
            bs,
            "{repro}: offset_at_line_col(start) disagrees for token #{i}"
        );
        assert_eq!(
            offset_at_line_col(src, t.line, t.col + t.length),
            be,
            "{repro}: offset_at_line_col(end) disagrees for token #{i}"
        );
        assert_eq!(
            index.range(src, bs, be),
            Range::new(
                Position::new(t.line, t.col),
                Position::new(t.line, t.col + t.length)
            ),
            "{repro}: LineIndex range disagrees for token #{i}"
        );
        previous = Some((t.line, t.col + t.length));
    }

    tokens.len()
}

/// A bounded, seeded mutation run: every committed corpus case is edited up to
/// [`MUTATIONS`] times and the full invariant set is checked after every edit.
/// A crash or an invalid/disordered range fails with the case, seed and edit
/// script printed; the fixture is restored before the panic is re-raised.
#[test]
fn seeded_mutations_keep_token_ranges_oracle_valid() {
    let mut checked = 0usize;
    let mut producing_cases = 0usize;
    for case in CASES {
        eprintln!(
            "semantic_token_mutations: case={} rng_seed={:#018x}",
            case.name, case.rng_seed
        );
        let mut rng = Rng::new(case.rng_seed);
        let mut bone = BoneWorkspace::new();
        bone.update(PATH, case.source.to_string());
        let baseline = decode_workspace(&bone, PATH);

        // Every check runs against a workspace holding exactly `src`, under one
        // failure policy: print the case, seed and replayable edit sequence (via
        // `repro`), restore the fixture to the corpus seed, then re-raise.
        let check = |bone: &mut BoneWorkspace, src: &str, repro: &str| -> usize {
            bone.update(PATH, src.to_string());
            match catch_unwind(AssertUnwindSafe(|| check_stream(bone, src, repro))) {
                Ok(n) => n,
                Err(payload) => {
                    bone.update(PATH, case.source.to_string());
                    assert_eq!(
                        decode_workspace(bone, PATH),
                        baseline,
                        "case {}: fixture did not restore after a failed check",
                        case.name
                    );
                    eprintln!(
                        "semantic_token_mutations: case {} fixture restored to the corpus seed before re-raising",
                        case.name
                    );
                    resume_unwind(payload)
                }
            }
        };

        checked += check(&mut bone, case.source, &format!("case {} seed", case.name));

        let mut src = case.source.to_string();
        let mut edits: Vec<Edit> = Vec::new();
        for step in 0..MUTATIONS {
            let (next, edit) = mutate(&src, &mut rng);
            src = next;
            edits.push(edit);
            let repro = format!(
                "case {} rng_seed={:#018x} step {step}\nedits:{}\nsource: {:?}",
                case.name,
                case.rng_seed,
                replay(&edits),
                src
            );
            checked += check(&mut bone, &src, &repro);
        }

        if !baseline.is_empty() {
            producing_cases += 1;
        }

        // Restoration on success: rewriting the corpus source must reproduce
        // the baseline stream exactly, with no contaminated workspace state.
        checked += check(
            &mut bone,
            case.source,
            &format!("case {} restore", case.name),
        );
        assert_eq!(
            decode_workspace(&bone, PATH),
            baseline,
            "case {}: edits contaminated the workspace state",
            case.name
        );
    }
    // Non-vacuity guards: the corpus must actually tokenize, and the run must
    // have checked a meaningful number of ranges.
    eprintln!(
        "semantic_token_mutations: checked {checked} token ranges across {} cases ({MUTATIONS} edits each)",
        CASES.len()
    );
    assert!(
        producing_cases >= 3,
        "only {producing_cases} corpus cases produced tokens; the corpus or the tokenizer regressed"
    );
    assert!(
        checked >= 200,
        "mutation run checked only {checked} tokens; the invariant set is not being exercised"
    );
}

/// Insert `text` at char index `at`.
fn insert_chars(s: &str, at: usize, text: &str) -> String {
    let b = char_boundary(s, at);
    let mut out = String::with_capacity(s.len() + text.len());
    out.push_str(&s[..b]);
    out.push_str(text);
    out.push_str(&s[b..]);
    out
}

/// Delete `n` chars starting at char index `at`.
fn delete_chars(s: &str, at: usize, n: usize) -> String {
    let b = char_boundary(s, at);
    let e = char_boundary(s, at + n);
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..b]);
    out.push_str(&s[e..]);
    out
}

/// Replace `n` chars starting at char index `at` with `text`.
fn replace_chars(s: &str, at: usize, n: usize, text: &str) -> String {
    let b = char_boundary(s, at);
    let e = char_boundary(s, at + n);
    let mut out = String::with_capacity(s.len() + text.len());
    out.push_str(&s[..b]);
    out.push_str(text);
    out.push_str(&s[e..]);
    out
}

/// Byte offset of char index `at`, clamped to the end.
fn char_boundary(s: &str, at: usize) -> usize {
    s.char_indices().nth(at).map(|(b, _)| b).unwrap_or(s.len())
}

/// Deterministic single edits at the shapes the invariant is about, used by the
/// targeted test instead of the RNG: inserted/replaced/deleted scalars beside
/// multibyte text, split and joined CRLF endings, comment intruders, and
/// deliberately incomplete tails.
fn targeted_edits(seed: &str) -> Vec<(&'static str, String)> {
    let chars: Vec<char> = seed.chars().collect();
    let mut out = Vec::new();
    for (i, c) in chars.iter().enumerate() {
        if !c.is_ascii() {
            // Insert, replace and delete right at a multibyte scalar boundary.
            out.push(("insert-multibyte-before", insert_chars(seed, i, "\u{2014}")));
            out.push(("insert-multibyte-after", insert_chars(seed, i + 1, "\u{4e2d}")));
            out.push(("replace-multibyte", replace_chars(seed, i, 1, "\u{e9}")));
            out.push(("delete-multibyte", delete_chars(seed, i, 1)));
        }
        if *c == '\n' {
            out.push(("cr-before-newline", insert_chars(seed, i, "\r")));
            out.push(("delete-newline", delete_chars(seed, i, 1)));
            if i > 0 && chars[i - 1] == '\r' {
                out.push(("delete-cr-before-newline", delete_chars(seed, i - 1, 1)));
            }
        }
    }
    out.push(("prefix-block-comment", insert_chars(seed, 0, "/* ")));
    out.push(("suffix-block-comment", format!("{seed}*/")));
    out.push(("prefix-line-comment", insert_chars(seed, 0, "// ")));
    for tail in ["(", "{", "\"", "fn f(", "/*", "@", "}", ":"] {
        out.push(("incomplete-tail", format!("{seed}{tail}")));
    }
    out
}

/// The same invariant set over fixed, inspectable edits rather than the RNG,
/// so the multibyte/CRLF/comment/incomplete shapes stay pinned even if a seed's
/// random walk stops reaching them.
#[test]
fn targeted_edits_near_multibyte_crlf_and_incomplete_syntax() {
    let cases = [
        (
            "multibyte",
            "fn run(x: int) -> int {\n  let s = \"h\u{e9}llo \u{4e16}\u{754c}\"\n  return x\n}\n",
        ),
        (
            "crlf",
            "fn run(x: int) -> int {\r\n  let s = \"\u{e9}\"\r\n  return x\r\n}\r\n",
        ),
        (
            "comment",
            "fn run() {\n  // \u{2014} \u{e9} \u{4e2d} note\n  let x = 1\n}\n",
        ),
        ("incomplete", "fn f(\nstruct S {\nfn g() { let s = "),
    ];
    let mut checked = 0usize;
    for (name, seed) in cases {
        for (label, edited) in targeted_edits(seed) {
            let mut bone = BoneWorkspace::new();
            bone.update(PATH, edited.clone());
            let repro = format!("targeted case {name} edit {label}: {edited:?}");
            checked += check_stream(&bone, &edited, &repro);
        }
    }
    eprintln!("semantic_token_mutations: targeted run checked {checked} token ranges");
    assert!(
        checked >= 50,
        "targeted edit run checked only {checked} tokens; the invariant set is not being exercised"
    );
}
