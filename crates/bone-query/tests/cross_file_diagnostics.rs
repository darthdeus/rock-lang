//! The editor opens one buffer at a time, but every file's diagnostics must stay current.

use crate::fixture::{Project, diagnostics_in};
use bone_query::BoneWorkspace;
use lsp_types::{Diagnostic, DiagnosticSeverity};

const MANIFEST: (&str, &str) = (
    "stick.toml",
    "[package]\nname = \"xp\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n",
);

/// `a.rock` defines `Foo`, `b.rock` uses it.
fn two_file_project() -> Project {
    Project::new(&[
        MANIFEST,
        ("main.rock", "fn main() -> int { 0 }\n"),
        ("a.rock", "pub mod a;\npub struct Foo { x: int }\n"),
        (
            "b.rock",
            "pub mod b;\nuse super::a::Foo;\nfn use_it() {\n  let f = Foo { x: 1 }\n}\n",
        ),
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

/// A reinferred dependent with a hole is never served a stale hole message.
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

/// Both labels of the `./` hint get a published diagnostic, so the editor paints both lines.
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

/// Only the whole-project pass sees duplicate structs in separate fragments of one module.
#[test]
fn duplicate_struct_across_fragments_surfaces_in_lsp() {
    for repaired in ["a.rock", "b.rock"] {
        let project = Project::new(&[
            MANIFEST,
            ("main.rock", "fn main() -> int { 0 }\n"),
            ("a.rock", "pub mod shared;\npub struct Foo { x: int }\n"),
            (
                "b.rock",
                "pub mod shared;\npub struct Foo { y: int }\nfn helper() -> int { 1 }\n",
            ),
        ]);
        let (a, b) = (project.path("a.rock"), project.path("b.rock"));
        let dup = |d: &Diagnostic| {
            d.message
                .contains("duplicate definition `Foo` in module `shared`")
        };
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
            &project.path(repaired),
            "pub mod shared;\npub struct Bar { y: int }\n".into(),
        );
        assert!(
            !per_file.iter().flat_map(|(_, d)| d).any(dup),
            "{per_file:?}"
        );
        assert!(
            !bone
                .diagnostics_for(&a)
                .iter()
                .any(|d| d.message.contains("duplicate definition `Foo`")),
            "diagnostics_for after the fix"
        );
        for file in [&a, &b] {
            assert!(
                bone.hover(file, lsp_types::Position::new(1, 12)).is_some(),
                "{repaired}: {file}"
            );
        }
    }
}

fn source_load_failure_recovers(broken_manifest: bool) {
    let manifest = "[package]\nname = \"dep\"\ntype = \"lib\"\n\n[language]\nmodules = \"v1\"\n";
    let dependency = "pub mod types;\npub struct Dependency { value: int }\n";
    let src = "fn main() -> int { sibling() }\n";
    let sibling = "use dep::types::Dependency;\nfn sibling() -> int { let d = Dependency { value: 1 }; d.value }\n";
    let project = Project::new(&[
        (
            "app/stick.toml",
            "[package]\nname = \"app\"\ntype = \"bin\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n\n[language]\nmodules = \"v1\"\n",
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
fn missing_first_buffer_loads_project_and_keeps_unsaved_edits() {
    let project = two_file_project();
    let missing = project.dir.path().join("new.rock");
    let missing = missing.to_string_lossy();
    let mut bone = BoneWorkspace::new();
    let per = bone.update(&missing, String::new());
    assert!(all_errors(&per).is_empty(), "{per:?}");
    let text = "pub mod new;\nuse super::a::Foo;\nfn draft() -> Foo { Foo { x: 1 } }\n";
    let per = bone.update(&missing, text.into());
    assert!(all_errors(&per).is_empty(), "{per:?}");
    bone.update(&project.path("b.rock"), project.read("b.rock"));
    assert_eq!(bone.file_text(&missing), Some(text));
    assert!(!std::path::Path::new(&*missing).exists());
}

#[test]
fn reexport_cycle_has_the_same_diagnostics_from_disk_and_edits() {
    let clean_a = "pub mod a;\n";
    let clean_b = "pub mod b;\n";
    let cycle_a = "pub mod a;\npub use super::b::back as start;\n";
    let cycle_b = "pub mod b;\npub use super::a::start as back;\n";
    let project = Project::new(&[
        MANIFEST,
        ("main.rock", "fn main() {}\n"),
        ("a.rock", clean_a),
        ("b.rock", clean_b),
    ]);
    let (a, b) = (project.path("a.rock"), project.path("b.rock"));
    let snapshot = |bone: &BoneWorkspace| {
        [&a, &b].map(|file| {
            let mut diagnostics = bone.diagnostics_for(file);
            diagnostics.retain(|d| d.severity == Some(DiagnosticSeverity::ERROR));
            diagnostics.sort_by_key(|d| (d.range.start, d.message.clone()));
            diagnostics
        })
    };
    let mut retained = BoneWorkspace::new();
    retained.load_project(&a);
    retained.update(&a, cycle_a.into());
    retained.update(&b, cycle_b.into());

    std::fs::write(&a, cycle_a).unwrap();
    std::fs::write(&b, cycle_b).unwrap();
    let mut fresh = BoneWorkspace::new();
    fresh.load_project(&a);
    assert_eq!(snapshot(&fresh), snapshot(&retained));
    assert!(snapshot(&fresh).iter().all(|errors| errors.len() == 1));

    for bone in [&mut retained, &mut fresh] {
        bone.update(&a, clean_a.into());
        bone.update(&b, clean_b.into());
        assert!(snapshot(bone).iter().all(Vec::is_empty));
    }
}
