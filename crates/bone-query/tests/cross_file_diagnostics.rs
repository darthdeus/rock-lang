//! Cross-file diagnostics. The editor opens one buffer at a time, so the
//! workspace indexes the whole project root and must keep every file's
//! diagnostics current as any of them changes.

use crate::fixture::{Project, diagnostics_in};
use bone_query::BoneWorkspace;
use lsp_types::{Diagnostic, DiagnosticSeverity};

const MANIFEST: (&str, &str) = ("stick.toml", "[package]\nname = \"xp\"\n");

/// `a.rock` defines `Foo`, `b.rock` uses it.
fn two_file_project() -> Project {
    Project::new(&[
        MANIFEST,
        ("a.rock", "struct Foo { x: int }\n"),
        ("b.rock", "fn use_it() {\n  let f = Foo { x: 1 }\n}\n"),
    ])
}

fn errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
    diags
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect()
}

fn all_errors(per_file: &[(String, Vec<Diagnostic>)]) -> Vec<&Diagnostic> {
    per_file.iter().flat_map(|(_, d)| errors(d)).collect()
}

#[test]
fn sibling_file_types_resolve_whether_loaded_or_opened() {
    let project = two_file_project();
    let b = project.path("b.rock");
    let mut loaded = BoneWorkspace::new();
    loaded.load_project(&b);
    let diags = loaded.diagnostics_for(&b);
    assert!(errors(&diags).is_empty(), "load_project: {diags:?}");
    // didOpen of b.rock alone, as the editor does.
    let (_, per_file) = project.open("b.rock");
    let diags = diagnostics_in(per_file, &b);
    assert!(errors(&diags).is_empty(), "update alone: {diags:?}");
}

/// Per-file hole diagnostics are cached across updates: an unrelated edit
/// must keep another file's holes, and editing a file must refresh its own.
#[test]
fn hole_diagnostics_survive_unrelated_edits_and_clear_on_fix() {
    let project = Project::new(&[
        MANIFEST,
        ("a.rock", "fn a() -> int { ? }\n"),
        ("b.rock", "fn b() -> int { 1 }\n"),
    ]);
    let (a, b) = (project.path("a.rock"), project.path("b.rock"));
    let has_hole = |per| {
        diagnostics_in(per, &a)
            .iter()
            .any(|x| x.message.contains("hole"))
    };
    let mut bone = BoneWorkspace::new();
    bone.load_project(&a);
    assert!(
        has_hole(bone.update(&a, project.read("a.rock"))),
        "a's hole"
    );
    assert!(
        has_hole(bone.update(&b, "fn b() -> int { 2 }\n".into())),
        "editing b must not drop a's cached hole"
    );
    assert!(
        !has_hole(bone.update(&a, "fn a() -> int { 1 }\n".into())),
        "removing a's `?` must clear its hole"
    );
}

/// A reinferred dependent that has a hole is never served a stale hole
/// message, even though hole-free dependents skip the hole reparse.
#[test]
fn dependent_hole_message_refreshes_after_interface_edit() {
    let project = Project::new(&[
        MANIFEST,
        ("b.rock", "fn give() -> int { 1 }\n"),
        ("a.rock", "fn use_it() {\n  let x: ? = give()\n}\n"),
    ]);
    let (a, b) = (project.path("a.rock"), project.path("b.rock"));
    let hole_msg = |per| {
        diagnostics_in(per, &a)
            .into_iter()
            .find(|x| x.message.contains("hole"))
            .map(|x| x.message)
    };
    let mut bone = BoneWorkspace::new();
    bone.load_project(&a);
    assert_eq!(
        hole_msg(bone.update(&a, project.read("a.rock"))).as_deref(),
        Some("type hole ? inferred as int")
    );
    assert_eq!(
        hole_msg(bone.update(&b, "fn give() -> float { 1.0 }\n".into())).as_deref(),
        Some("type hole ? inferred as float")
    );
}

/// The `./` hint has two labels, the `/` cause and the use site; each gets a
/// published diagnostic so the editor paints both lines.
#[test]
fn multi_label_hint_publishes_a_diagnostic_per_line() {
    let src = "fn main() {\n  let h = 8 / 3\n  let x: int = 2 * h\n}\n";
    let project = Project::new(&[MANIFEST, ("a.rock", src)]);
    let (_, per_file) = project.open("a.rock");
    let diags = diagnostics_in(per_file, &project.path("a.rock"));
    let hint_lines: Vec<u32> = diags
        .iter()
        .filter(|d| d.message.contains("./") || d.message.contains("used where `int`"))
        .map(|d| d.range.start.line)
        .collect();
    assert!(
        hint_lines.contains(&1) && hint_lines.contains(&2),
        "{diags:?}"
    );
}

/// Only the whole-project pass sees a duplicate struct across modules; it was
/// once logged and dropped while `rock compile` rejected the program.
#[test]
fn duplicate_struct_across_modules_surfaces_in_lsp() {
    let project = Project::new(&[
        MANIFEST,
        ("a.rock", "struct Foo { x: int }\nfn main() -> int { 0 }\n"),
        (
            "b.rock",
            "struct Foo { y: int }\nfn helper() -> int { 1 }\n",
        ),
    ]);
    let (a, b) = (project.path("a.rock"), project.path("b.rock"));
    let dup = |d: &Diagnostic| d.message.contains("duplicate struct `Foo`");
    let (mut bone, per_file) = project.open("a.rock");
    assert!(
        per_file.iter().flat_map(|(_, d)| d).any(dup),
        "{per_file:?}"
    );
    // The error is symmetric and attaches by module order, so either file
    // may carry it.
    let (da, db) = (bone.diagnostics_for(&a), bone.diagnostics_for(&b));
    assert!(
        da.iter().chain(&db).any(dup),
        "diagnostics_for: a={da:?} b={db:?}"
    );

    let per_file = bone.update(
        &b,
        "struct Bar { y: int }\nfn helper() -> int { 1 }\n".into(),
    );
    assert!(
        !per_file.iter().flat_map(|(_, d)| d).any(dup),
        "{per_file:?}"
    );
    assert!(
        !bone
            .diagnostics_for(&a)
            .iter()
            .any(|d| d.message.contains("duplicate struct")),
        "diagnostics_for after the fix"
    );
}

fn source_load_failure_recovers(broken_manifest: bool) {
    let manifest = "[package]\nname = \"dep\"\ntype = \"lib\"\n";
    let dependency = "struct Dependency { value: int }\n";
    let src = "fn main() -> int { sibling() }\n";
    let sibling = "fn sibling() -> int { let d = Dependency { value: 1 }; d.value }\n";
    let project = Project::new(&[
        (
            "app/stick.toml",
            "[package]\nname = \"app\"\n[dependencies]\ndep = { path = \"../dep\" }\n",
        ),
        ("dep/stick.toml", manifest),
        ("dep/types.rock", dependency),
        ("app/main.rock", src),
        ("app/sibling.rock", sibling),
    ]);
    let dep = project.dir.path().join("dep");
    let cause = if broken_manifest {
        std::fs::write(dep.join("stick.toml"), "[package").unwrap();
        "stick.toml"
    } else {
        // Invalid UTF-8 is unreadable by the source loader, even as root.
        std::fs::write(dep.join("types.rock"), [0xff, 0xfe]).unwrap();
        "types.rock"
    };
    let main = project.path("app/main.rock");
    let reports_cause = |diags: &[Diagnostic]| {
        diags.iter().any(|d| {
            d.severity == Some(DiagnosticSeverity::ERROR)
                && d.source.as_deref() == Some("bone")
                && d.message.contains(cause)
        })
    };

    let mut bone = BoneWorkspace::new();
    bone.load_project(&main);
    let diags = bone.diagnostics_for(&main);
    assert!(reports_cause(&diags), "CLI diagnostics: {diags:?}");
    let lints = bone.lint_diagnostics(&main);
    assert!(reports_cause(&lints), "lint diagnostics: {lints:?}");
    for _ in 0..2 {
        let per = bone.update(&main, src.to_string());
        assert!(per.iter().any(|(p, _)| *p == main), "edited file published");
        let diags = diagnostics_in(per, &main);
        assert!(reports_cause(&diags), "live updates: {diags:?}");
    }

    std::fs::write(dep.join("stick.toml"), manifest).unwrap();
    std::fs::write(dep.join("types.rock"), dependency).unwrap();
    let per = bone.update(&main, src.to_string());
    assert!(all_errors(&per).is_empty(), "repair: {per:?}");
    assert_eq!(
        bone.file_text(&project.path("app/sibling.rock")),
        Some(sibling),
        "unopened sibling indexed after repair"
    );
    let dep_path = project.path("dep/types.rock");
    assert!(
        bone.projects_summary()
            .iter()
            .flat_map(|(_, modules)| modules)
            .any(|m| m.id == dep_path),
        "dependency indexed in the consuming project after repair"
    );
    assert!(!reports_cause(&bone.diagnostics_for(&main)), "CLI cleared");
    assert!(
        !reports_cause(&bone.lint_diagnostics(&main)),
        "lint cleared"
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
    let src = "fn main() -> int { 1 }\n";
    let project = Project::new(&[("stick.toml", "[package"), ("main.rock", src)]);
    let file = project.dir.path().join("main.rock");
    let file = file.to_str().unwrap();
    let mut bone = BoneWorkspace::new();
    let per = bone.update(file, src.to_string());
    assert!(
        all_errors(&per)
            .iter()
            .any(|d| d.message.contains("stick.toml")),
        "{per:?}"
    );
    let lints = bone.lint_diagnostics(file);
    assert!(
        lints.iter().any(|d| d.message.contains("stick.toml")),
        "{lints:?}"
    );
}

#[test]
fn standalone_buffer_without_disk_file_needs_no_project() {
    let project = Project::new(&[]);
    let file = project.dir.path().join("scratch.rock");
    let file = file.to_str().unwrap();
    let src = "fn scratch() -> int { 1 }\n";
    let mut bone = BoneWorkspace::new();
    let per = bone.update(file, src.to_string());
    assert!(all_errors(&per).is_empty(), "{per:?}");
    assert_eq!(bone.file_text(file), Some(src));
}

#[test]
fn missing_first_buffer_loads_project_and_keeps_unsaved_edits() {
    let project = two_file_project();
    let missing = project.dir.path().join("new.rock");
    let missing = missing.to_string_lossy();
    let mut bone = BoneWorkspace::new();
    let per = bone.update(&missing, String::new());
    assert!(all_errors(&per).is_empty(), "{per:?}");
    let text = "fn draft() -> Foo { Foo { x: 1 } }\n";
    let per = bone.update(&missing, text.into());
    assert!(all_errors(&per).is_empty(), "{per:?}");
    bone.update(&project.path("b.rock"), project.read("b.rock"));
    assert_eq!(bone.file_text(&missing), Some(text));
    assert!(!std::path::Path::new(&*missing).exists());
}

#[test]
fn first_unsaved_source_loads_dependencies_of_empty_project() {
    let project = Project::new(&[
        (
            "app/stick.toml",
            "[package]\nname = \"app\"\n[dependencies]\ndep = \"../dep\"\n",
        ),
        ("dep/stick.toml", "[package]\nname = \"dep\"\n"),
        ("dep/types.rock", "struct Foo { x: int }\n"),
    ]);
    let path = project.dir.path().join("app/new.rock");
    let path = path.to_string_lossy();
    let mut bone = BoneWorkspace::new();
    let per = bone.update(&path, "fn draft() -> Foo { Foo { x: 1 } }\n".into());
    assert!(all_errors(&per).is_empty(), "{per:?}");
    assert!(bone.hover(&path, lsp_types::Position::new(0, 14)).is_some());
}
