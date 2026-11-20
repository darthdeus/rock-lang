//! Seeded identifier swaps through the editor queries: no query may panic on a
//! buffer that no longer resolves.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use bone_query::BoneWorkspace;
use lsp_types::{Position, Range};

#[cfg(not(windows))]
const PATH: &str = "/tmp/rock_bone_query_mutations.rock";
#[cfg(windows)]
const PATH: &str = r"C:\tmp\rock_bone_query_mutations.rock";

const SWAPS: usize = 24;

const SOURCES: &[&str] = &[
    include_str!("../../../tests/structs/field_ref.rock"),
    include_str!("../../../tests/patterns/let_destructuring.rock"),
    include_str!("../../../tests/closures/inlined_calls.rock"),
    "value Vary { id: float }\n\
     fn frag(vary: Vary, t: Texture) -> int {\n  return vary.id as int\n}\n",
];

fn identifiers(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut spans = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if word(bytes[at]) {
            let start = at;
            while at < bytes.len() && word(bytes[at]) {
                at += 1;
            }
            if !bytes[start].is_ascii_digit() {
                spans.push((start, at));
            }
        } else {
            at += 1;
        }
    }
    spans
}

fn position(source: &str, offset: usize) -> Position {
    let before = &source[..offset];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Position::new(
        before.matches('\n').count() as u32,
        before[line_start..].encode_utf16().count() as u32,
    )
}

#[test]
fn identifier_swaps_never_panic_a_query() {
    let mut random = 0x5eed_0441_u64;
    let mut pick = |bound: usize| {
        random = random
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((random >> 33) % bound as u64) as usize
    };
    for (index, source) in SOURCES.iter().enumerate() {
        let spans = identifiers(source);
        let mut bone = BoneWorkspace::new();
        for step in 0..SWAPS {
            let (start, end) = spans[pick(spans.len())];
            let (from, to) = spans[pick(spans.len())];
            let replacement = &source[from..to];
            let mutated = format!("{}{replacement}{}", &source[..start], &source[end..]);
            let at = position(&mutated, start);
            let after = position(&mutated, start + replacement.len());
            let lines = mutated.lines().count() as u32 + 1;
            let result = catch_unwind(AssertUnwindSafe(|| {
                bone.update(PATH, mutated.clone());
                bone.completion(PATH, after);
                bone.hover(PATH, at);
                bone.definition(PATH, at);
                bone.references(PATH, at);
                bone.signature_help(PATH, after);
                bone.semantic_tokens(PATH);
                bone.inlay_hints(
                    PATH,
                    Range::new(Position::new(0, 0), Position::new(lines, 0)),
                );
            }));
            if let Err(payload) = result {
                eprintln!(
                    "source {index} step {step}: `{}` -> `{replacement}` at {at:?}\n{mutated}",
                    &source[start..end]
                );
                resume_unwind(payload);
            }
        }
    }
}
