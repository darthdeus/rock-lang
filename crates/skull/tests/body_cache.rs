//! The persistent per-body cache serves a chunk only while every input it
//! was built from is unchanged, and never changes the emitted text.

use crate::common::{module_id, ws_of_files};
use rustc_hash::FxHashMap;
use skull::mir_emit::{BodyCache, BodyCacheConfig, FullEmit, MirEmit};
use std::sync::Arc;
use ustr::Ustr;

const LIB: &str = r#"
fn helper(n: int) -> int {
    return n + 1
}

fn other(n: int) -> int {
    let total = 0
    for i in 0..n {
        total = total + i
    }
    return total
}
"#;

const MAIN: &str = r#"
fn main() {
    print("{helper(41)} {other(3)}")
}
"#;

fn config(dir: &std::path::Path, identity: &str, files: &[(&str, &str)]) -> BodyCacheConfig {
    let sources: FxHashMap<Ustr, Arc<str>> = files
        .iter()
        .map(|(file, src)| (Ustr::from(file), Arc::from(*src)))
        .collect();
    BodyCacheConfig {
        dir: dir.to_path_buf(),
        compiler_identity: identity.to_string(),
        project_key: "test".to_string(),
        sources,
    }
}

/// Emit `files` through a cache in `dir`, returning the emission and what
/// the cache did.
fn emit_cached(
    dir: &std::path::Path,
    identity: &str,
    files: &[(&str, &str)],
) -> (MirEmit, skull::mir_emit::BodyCacheStats) {
    let ws = ws_of_files(files);
    let cache = BodyCache::open(config(dir, identity, files));
    let emit = FullEmit::one_shot()
        .body_cache(Some(&cache))
        .run(&ws, module_id(&ws, "<main>"))
        .expect("emit")
        .0;
    (emit, cache.stats())
}

fn emit_uncached(files: &[(&str, &str)]) -> MirEmit {
    let ws = ws_of_files(files);
    FullEmit::one_shot()
        .run(&ws, module_id(&ws, "<main>"))
        .expect("emit")
        .0
}

#[test]
fn a_second_compile_is_served_from_the_cache_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let files = [("<lib>", LIB), ("<main>", MAIN)];
    let fresh = emit_uncached(&files);
    let (first, cold) = emit_cached(dir.path(), "compiler-a", &files);
    assert!(cold.enabled, "{cold:?}");
    assert_eq!(cold.hits, 0, "{cold:?}");
    assert!(cold.misses > 0 && cold.stored == cold.misses, "{cold:?}");
    assert_eq!(first.lua, fresh.lua, "a cold cache changes nothing");
    let (second, warm) = emit_cached(dir.path(), "compiler-a", &files);
    assert_eq!(warm.misses, 0, "{warm:?}");
    assert_eq!(warm.hits, cold.misses, "{warm:?}");
    assert_eq!(second.lua, fresh.lua, "a warm cache changes nothing");
    assert_eq!(second.artifact_fingerprint, fresh.artifact_fingerprint);
    assert_eq!(second.lua.len(), fresh.lua.len());
}

#[test]
fn editing_an_inlined_callee_rebuilds_its_callers() {
    let dir = tempfile::tempdir().unwrap();
    let files = [("<lib>", LIB), ("<main>", MAIN)];
    let (before, _) = emit_cached(dir.path(), "compiler-a", &files);
    // `helper` is spliced into `main`, so its text is in main's chunk.
    let main_body = crate::common::function_body(&before.lua, "main");
    assert!(
        !main_body.contains("helper("),
        "the fixture must inline helper into main:\n{main_body}"
    );
    let edited_lib = LIB.replace("return n + 1", "return n + 2");
    let edited = [("<lib>", edited_lib.as_str()), ("<main>", MAIN)];
    let fresh = emit_uncached(&edited);
    let (after, stats) = emit_cached(dir.path(), "compiler-a", &edited);
    assert_eq!(after.lua, fresh.lua, "the cache served a stale caller");
    // The edited callee and the caller that embeds it miss; `other` and the
    // stdlib bodies still hit.
    assert_eq!(stats.misses, 2, "{stats:?}");
    assert!(stats.hits > 0, "{stats:?}");
    assert_ne!(before.lua, after.lua);
}

#[test]
fn another_compiler_identity_misses_everything() {
    let dir = tempfile::tempdir().unwrap();
    let files = [("<lib>", LIB), ("<main>", MAIN)];
    let (_, cold) = emit_cached(dir.path(), "compiler-a", &files);
    let (_, other) = emit_cached(dir.path(), "compiler-b", &files);
    assert_eq!(other.hits, 0, "{other:?}");
    assert_eq!(other.misses, cold.misses, "{other:?}");
    let (_, back) = emit_cached(dir.path(), "compiler-a", &files);
    assert_eq!(back.misses, 0, "{back:?}");
}

#[test]
fn a_module_without_source_text_disables_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let files = [("<lib>", LIB), ("<main>", MAIN)];
    let ws = ws_of_files(&files);
    let cache = BodyCache::open(config(dir.path(), "compiler-a", &files[1..]));
    let emit = FullEmit::one_shot()
        .body_cache(Some(&cache))
        .run(&ws, module_id(&ws, "<main>"))
        .expect("emit")
        .0;
    let stats = cache.stats();
    assert!(!stats.enabled, "{stats:?}");
    assert_eq!(emit.lua, emit_uncached(&files).lua);
}
