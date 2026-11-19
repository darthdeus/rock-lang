//! `.rock.tags` (extended ctags) emission through `BoneWorkspace::write_ctags`.

use crate::fixture::Project;
use bone_query::BoneWorkspace;

fn tags_in(project: &Project, dir: &str) -> String {
    std::fs::read_to_string(project.dir.path().join(dir).join(".rock.tags")).unwrap()
}

fn has_tag(tags: &str, name: &str) -> bool {
    tags.lines().any(|l| l.starts_with(&format!("{name}\t")))
}

#[test]
fn macro_tag_line_follows_a_body_edit_above_it() {
    let before = "fn helper() -> int {\n  1\n}\n\nmacro twice(x: expr) -> int { x + x }\n";
    let after = before.replace("  1\n", "  let n = 2\n  n\n");
    let project = Project::new(&[("input.rock", before)]);
    let path = project.path("input.rock");
    let mut bone = BoneWorkspace::new();
    for (source, line) in [(before, "5;\""), (&after, "6;\"")] {
        bone.update(&path, source.into());
        bone.write_ctags(&path).unwrap();
        let tags = tags_in(&project, "");
        let tag = tags.lines().find(|l| l.starts_with("twice\t")).unwrap();
        assert_eq!(tag.split('\t').nth(2), Some(line));
    }
}

#[test]
fn write_ctags_emits_every_symbol_kind_with_its_fields() {
    let src = "fn add(a: int, b: int) -> int {\n  a + b\n}\n\nstruct Point {\n  x: int,\n  y: int,\n}\n\nenum Color {\n  Red,\n  Blue,\n}\n\nglobal MAX_HP: int = 100\ndeco init my_system(stage: int = 100) {\n}\nmacro twice(x: expr, rest: ...expr) -> string {\n  x\n}\n";
    let project = Project::new(&[("game.rock", src)]);
    // Tags name the canonical path; on macOS the temp dir is behind a symlink.
    let path = project.dir.path().join("game.rock").canonicalize().unwrap();
    let path = path.to_string_lossy();
    let mut bone = BoneWorkspace::new();
    bone.update(&path, src.to_string());
    let count = bone.write_ctags(&path).expect("write_ctags");
    let tags = tags_in(&project, "");
    assert!(
        tags.starts_with("!_TAG_FILE_FORMAT\t2\t/extended format/\n"),
        "{tags}"
    );
    let headers = tags.lines().filter(|l| l.starts_with('!')).count();
    assert_eq!(
        count,
        tags.lines().count() - headers,
        "count is the tag lines"
    );
    let own: Vec<String> = tags
        .lines()
        .filter(|l| l.contains(&*path))
        .map(|l| l.replace(&*path, "FILE"))
        .collect();
    assert_eq!(
        own,
        [
            "Blue\tFILE\t12;\"\tg\tenum:Color",
            "Color\tFILE\t10;\"\te",
            "MAX_HP\tFILE\t15;\"\tv\ttype:int",
            "Point\tFILE\t5;\"\ts",
            "Red\tFILE\t11;\"\tg\tenum:Color",
            "add\tFILE\t1;\"\tf\tsignature:(a: int, b: int)\treturns:int",
            "my_system\tFILE\t16;\"\td\tsignature:(stage: int)",
            "twice\tFILE\t18;\"\tM\tsignature:(x: expr, rest: ...expr)\treturns:string",
            "x\tFILE\t6;\"\tm\tstruct:Point",
            "y\tFILE\t7;\"\tm\tstruct:Point",
        ]
    );
}

/// A save refreshes every loaded root, since the editor may hold a stale sibling.
#[test]
fn write_all_ctags_regenerates_every_open_root() {
    let project = Project::new(&[
        ("alpha/stick.toml", "[package]\nname = \"alpha\"\n"),
        ("beta/stick.toml", "[package]\nname = \"beta\"\n"),
        ("alpha/a.rock", "fn alpha_fn() -> int { 1 }\n"),
        ("beta/b.rock", "fn beta_fn() -> int { 2 }\n"),
        ("alpha/.rock.tags", "STALE\n"),
        ("beta/.rock.tags", "STALE\n"),
    ]);
    let mut bone = BoneWorkspace::new();
    for file in ["alpha/a.rock", "beta/b.rock"] {
        bone.update(&project.path(file), project.read(file));
    }
    let results = bone.write_all_ctags();
    assert_eq!(results.len(), 2, "{results:?}");
    for (root, res) in &results {
        res.as_ref()
            .unwrap_or_else(|e| panic!("write failed for {}: {e}", root.display()));
    }
    for (dir, name) in [("alpha", "alpha::alpha_fn"), ("beta", "beta::beta_fn")] {
        let tags = tags_in(&project, dir);
        assert!(has_tag(&tags, name), "{dir} not refreshed:\n{tags}");
        assert!(!tags.contains("STALE"), "{dir} still stale:\n{tags}");
    }
}

/// A scratch file's root falls back to its parent; the save sweep must not scatter tags there.
#[test]
fn write_all_ctags_skips_standalone_non_stick_roots() {
    let project = Project::new(&[
        ("proj/stick.toml", "[package]\nname = \"proj\"\n"),
        ("proj/a.rock", "fn alpha_fn() -> int { 1 }\n"),
        ("scratch.rock", "fn scratch_fn() -> int { 9 }\n"),
    ]);
    let mut bone = BoneWorkspace::new();
    for file in ["proj/a.rock", "scratch.rock"] {
        bone.update(&project.path(file), project.read(file));
    }
    let results = bone.write_all_ctags();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(results[0].0.ends_with("proj"), "{:?}", results[0].0);
    assert!(project.dir.path().join("proj/.rock.tags").exists());
    assert!(!project.dir.path().join(".rock.tags").exists());
}

/// Tags cover unopened siblings and the stdlib, pointing at the real stdlib file.
#[test]
fn write_ctags_covers_whole_project_and_stdlib() {
    let project = Project::new(&[
        ("stick.toml", "[package]\nname = \"proj\"\n"),
        ("a.rock", "fn alpha() -> int { 1 }\n"),
        ("b.rock", "fn beta() -> int { 2 }\n"),
    ]);
    let (bone, _) = project.open("a.rock");
    bone.write_ctags(&project.path("a.rock"))
        .expect("write_ctags");
    let tags = tags_in(&project, "");
    assert!(has_tag(&tags, "proj::alpha"), "opened file");
    assert!(has_tag(&tags, "proj::beta"), "unopened sibling");
    let push = tags
        .lines()
        .find(|l| l.starts_with("vec_push\t"))
        .expect("stdlib vec_push tag");
    assert!(push.contains("unified.rock"), "{push}");
}
