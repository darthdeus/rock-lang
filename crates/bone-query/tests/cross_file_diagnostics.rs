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
    diags.iter().any(|d| {
        d.severity == Some(lsp_types::DiagnosticSeverity::ERROR) && d.message.contains("Foo")
    })
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
        per.into_iter()
            .find(|(p, _)| p == file)
            .map(|(_, d)| d)
            .unwrap_or_default()
    };
    let has_hole = |d: &[lsp_types::Diagnostic]| d.iter().any(|x| x.message.contains("hole"));

    let mut bone = BoneWorkspace::new();
    bone.load_project(&a);

    // Initial edit of a: its hole is reported.
    let a_src = std::fs::read_to_string(&a).unwrap();
    let d0 = bone.update(&a, a_src);
    assert!(
        has_hole(&diags_for(d0, &a)),
        "a.rock's `?` must report a hole"
    );

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
    std::fs::write(
        dir.path().join("a.rock"),
        "fn use_it() {\n  let x: ? = give()\n}\n",
    )
    .unwrap();
    // Canonicalize as the server/CLI do before calling `update` -- the session
    // keys modules by `canonicalize_lenient`, so the changed-set match needs the
    // same (its Windows form drops the `\\?\` prefix `std::fs::canonicalize` adds).
    let canon = |n: &str| bone_query::canonicalize_lenient(&dir.path().join(n).to_string_lossy());
    let a = canon("a.rock");
    let b = canon("b.rock");

    let diags_for = |per: Vec<(String, Vec<lsp_types::Diagnostic>)>, file: &str| {
        per.into_iter()
            .find(|(p, _)| p == file)
            .map(|(_, d)| d)
            .unwrap_or_default()
    };
    let hole_msg = |d: &[lsp_types::Diagnostic]| {
        d.iter()
            .find(|x| x.message.contains("hole"))
            .map(|x| x.message.clone())
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

/// The `./` hint is a two-label error (the `/` cause + the use site). The live
/// publish path (`update`) must surface BOTH as diagnostics so nvim paints a
/// squiggle on each line -- not just the primary. Regression for the LSP
/// dropping every label but the first.
#[test]
fn multi_label_hint_publishes_a_diagnostic_per_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"xp\"\n").unwrap();
    // `frame_h` is float (via `/`); using it in an int hole one line later is
    // the mismatch. The hint points at the `/` (line 1) and the use (line 2).
    let src = "fn main() {\n  let h = 8 / 3\n  let x: int = 2 * h\n}\n";
    std::fs::write(dir.path().join("a.rock"), src).unwrap();
    let a = bone_query::canonicalize_lenient(&dir.path().join("a.rock").to_string_lossy());

    let mut bone = BoneWorkspace::new();
    let per_file = bone.update(&a, src.to_string());
    let diags: Vec<_> = per_file
        .into_iter()
        .find(|(p, _)| p == &a)
        .map(|(_, d)| d)
        .unwrap_or_default();

    let hint_lines: Vec<u32> = diags
        .iter()
        .filter(|d| d.message.contains("./") || d.message.contains("used where `int`"))
        .map(|d| d.range.start.line)
        .collect();
    assert!(
        hint_lines.contains(&1) && hint_lines.contains(&2),
        "expected a squiggle on both the `/` line (1) and the use line (2), got: {:?}",
        diags
            .iter()
            .map(|d| (d.range.start.line, d.message.as_str()))
            .collect::<Vec<_>>()
    );
}

/// An interface-level error that only the whole-project (batch) pass can see --
/// two files declaring `struct Foo` -> "duplicate struct `Foo` across modules" --
/// must reach the editor. Regression: `batch_load_for_file` logged the batch
/// error and fell back to per-file loading, silently DROPPING it, so the LSP
/// showed nothing while `rock compile` rejected the same program.
#[test]
fn duplicate_struct_across_modules_surfaces_in_lsp() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"xp\"\n").unwrap();
    std::fs::write(
        dir.path().join("a.rock"),
        "struct Foo { x: int }\nfn main() -> int { 0 }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.rock"),
        "struct Foo { y: int }\nfn helper() -> int { 1 }\n",
    )
    .unwrap();
    let a = bone_query::canonicalize_lenient(&dir.path().join("a.rock").to_string_lossy());

    let has_dup = |per: &[(String, Vec<lsp_types::Diagnostic>)]| {
        per.iter()
            .flat_map(|(_, d)| d)
            .any(|d| d.message.contains("duplicate struct `Foo`"))
    };

    let b = bone_query::canonicalize_lenient(&dir.path().join("b.rock").to_string_lossy());
    let mut bone = BoneWorkspace::new();
    let d0 = bone.update(&a, std::fs::read_to_string(&a).unwrap());
    assert!(
        has_dup(&d0),
        "duplicate struct across modules must surface: {d0:?}"
    );
    // `diagnostics_for` (the one-shot CLI path) must agree with `update`. The
    // "duplicate across modules" error is symmetric; which file carries it depends
    // on module order, and with deterministic sorted discovery it attaches to the
    // second-declared struct (b.rock) -- so require it on either file rather than
    // pin one to the (now-changed) discovery order.
    let a_dup = bone
        .diagnostics_for(&a)
        .iter()
        .any(|d| d.message.contains("duplicate struct `Foo`"));
    let b_dup = bone
        .diagnostics_for(&b)
        .iter()
        .any(|d| d.message.contains("duplicate struct `Foo`"));
    assert!(
        a_dup || b_dup,
        "diagnostics_for must also surface the duplicate: a={:?} b={:?}",
        bone.diagnostics_for(&a),
        bone.diagnostics_for(&b)
    );

    // Fix b.rock (rename its Foo): the duplicate must CLEAR, not stick stale.
    let d1 = bone.update(
        &b,
        "struct Bar { y: int }\nfn helper() -> int { 1 }\n".to_string(),
    );
    assert!(
        !has_dup(&d1),
        "renaming b.rock's struct must clear the duplicate: {d1:?}"
    );
    assert!(
        !bone
            .diagnostics_for(&a)
            .iter()
            .any(|d| d.message.contains("duplicate struct")),
        "duplicate must be gone from diagnostics_for after the fix"
    );
}

fn source_load_failure_recovers(broken_manifest: bool) {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    let dep = dir.path().join("dep");
    std::fs::create_dir(&app).unwrap();
    std::fs::create_dir(&dep).unwrap();
    std::fs::write(
        app.join("stick.toml"),
        "[package]\nname = \"app\"\n[dependencies]\ndep = { path = \"../dep\" }\n",
    )
    .unwrap();
    let manifest = "[package]\nname = \"dep\"\ntype = \"lib\"\n";
    let dependency = "struct Dependency { value: int }\n";
    std::fs::write(dep.join("stick.toml"), manifest).unwrap();
    std::fs::write(dep.join("types.rock"), dependency).unwrap();
    let broken = if broken_manifest {
        dep.join("stick.toml")
    } else {
        dep.join("types.rock")
    };
    if broken_manifest {
        std::fs::write(&broken, "[package").unwrap();
    } else {
        // Invalid UTF-8 is unreadable by the source loader, even as root.
        std::fs::write(&broken, [0xff, 0xfe]).unwrap();
    }
    let src = "fn main() -> int { sibling() }\n";
    std::fs::write(app.join("main.rock"), src).unwrap();
    let sibling = "fn sibling() -> int { let d = Dependency { value: 1 }; d.value }\n";
    std::fs::write(app.join("sibling.rock"), sibling).unwrap();
    let main = bone_query::canonicalize_lenient(&app.join("main.rock").to_string_lossy());
    let sibling_path =
        bone_query::canonicalize_lenient(&app.join("sibling.rock").to_string_lossy());
    let dep_path = bone_query::canonicalize_lenient(&dep.join("types.rock").to_string_lossy());
    let reports_cause = |diags: &[lsp_types::Diagnostic]| {
        diags.iter().any(|d| {
            d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)
                && d.source.as_deref() == Some("bone")
                && d.message
                    .contains(broken.file_name().unwrap().to_str().unwrap())
        })
    };

    let mut bone = BoneWorkspace::new();
    bone.load_project(&main);
    let diags = bone.diagnostics_for(&main);
    assert!(
        reports_cause(&diags),
        "CLI diagnostics must identify the broken source/manifest: {diags:?}"
    );
    for _ in 0..2 {
        let per = bone.update(&main, src.to_string());
        let diags = &per
            .iter()
            .find(|(p, _)| p == &main)
            .expect("edited-file diagnostics")
            .1;
        assert!(
            reports_cause(diags),
            "live updates must retain the load failure: {diags:?}"
        );
    }

    std::fs::write(dep.join("stick.toml"), manifest).unwrap();
    std::fs::write(dep.join("types.rock"), dependency).unwrap();
    let per = bone.update(&main, src.to_string());
    for (path, diags) in &per {
        assert!(
            !diags
                .iter()
                .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
            "repair must clear errors and resolve unopened siblings/dependencies in {path}: {diags:?}"
        );
    }
    assert_eq!(
        bone.file_text(&sibling_path),
        Some(sibling),
        "unopened sibling indexed after repair"
    );
    assert!(
        bone.projects_summary()
            .iter()
            .flat_map(|(_, modules)| modules)
            .any(|m| m.id == dep_path),
        "dependency indexed in the consuming project after repair"
    );
    assert!(
        !reports_cause(&bone.diagnostics_for(&main)),
        "CLI load error cleared after repair"
    );
}

#[test]
fn malformed_dependency_manifest_reports_source_load_error_and_recovers() {
    source_load_failure_recovers(true);
}

#[test]
fn unreadable_dependency_source_reports_source_load_error_and_recovers() {
    source_load_failure_recovers(false);
}

#[test]
fn malformed_project_manifest_reports_source_discovery_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package").unwrap();
    let file = dir.path().join("main.rock");
    let src = "fn main() -> int { 1 }\n";
    std::fs::write(&file, src).unwrap();
    let file = file.to_str().unwrap();
    let mut bone = BoneWorkspace::new();
    let per = bone.update(file, src.to_string());
    assert!(
        per.iter().flat_map(|(_, ds)| ds).any(|d| {
            d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)
                && d.message.contains("stick.toml")
        }),
        "discovery must report the manifest cause: {per:?}"
    );
}

#[test]
fn standalone_buffer_without_disk_file_needs_no_project() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("scratch.rock");
    let src = "fn scratch() -> int { 1 }\n";
    let mut bone = BoneWorkspace::new();
    let per = bone.update(file.to_str().unwrap(), src.to_string());
    assert!(
        !per.iter()
            .flat_map(|(_, ds)| ds)
            .any(|d| { d.severity == Some(lsp_types::DiagnosticSeverity::ERROR) }),
        "standalone buffer must work without a project or disk source: {per:?}"
    );
    assert_eq!(bone.file_text(file.to_str().unwrap()), Some(src));
}

#[test]
fn missing_first_buffer_loads_project_and_keeps_unsaved_edits() {
    let (dir, b) = two_file_project();
    let missing = dir.path().join("new.rock").to_string_lossy().into_owned();
    let mut bone = BoneWorkspace::new();
    let per = bone.update(&missing, String::new());
    assert!(
        per.iter()
            .flat_map(|(_, d)| d)
            .all(|d| d.severity != Some(lsp_types::DiagnosticSeverity::ERROR)),
        "{per:?}"
    );
    let text = "fn draft() -> Foo { Foo { x: 1 } }\n";
    let per = bone.update(&missing, text.into());
    assert!(
        per.iter()
            .flat_map(|(_, d)| d)
            .all(|d| d.severity != Some(lsp_types::DiagnosticSeverity::ERROR)),
        "{per:?}"
    );
    bone.update(&b, std::fs::read_to_string(&b).unwrap());
    assert_eq!(bone.file_text(&missing), Some(text));
    assert!(!std::path::Path::new(&missing).exists());
}

#[test]
fn first_unsaved_source_loads_dependencies_of_empty_project() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    let dep = dir.path().join("dep");
    std::fs::create_dir(&app).unwrap();
    std::fs::create_dir(&dep).unwrap();
    std::fs::write(
        app.join("stick.toml"),
        "[package]\nname = \"app\"\n[dependencies]\ndep = \"../dep\"\n",
    )
    .unwrap();
    std::fs::write(dep.join("stick.toml"), "[package]\nname = \"dep\"\n").unwrap();
    std::fs::write(dep.join("types.rock"), "struct Foo { x: int }\n").unwrap();
    let path = app.join("new.rock").to_string_lossy().into_owned();
    let mut bone = BoneWorkspace::new();
    let per = bone.update(&path, "fn draft() -> Foo { Foo { x: 1 } }\n".into());
    assert!(
        per.iter()
            .flat_map(|(_, d)| d)
            .all(|d| d.severity != Some(lsp_types::DiagnosticSeverity::ERROR)),
        "{per:?}"
    );
    assert!(bone.hover(&path, lsp_types::Position::new(0, 14)).is_some());
}
