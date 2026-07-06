//! Cross-file symbol resolution for the bone LSP.
//!
//! Opening a single file of a multi-file project must still resolve types and
//! functions defined in sibling files -- otherwise every cross-file reference
//! is a false "unknown" error. The editor opens one buffer at a time, so the
//! workspace has to index the whole project root, not just the touched file.

use bone_query::BoneWorkspace;

/// A two-file project: `a.rock` defines `Foo`, `b.rock` uses it. Returns the
/// tempdir (kept alive) and the absolute path to `b.rock`.
fn two_file_project() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"xp\"\n").unwrap();
    std::fs::write(dir.path().join("a.rock"), "struct Foo { x: int }\n").unwrap();
    std::fs::write(
        dir.path().join("b.rock"),
        "fn use_it() {\n  let f = Foo { x: 1 }\n}\n",
    )
    .unwrap();
    // Canonical path: `BoneWorkspace` keys modules (and the per-file diagnostics
    // it returns) by `canonicalize_lenient`, so a return-key match (`p == b`)
    // needs the SAME canonicalization -- not `std::fs::canonicalize`, whose
    // Windows `\\?\C:\...` form differs from the stripped key the workspace uses.
    let b = bone_query::canonicalize_lenient(&dir.path().join("b.rock").to_string_lossy());
    (dir, b)
}

fn mentions_unknown_foo(diags: &[lsp_types::Diagnostic]) -> bool {
    diags
        .iter()
        .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR) && d.message.contains("Foo"))
}

#[test]
fn load_project_resolves_cross_file_types() {
    // Sanity check the mechanism: with the whole project indexed, `Foo`
    // resolves and `b.rock` has no error.
    let (_dir, b) = two_file_project();
    let mut bone = BoneWorkspace::new();
    bone.load_project(&b);
    let diags = bone.diagnostics_for(&b);
    assert!(
        !mentions_unknown_foo(&diags),
        "Foo is defined in a.rock; with the project loaded it must resolve: {diags:?}"
    );
}

#[test]
fn opening_one_file_resolves_cross_file_symbols() {
    // Mirror the editor: a fresh workspace, then `update` ONLY b.rock (as
    // didOpen does). `Foo` lives in a.rock; it must still resolve, so no
    // "unknown" error -- the workspace indexes the project root on first touch.
    let (_dir, b) = two_file_project();
    let b_src = std::fs::read_to_string(&b).unwrap();
    let mut bone = BoneWorkspace::new();
    let per_file = bone.update(&b, b_src);
    let b_diags: Vec<_> = per_file
        .into_iter()
        .find(|(p, _)| p == &b)
        .map(|(_, d)| d)
        .unwrap_or_default();
    assert!(
        !mentions_unknown_foo(&b_diags),
        "opening b.rock alone must resolve Foo from a.rock: {b_diags:?}"
    );
}

/// `update` caches per-file hole diagnostics to avoid reparsing the whole
/// compilation unit every keystroke (only changed files are recomputed). This
/// pins the cache invariants: (1) an UNRELATED edit must not drop another file's
/// holes (they're served from cache), and (2) editing a file must REFRESH its
/// own holes (it's in the changed set). A cache that served stale data or never
/// invalidated the edited file would trip this.
#[test]
fn hole_diagnostics_survive_unrelated_edits_and_clear_on_fix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"xp\"\n").unwrap();
    // a.rock has an unfilled expression hole; b.rock is unrelated.
    std::fs::write(dir.path().join("a.rock"), "fn a() -> int { ? }\n").unwrap();
    std::fs::write(dir.path().join("b.rock"), "fn b() -> int { 1 }\n").unwrap();
    // Canonical paths: `update` returns per-file diagnostics keyed by
    // `canonicalize_lenient`, so the `p == file` match below needs the same
    // canonicalization (see `two_file_project`).
    let a = bone_query::canonicalize_lenient(&dir.path().join("a.rock").to_string_lossy());
    let b = bone_query::canonicalize_lenient(&dir.path().join("b.rock").to_string_lossy());

    let diags_for = |per: Vec<(String, Vec<lsp_types::Diagnostic>)>, file: &str| {
        per.into_iter().find(|(p, _)| p == file).map(|(_, d)| d).unwrap_or_default()
    };
    let has_hole = |d: &[lsp_types::Diagnostic]| d.iter().any(|x| x.message.contains("hole"));

    let mut bone = BoneWorkspace::new();
    bone.load_project(&a);

    // Initial edit of a: its hole is reported.
    let a_src = std::fs::read_to_string(&a).unwrap();
    let d0 = bone.update(&a, a_src);
    assert!(has_hole(&diags_for(d0, &a)), "a.rock's `?` must report a hole");

    // Unrelated edit of b: a.rock's hole must STILL be present (cache served).
    let d1 = bone.update(&b, "fn b() -> int { 2 }\n".to_string());
    assert!(
        has_hole(&diags_for(d1, &a)),
        "editing b.rock must not drop a.rock's cached hole diagnostic"
    );

    // Fix a (remove the hole): its hole diagnostic must clear (recomputed).
    let d2 = bone.update(&a, "fn a() -> int { 1 }\n".to_string());
    assert!(
        !has_hole(&diags_for(d2, &a)),
        "removing a.rock's `?` must clear its hole diagnostic (edited file is recomputed)"
    );
}

/// A reinferred DEPENDENT that HAS a hole must get its hole message refreshed,
/// not served stale. `a.rock` has a type hole whose inferred type comes from
/// `b.rock`'s `give()` return type; editing `give`'s signature reinfers `a`, so
/// its "inferred as <ty>" message must follow. This pins the hazard in skipping
/// hole reparse for hole-free dependents: a dependent WITH a hole is never skipped.
#[test]
fn dependent_hole_message_refreshes_after_interface_edit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"xp\"\n").unwrap();
    std::fs::write(dir.path().join("b.rock"), "fn give() -> int { 1 }\n").unwrap();
    std::fs::write(dir.path().join("a.rock"), "fn use_it() {\n  let x: ? = give()\n}\n").unwrap();
    // Canonicalize as the server/CLI do before calling `update` -- the session
    // keys modules by `canonicalize_lenient`, so the changed-set match needs the
    // same (its Windows form drops the `\\?\` prefix `std::fs::canonicalize` adds).
    let canon = |n: &str| bone_query::canonicalize_lenient(&dir.path().join(n).to_string_lossy());
    let a = canon("a.rock");
    let b = canon("b.rock");

    let diags_for = |per: Vec<(String, Vec<lsp_types::Diagnostic>)>, file: &str| {
        per.into_iter().find(|(p, _)| p == file).map(|(_, d)| d).unwrap_or_default()
    };
    let hole_msg = |d: &[lsp_types::Diagnostic]| {
        d.iter().find(|x| x.message.contains("hole")).map(|x| x.message.clone())
    };

    let mut bone = BoneWorkspace::new();
    bone.load_project(&a);

    // Open a: its type hole is inferred from give()'s int return.
    let d0 = bone.update(&a, std::fs::read_to_string(&a).unwrap());
    assert_eq!(
        hole_msg(&diags_for(d0, &a)).as_deref(),
        Some("type hole ? inferred as int"),
        "a.rock's type hole should infer give()'s int return"
    );

    // Edit b's signature: int -> float. a.rock is a reinferred dependent; its
    // hole message must follow to `float`, not stay the cached `int`.
    let d1 = bone.update(&b, "fn give() -> float { 1.0 }\n".to_string());
    assert_eq!(
        hole_msg(&diags_for(d1, &a)).as_deref(),
        Some("type hole ? inferred as float"),
        "editing give()'s return type must refresh a.rock's dependent hole message, not serve stale"
    );
}
