//! No hand-written JS may READ a return-bank slot. This is the property the
//! bank's safety actually rests on -- not the double-invocation shape.
//!
//! The bank is caller-saved-by-immediacy: `multi_assign` emits the call and
//! every `d = __rock_retN` capture as one indivisible run, so in EMITTED code
//! nothing can run between publish and capture. Hand-written JS is an opaque
//! string the emitter cannot schedule, so it is the only thing that could sit in
//! that window -- and it can only observe a clobber by READING a slot. A body
//! that merely calls user code twice (the three sort comparators) cannot corrupt
//! anything, because it never reads the bank: the second call overwrites slots
//! nobody looks at.
//!
//! So the audit target is READS, not double invocations. Publishing is
//! legitimate and expected -- the imgui stubs publish, through the one
//! compiler-owned `return __rock_publish(..)` whose arity comes from the
//! concrete result ABI. Writing a slot BY HAND is no longer legal at all
//! (`lower_publisher` rejects it), so the writer population this scan is
//! anchored against is the publishers.
//!
//! A hand-written READ would be the real hazard, and it would be silent: the
//! slot holds a stale leaf from whatever multi-value call ran last, which reads
//! back as a plausible number rather than throwing.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

/// Hand-written JS lives in `inline(js)`/`extern(js)` bodies in these `.rock`
/// sources, plus the runtime prelude string constants in `skull-codegen-js`.
/// Generated `game.js` build outputs are NOT hand-written and are excluded --
/// they are full of legitimate emitter-produced captures.
fn hand_written_sources() -> Vec<PathBuf> {
    let root = repo_root();
    let mut out = vec![
        root.join("crates/skull-codegen-js/src/lib.rs"),
        root.join("stdlib/js.rock"),
        root.join("stdlib/unified.rock"),
    ];
    for dir in ["stone/engine/web"] {
        let d = root.join(dir);
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "rock") {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Every `__rock_retN` occurrence that is not an assignment target. A write is
/// `__rock_retN` followed (modulo spaces) by a single `=` that is not `==`/`=>`.
fn bank_reads(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let needle = "__rock_ret";
    let mut from = 0;
    while let Some(rel) = text[from..].find(needle) {
        let at = from + rel;
        from = at + needle.len();
        let rest = &text[from..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue; // `__rock_ret` in prose, or the `ret_slot` helper name
        }
        let after = &rest[digits.len()..];
        let trimmed = after.trim_start();
        let is_write = trimmed.starts_with('=')
            && !trimmed.starts_with("==")
            && !trimmed.starts_with("=>");
        if is_write {
            continue;
        }
        let line = text[..at].matches('\n').count() + 1;
        let start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let end = text[at..].find('\n').map(|i| at + i).unwrap_or(text.len());
        out.push((line, text[start..end].trim().to_string()));
    }
    out
}

/// Lines that are pure prose (`//`, `///`, `//!`) mention the slots constantly.
fn is_comment(line: &str) -> bool {
    line.starts_with("//")
}

#[test]
fn no_hand_written_js_reads_a_return_bank_slot() {
    let mut offenders: Vec<String> = Vec::new();
    for path in hand_written_sources() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            panic!("cannot read {}", path.display());
        };
        for (line, src) in bank_reads(&text) {
            if is_comment(&src) {
                continue;
            }
            offenders.push(format!("{}:{}: {}", rel(&path), line, src));
        }
    }
    assert!(
        offenders.is_empty(),
        "hand-written JS reads a return-bank slot -- the read sees whatever \
         multi-value call ran last, silently:\n  {}",
        offenders.join("\n  ")
    );
}

/// The scan is only worth anything if it is actually looking at the bodies that
/// publish. Without this a bad path list would pass by finding nothing at all.
///
/// Anchored on the PUBLISHERS, not on hand-written slot writes: those are the
/// mechanism this contract replaced, and counting them would leave the read gate
/// vacuous the moment the last one was migrated -- which is exactly what
/// happened when they were.
#[test]
fn the_scan_sees_the_known_hand_written_publishers() {
    let mut publishers = 0usize;
    for path in hand_written_sources() {
        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            publishers += line.matches("__rock_publish(").count();
        }
    }
    // The imgui stubs are the publishing set: 13 multi-lane bodies.
    assert!(
        publishers >= 13,
        "expected the known multi-lane publishers to be in scan range, saw {publishers} \
         -- the path list is probably wrong, which would make the read gate vacuous"
    );
}

fn rel(p: &Path) -> String {
    p.strip_prefix(repo_root()).unwrap_or(p).display().to_string()
}
