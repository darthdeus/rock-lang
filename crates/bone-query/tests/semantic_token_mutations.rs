//! Bounded edits through the real workspace, checked without production position helpers.
//! Columns deliberately count UTF-16 code units, not bytes or Unicode scalars.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use bone_query::{BoneWorkspace, semantic_tokens_legend, semantic_tokens_modifiers_legend};
use lsp_types::{Position, Range, SemanticToken};

const PATH: &str = "/tmp/rock_bone_semantic_mutations.rock";
const MUTATIONS: usize = 32;

struct Case {
    name: &'static str,
    seed: u64,
    source: &'static str,
    valid: bool,
}

const CASES: &[Case] = &[
    Case {
        name: "interpolation-field-access",
        seed: 0x5eed_0001,
        source: concat!(
            "global SPEED: int = 10\nconst LIMIT: int = 20\n",
            "enum Color { Red, Green }\n",
            "struct Mob { health: int, attack_co: int? }\n",
            "fn hit(m: Mob, n: int) -> int {\n",
            "  let s = \"hit {n} \u{2014} {m.health}\"\n",
            "  return m.health + SPEED\n}\n",
        ),
        valid: true,
    },
    Case {
        name: "multiline-strings-comments",
        seed: 0x5eed_0002,
        source: concat!(
            "// rock: allow(unused_var)\n",
            "fn run() {\n",
            "  let s = \"\"\"first \u{e9}\nsecond \u{4e2d}\nthird\"\"\"\n",
            "  /* block\n     comment */\n",
            "  let x = 1 // trailing \u{2014}\n}\n",
        ),
        valid: true,
    },
    Case {
        name: "utf16-not-scalars-or-bytes",
        seed: 0x5eed_0003,
        source: "fn run() { let s = \"\u{e9}\u{1f980}\"; let marker = 1 }\n",
        valid: true,
    },
    Case {
        name: "crlf-comments",
        seed: 0x5eed_0004,
        source: "fn run(x: int) -> int {\r\n  // \u{e9} \u{4e2d}\r\n  return x\r\n}\r\n",
        valid: true,
    },
    Case {
        name: "generics-macros-decorators",
        seed: 0x5eed_0006,
        source: concat!(
            "macro twice(x: int) -> int {\n  x * 2\n}\n",
            "deco init capture {\n}\n@capture\n",
            "fn id<T>(a: T) -> T {\n  return a\n}\n",
            "fn use_it() -> int {\n  return twice(3)\n}\n",
        ),
        valid: true,
    },
    Case {
        name: "malformed",
        seed: 0x5eed_0005,
        source: "fn f(\nstruct S {\n  field: \nfn g() { let s = \"unterminated\n",
        valid: false,
    },
];

struct Rng(u64);

impl Rng {
    fn pick(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 % bound as u64) as usize
    }
}

const INSERTIONS: &[&str] = &[
    "\u{e9}",
    "\u{4e2d}",
    "\u{1f980}",
    "\r",
    "\r\n",
    "\n",
    "// c\n",
    "/*",
    "*/",
    "\"",
    "\"\"\"",
    "{",
    "}",
    "(",
    ")",
    ".",
    "?",
    "fn f(",
    "let x = ",
    "{name}",
];

#[derive(Debug)]
struct Edit {
    at_scalar: usize,
    removed: String,
    inserted: String,
}

fn mutate(source: &str, rng: &mut Rng) -> (String, Edit) {
    let mut chars: Vec<char> = source.chars().collect();
    let op = rng.pick(3);
    let at = rng.pick(chars.len() + 1);
    let count = if op == 0 || at == chars.len() {
        0
    } else {
        1 + rng.pick(3.min(chars.len() - at))
    };
    let inserted = if op == 1 {
        ""
    } else {
        INSERTIONS[rng.pick(INSERTIONS.len())]
    };
    let removed = chars.splice(at..at + count, inserted.chars()).collect();
    (
        chars.into_iter().collect(),
        Edit {
            at_scalar: at,
            removed,
            inserted: inserted.into(),
        },
    )
}

#[derive(Debug, PartialEq, Eq)]
struct Decoded {
    line: u32,
    col: u32,
    length: u32,
    kind: u32,
    modifiers: u32,
}

// Wire decoding is test-owned, with checked arithmetic so corrupt deltas fail
// in release tests too. No tokenizer spans, LineIndex or query decoder is used.
fn decode(data: &[SemanticToken]) -> Result<Vec<Decoded>, String> {
    let (mut line, mut col) = (0u32, 0u32);
    let mut out = Vec::new();
    for (i, t) in data.iter().enumerate() {
        line = line
            .checked_add(t.delta_line)
            .ok_or_else(|| format!("token {i}: line overflow"))?;
        col = if t.delta_line == 0 {
            col.checked_add(t.delta_start)
                .ok_or_else(|| format!("token {i}: column overflow"))?
        } else {
            t.delta_start
        };
        out.push(Decoded {
            line,
            col,
            length: t.length,
            kind: t.token_type,
            modifiers: t.token_modifiers_bitset,
        });
    }
    Ok(out)
}

// Split only on LF: a CR in CRLF still occupies one column, matching the
// current contract. Slicing UTF-16 vectors cannot hide a byte/scalar drift.
fn validate(
    source: &str,
    data: &[SemanticToken],
    require_tokens: bool,
) -> Result<Vec<Decoded>, String> {
    if require_tokens && data.is_empty() {
        return Err("valid control returned no tokens".into());
    }
    let lines: Vec<Vec<u16>> = source
        .split('\n')
        .map(|s| s.encode_utf16().collect())
        .collect();
    let types = semantic_tokens_legend().len();
    let modifiers = semantic_tokens_modifiers_legend().len();
    let allowed_modifiers = if modifiers >= u32::BITS as usize {
        u32::MAX
    } else {
        (1u32 << modifiers) - 1
    };
    let tokens = decode(data)?;
    let mut previous_end = (0, 0);
    for (i, t) in tokens.iter().enumerate() {
        let fail = |why| format!("token {i} {t:?}: {why}");
        if t.kind as usize >= types {
            return Err(fail("type outside legend"));
        }
        if t.modifiers & !allowed_modifiers != 0 {
            return Err(fail("modifier outside legend"));
        }
        if t.length == 0 {
            return Err(fail("empty span"));
        }
        let end = t
            .col
            .checked_add(t.length)
            .ok_or_else(|| fail("end overflow"))?;
        let line = lines
            .get(t.line as usize)
            .ok_or_else(|| fail("line out of bounds"))?;
        let slice = line
            .get(t.col as usize..end as usize)
            .ok_or_else(|| fail("span out of bounds"))?;
        if slice.is_empty() || slice.contains(&(b'\n' as u16)) {
            return Err(fail("not a nonempty single-line span"));
        }
        if (t.line, t.col) < previous_end {
            return Err(fail("overlapping or unordered span"));
        }
        previous_end = (t.line, end);
    }
    Ok(tokens)
}

fn check_stream(bone: &BoneWorkspace, source: &str, require_tokens: bool, context: &str) -> usize {
    let full = bone.semantic_tokens(PATH).data;
    let decoded =
        validate(source, &full, require_tokens).unwrap_or_else(|e| panic!("{context}: {e}"));
    let line_count = source.split('\n').count() as u32;
    let whole = Range::new(Position::new(0, 0), Position::new(line_count, 0));
    let ranged = bone.semantic_tokens_range(PATH, whole).data;
    validate(source, &ranged, require_tokens)
        .unwrap_or_else(|e| panic!("{context}: whole range: {e}"));
    assert_eq!(
        ranged, full,
        "{context}: whole-file range diverged from full"
    );
    // Range tokens are independently delta-encoded, not a raw slice of full.
    let viewport = Range::new(Position::new(1, 0), Position::new(3, 0));
    let ranged = bone.semantic_tokens_range(PATH, viewport).data;
    let ranged =
        validate(source, &ranged, false).unwrap_or_else(|e| panic!("{context}: viewport: {e}"));
    let expected: Vec<_> = decoded
        .iter()
        .filter(|t| (1..3).contains(&t.line))
        .collect();
    assert_eq!(
        ranged.iter().collect::<Vec<_>>(),
        expected,
        "{context}: viewport diverged from full"
    );
    decoded.len()
}

// Workspace state is local to a case; nevertheless restore on unwind as well as
// success to exercise repair after every malformed buffer. Update panics get
// the same replay context as query/assertion panics.
fn with_repair<T>(
    bone: &mut BoneWorkspace,
    seed: &str,
    work: impl FnOnce(&mut BoneWorkspace) -> T,
) -> T {
    let result = catch_unwind(AssertUnwindSafe(|| work(bone)));
    bone.update(PATH, seed.into());
    match result {
        Ok(value) => value,
        Err(payload) => resume_unwind(payload),
    }
}

fn replay(case: &Case, label: &str, edits: &[Edit], source: &str) -> String {
    let mut out = format!("case={} seed={:#018x} {label}", case.name, case.seed);
    for (step, edit) in edits.iter().enumerate() {
        out.push_str(&format!(
            "\nedit={step} at_scalar={} remove={:?} insert={:?}",
            edit.at_scalar, edit.removed, edit.inserted
        ));
    }
    out.push_str(&format!("\nsource={source:?}"));
    out
}

fn update_and_check(
    bone: &mut BoneWorkspace,
    source: &str,
    required: bool,
    context: &str,
) -> usize {
    match catch_unwind(AssertUnwindSafe(|| {
        bone.update(PATH, source.into());
        check_stream(bone, source, required, context)
    })) {
        Ok(n) => n,
        Err(payload) => {
            eprintln!("semantic_token_mutations failure: {context}");
            resume_unwind(payload)
        }
    }
}

#[test]
fn validator_rejects_corrupt_streams_before_production_controls() {
    let good = SemanticToken {
        delta_line: 0,
        delta_start: 0,
        length: 1,
        token_type: 0,
        token_modifiers_bitset: 0,
    };
    assert!(validate("x\n", &[good], true).is_ok());
    let corruptions = [
        (
            "shift-column",
            SemanticToken {
                delta_start: 1,
                ..good
            },
        ),
        (
            "shift-line",
            SemanticToken {
                delta_line: 2,
                ..good
            },
        ),
        ("cross-newline", SemanticToken { length: 2, ..good }),
        ("zero-length", SemanticToken { length: 0, ..good }),
        (
            "bad-type",
            SemanticToken {
                token_type: semantic_tokens_legend().len() as u32,
                ..good
            },
        ),
        (
            "bad-modifier",
            SemanticToken {
                token_modifiers_bitset: 1 << semantic_tokens_modifiers_legend().len(),
                ..good
            },
        ),
    ];
    for (name, bad) in corruptions {
        let error = validate("x\n", &[bad], true).expect_err(name);
        eprintln!("rejected {name}: {error}");
    }
    assert!(
        validate("x\n", &[], true).is_err(),
        "empty fallback passed a valid control"
    );
    assert!(
        validate("", &[], false).is_ok(),
        "malformed/empty buffers may emit nothing"
    );
    assert!(
        decode(&[
            SemanticToken {
                delta_start: u32::MAX,
                ..good
            },
            SemanticToken {
                delta_start: 1,
                ..good
            }
        ])
        .is_err()
    );
    assert!(
        decode(&[
            SemanticToken {
                delta_line: u32::MAX,
                ..good
            },
            SemanticToken {
                delta_line: 1,
                ..good
            }
        ])
        .is_err()
    );
    assert!(
        validate("xx\n", &[good, good], true).is_err(),
        "overlap accepted"
    );

    let mut bone = BoneWorkspace::new();
    for case in CASES.iter().filter(|c| c.valid) {
        update_and_check(
            &mut bone,
            case.source,
            true,
            &replay(case, "valid control", &[], case.source),
        );
    }
}

#[test]
fn seeded_mutations_and_repairs_keep_token_spans_valid() {
    let mut checked = 0;
    for case in CASES {
        let mut bone = BoneWorkspace::new();
        checked += update_and_check(
            &mut bone,
            case.source,
            case.valid,
            &replay(case, "seed", &[], case.source),
        );
        let baseline = bone.semantic_tokens(PATH).data;
        let mut source = case.source.to_string();
        let mut rng = Rng(case.seed);
        let mut edits = Vec::new();
        with_repair(&mut bone, case.source, |bone| {
            for step in 0..MUTATIONS {
                let (next, edit) = mutate(&source, &mut rng);
                source = next;
                edits.push(edit);
                checked += update_and_check(
                    bone,
                    &source,
                    false,
                    &replay(case, &format!("step={step}"), &edits, &source),
                );
            }
        });
        let context = replay(case, "repair", &edits, case.source);
        checked += check_stream(&bone, case.source, case.valid, &context);
        assert_eq!(
            bone.semantic_tokens(PATH).data,
            baseline,
            "{context}: repair changed baseline"
        );
        eprintln!(
            "case={} seed={:#x}: {MUTATIONS} edits and repair passed",
            case.name, case.seed
        );
    }
    eprintln!("checked {checked} full-stream token spans");
}

#[test]
fn targeted_boundary_edits_keep_spans_valid_and_repair() {
    for case in CASES {
        let mut bone = BoneWorkspace::new();
        update_and_check(
            &mut bone,
            case.source,
            case.valid,
            &replay(case, "seed", &[], case.source),
        );
        let baseline = bone.semantic_tokens(PATH).data;
        let chars: Vec<char> = case.source.chars().collect();
        let mut edits_checked = 0;
        for (at, ch) in chars.iter().enumerate() {
            if ch.is_ascii() && !matches!(ch, '\n' | '\r' | '.' | '{' | '}' | '"') {
                continue;
            }
            for (remove, insert) in [
                (0, "\u{2014}"),
                (0, "\r"),
                (0, "/*"),
                (1, ""),
                (1, "\u{4e2d}"),
            ] {
                let mut edited = chars.clone();
                let removed: String = edited.splice(at..at + remove, insert.chars()).collect();
                let source: String = edited.into_iter().collect();
                let edit = Edit {
                    at_scalar: at,
                    removed,
                    inserted: insert.into(),
                };
                let context = replay(case, "boundary", &[edit], &source);
                with_repair(&mut bone, case.source, |bone| {
                    update_and_check(bone, &source, false, &context);
                });
                check_stream(&bone, case.source, case.valid, &context);
                assert_eq!(
                    bone.semantic_tokens(PATH).data,
                    baseline,
                    "{context}: boundary repair"
                );
                edits_checked += 1;
            }
        }
        eprintln!(
            "case={} seed={:#x}: {edits_checked} boundary edits and repairs passed",
            case.name, case.seed
        );
    }
}

#[test]
fn targeted_malformed_buffers_repair_and_failure_cleanup() {
    let case = &CASES[0];
    let mut bone = BoneWorkspace::new();
    update_and_check(
        &mut bone,
        case.source,
        true,
        &replay(case, "seed", &[], case.source),
    );
    let baseline = bone.semantic_tokens(PATH).data;
    for malformed in [
        "",
        " \n\t",
        "fn f(",
        "/* unclosed",
        "\"unclosed",
        "fn f() { let s = \"{x.\"",
        "\"\"\"first\nsecond",
    ] {
        let edit = Edit {
            at_scalar: 0,
            removed: case.source.into(),
            inserted: malformed.into(),
        };
        let context = replay(case, "targeted", &[edit], malformed);
        with_repair(&mut bone, case.source, |bone| {
            update_and_check(bone, malformed, false, &context);
        });
        check_stream(&bone, case.source, true, &context);
        assert_eq!(
            bone.semantic_tokens(PATH).data,
            baseline,
            "{context}: targeted repair"
        );
    }
    let intentional = catch_unwind(AssertUnwindSafe(|| {
        with_repair(&mut bone, case.source, |bone| {
            bone.update(PATH, "fn broken(".into());
            panic!("intentional failure to prove repair on unwind");
        });
    }));
    assert!(intentional.is_err());
    assert_eq!(
        bone.semantic_tokens(PATH).data,
        baseline,
        "failure cleanup did not restore the source"
    );
    check_stream(
        &bone,
        case.source,
        true,
        &replay(case, "unwind repair", &[], case.source),
    );
}

#[test]
fn non_bmp_control_pins_utf16_columns_after_multibyte_text() {
    let case = &CASES[2];
    let mut bone = BoneWorkspace::new();
    update_and_check(
        &mut bone,
        case.source,
        true,
        &replay(case, "utf-16 control", &[], case.source),
    );
    // Locate an identifier independently, after both a multibyte BMP scalar
    // and a supplementary scalar. Byte and scalar columns both differ here.
    let offset = case.source.find("marker").unwrap();
    let prefix = &case.source[..offset];
    let line = prefix.bytes().filter(|b| *b == b'\n').count() as u32;
    let col = prefix.rsplit('\n').next().unwrap().encode_utf16().count() as u32;
    let decoded = decode(&bone.semantic_tokens(PATH).data).unwrap();
    assert!(
        decoded
            .iter()
            .any(|t| t.line == line && t.col == col && t.length == "marker".chars().count() as u32),
        "{}: missing UTF-16-aligned marker at {line}:{col}; {decoded:?}",
        replay(case, "utf-16 control", &[], case.source)
    );
}
