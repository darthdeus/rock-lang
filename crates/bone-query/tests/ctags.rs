//! `.rock.tags` (ctags) emission from the bone workspace.
//!
//! The LSP regenerates a project's ctags file on save by calling
//! `BoneWorkspace::write_ctags`; this drives that method directly and asserts
//! the on-disk extended-ctags output (name / file / line / kind) for each
//! top-level symbol category. Mirrors the format the old compile-path generator
//! produced (`rock-cli/src/ctags.rs`), now sourced from bone's HIR.

use bone_query::BoneWorkspace;

#[test]
fn write_ctags_emits_extended_tags_for_top_level_symbols() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("game.rock");
    // line 1: fn add   line 5: struct Point   line 10: enum Color   line 15: global
    let src = "fn add(a: int, b: int) -> int {\n  a + b\n}\n\nstruct Point {\n  x: int,\n  y: int,\n}\n\nenum Color {\n  Red,\n  Blue,\n}\n\nglobal MAX_HP: int = 100\n";
    std::fs::write(&file, src).unwrap();
    let path = file.to_string_lossy().to_string();

    let mut bone = BoneWorkspace::new();
    bone.update(&path, src.to_string());

    let count = bone.write_ctags(&path).expect("write_ctags should succeed");
    assert!(count >= 3, "expected >=3 symbols emitted, got {count}");

    let tags = std::fs::read_to_string(dir.path().join(".rock.tags"))
        .expect(".rock.tags should be written to the project root");

    assert!(
        tags.contains("!_TAG_FILE_FORMAT"),
        "ctags header missing:\n{tags}"
    );
    // `name<TAB>file<TAB>line;"<TAB>kind` -- check name + kind char precisely.
    assert!(
        tags.lines()
            .any(|l| l.starts_with("add\t") && l.contains(";\"\tf")),
        "function `add` (kind f) tag missing:\n{tags}"
    );
    assert!(
        tags.lines()
            .any(|l| l.starts_with("Point\t") && l.contains(";\"\ts")),
        "struct `Point` (kind s) tag missing:\n{tags}"
    );
    assert!(
        tags.lines()
            .any(|l| l.starts_with("Color\t") && l.contains(";\"\te")),
        "enum `Color` (kind e) tag missing:\n{tags}"
    );
    assert!(
        tags.lines()
            .any(|l| l.starts_with("MAX_HP\t") && l.contains(";\"\tv")),
        "global `MAX_HP` (kind v) tag missing:\n{tags}"
    );
    // line number is 1-based: `add` is declared on line 1.
    assert!(
        tags.lines()
            .any(|l| l.starts_with("add\t") && l.contains("\t1;\"")),
        "function `add` should be tagged at line 1:\n{tags}"
    );

    // Functions carry enriched `signature:` / `returns:` fields (sourced from
    // bone's HIR), matching the `rock ctags` extended format.
    let add_line = tags
        .lines()
        .find(|l| l.starts_with("add\t"))
        .expect("add tag present");
    assert!(
        add_line.contains("\tsignature:(a: int, b: int)"),
        "function `add` should carry its parameter signature:\n{add_line}"
    );
    assert!(
        add_line.contains("\treturns:int"),
        "function `add` should carry its return type:\n{add_line}"
    );
    // Non-function symbols carry no signature/returns fields.
    let point_line = tags
        .lines()
        .find(|l| l.starts_with("Point\t"))
        .expect("Point tag present");
    assert!(
        !point_line.contains("signature:") && !point_line.contains("returns:"),
        "struct `Point` should not carry function fields:\n{point_line}"
    );
}

#[test]
fn write_ctags_emits_struct_fields_and_enum_variants() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("game.rock");
    // struct Point { x, y } on lines 1-4; enum Color { Red, Blue } on lines 6-9.
    let src = "struct Point {\n  x: int,\n  y: int,\n}\n\nenum Color {\n  Red,\n  Blue,\n}\n";
    std::fs::write(&file, src).unwrap();
    let path = file.to_string_lossy().to_string();

    let mut bone = BoneWorkspace::new();
    bone.update(&path, src.to_string());

    bone.write_ctags(&path).expect("write_ctags should succeed");
    let tags = std::fs::read_to_string(dir.path().join(".rock.tags"))
        .expect(".rock.tags should be written to the project root");

    // Struct fields are tagged as members (kind `m`) scoped to their struct.
    assert!(
        tags.lines()
            .any(|l| l.starts_with("x\t") && l.contains(";\"\tm") && l.contains("\tstruct:Point")),
        "struct field `x` (kind m, scope struct:Point) tag missing:\n{tags}"
    );
    assert!(
        tags.lines()
            .any(|l| l.starts_with("y\t") && l.contains(";\"\tm") && l.contains("\tstruct:Point")),
        "struct field `y` (kind m, scope struct:Point) tag missing:\n{tags}"
    );
    // Field line numbers are 1-based: `x` is declared on line 2.
    assert!(
        tags.lines()
            .any(|l| l.starts_with("x\t") && l.contains("\t2;\"")),
        "struct field `x` should be tagged at line 2:\n{tags}"
    );

    // Enum variants are tagged as enumerators (kind `g`) scoped to their enum.
    assert!(
        tags.lines()
            .any(|l| l.starts_with("Red\t") && l.contains(";\"\tg") && l.contains("\tenum:Color")),
        "enum variant `Red` (kind g, scope enum:Color) tag missing:\n{tags}"
    );
    assert!(
        tags.lines()
            .any(|l| l.starts_with("Blue\t") && l.contains(";\"\tg") && l.contains("\tenum:Color")),
        "enum variant `Blue` (kind g, scope enum:Color) tag missing:\n{tags}"
    );
}

#[test]
fn write_ctags_emits_decorators() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("game.rock");
    // A user-defined decorator on line 1.
    let src = "deco init my_system(stage: int = 100) {\n}\n";
    std::fs::write(&file, src).unwrap();
    let path = file.to_string_lossy().to_string();

    let mut bone = BoneWorkspace::new();
    bone.update(&path, src.to_string());

    bone.write_ctags(&path).expect("write_ctags should succeed");
    let tags = std::fs::read_to_string(dir.path().join(".rock.tags"))
        .expect(".rock.tags should be written to the project root");

    // Decorators are tagged as kind `d`, carrying their parameter signature.
    let deco_line = tags
        .lines()
        .find(|l| l.starts_with("my_system\t"))
        .unwrap_or_else(|| panic!("decorator `my_system` tag missing:\n{tags}"));
    assert!(
        deco_line.contains(";\"\td"),
        "decorator `my_system` should be kind d:\n{deco_line}"
    );
    assert!(
        deco_line.contains("\tsignature:(stage: int)"),
        "decorator `my_system` should carry its parameter signature:\n{deco_line}"
    );
    // 1-based line number.
    assert!(
        deco_line.contains("\t1;\""),
        "decorator `my_system` should be tagged at line 1:\n{deco_line}"
    );
}

#[test]
fn write_ctags_emits_global_type() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("game.rock");
    let src = "global MAX_HP: int = 100\n";
    std::fs::write(&file, src).unwrap();
    let path = file.to_string_lossy().to_string();

    let mut bone = BoneWorkspace::new();
    bone.update(&path, src.to_string());

    bone.write_ctags(&path).expect("write_ctags should succeed");
    let tags = std::fs::read_to_string(dir.path().join(".rock.tags"))
        .expect(".rock.tags should be written to the project root");

    let g = tags
        .lines()
        .find(|l| l.starts_with("MAX_HP\t"))
        .unwrap_or_else(|| panic!("global `MAX_HP` tag missing:\n{tags}"));
    assert!(g.contains(";\"\tv"), "global should be kind v:\n{g}");
    assert!(
        g.contains("\ttype:int"),
        "global should carry its declared type:\n{g}"
    );
}

#[test]
fn write_ctags_emits_macros() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("game.rock");
    // A macro with typed params and a return type, on line 1.
    let src = "macro twice(x: expr, rest: ...expr) -> string {\n  x\n}\n";
    std::fs::write(&file, src).unwrap();
    let path = file.to_string_lossy().to_string();

    let mut bone = BoneWorkspace::new();
    bone.update(&path, src.to_string());

    bone.write_ctags(&path).expect("write_ctags should succeed");
    let tags = std::fs::read_to_string(dir.path().join(".rock.tags"))
        .expect(".rock.tags should be written to the project root");

    let mac = tags
        .lines()
        .find(|l| l.starts_with("twice\t"))
        .unwrap_or_else(|| panic!("macro `twice` tag missing:\n{tags}"));
    assert!(
        mac.contains(";\"\tM"),
        "macro `twice` should be kind M:\n{mac}"
    );
    assert!(
        mac.contains("\tsignature:(x: expr, rest: ...expr)"),
        "macro `twice` should carry its (variadic) parameter signature:\n{mac}"
    );
    assert!(
        mac.contains("\treturns:string"),
        "macro `twice` should carry its return type:\n{mac}"
    );
    assert!(
        mac.contains("\t1;\""),
        "macro `twice` should be tagged at line 1:\n{mac}"
    );
}

#[test]
fn write_all_ctags_regenerates_every_open_root() {
    // Two separate stick projects, a file open in each. A save in ONE must
    // refresh BOTH `.rock.tags` (the editor may hold a stale sibling from a
    // prior session) -- `write_all_ctags` regenerates every loaded root, not
    // just the edited file's root.
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("alpha");
    let pb = dir.path().join("beta");
    std::fs::create_dir_all(&pa).unwrap();
    std::fs::create_dir_all(&pb).unwrap();
    std::fs::write(pa.join("stick.toml"), "[package]\nname = \"alpha\"\n").unwrap();
    std::fs::write(pb.join("stick.toml"), "[package]\nname = \"beta\"\n").unwrap();
    std::fs::write(pa.join("a.rock"), "fn alpha_fn() -> int { 1 }\n").unwrap();
    std::fs::write(pb.join("b.rock"), "fn beta_fn() -> int { 2 }\n").unwrap();

    let a = pa.join("a.rock").to_string_lossy().to_string();
    let b = pb.join("b.rock").to_string_lossy().to_string();
    let mut bone = BoneWorkspace::new();
    bone.update(&a, "fn alpha_fn() -> int { 1 }\n".to_string());
    bone.update(&b, "fn beta_fn() -> int { 2 }\n".to_string());

    // Pre-seed both tag files with stale junk to prove they get rewritten.
    std::fs::write(pa.join(".rock.tags"), "STALE\n").unwrap();
    std::fs::write(pb.join(".rock.tags"), "STALE\n").unwrap();

    // One call, no path argument: regenerate every open root.
    let results = bone.write_all_ctags();
    assert_eq!(
        results.len(),
        2,
        "expected both roots regenerated, got {results:?}"
    );
    for (root, res) in &results {
        res.as_ref()
            .unwrap_or_else(|e| panic!("write failed for {}: {e}", root.display()));
    }

    let tags_a = std::fs::read_to_string(pa.join(".rock.tags")).unwrap();
    let tags_b = std::fs::read_to_string(pb.join(".rock.tags")).unwrap();
    assert!(
        tags_a.lines().any(|l| l.starts_with("alpha_fn\t")),
        "alpha root not refreshed:\n{tags_a}"
    );
    assert!(
        tags_b.lines().any(|l| l.starts_with("beta_fn\t")),
        "beta root not refreshed:\n{tags_b}"
    );
    assert!(
        !tags_a.contains("STALE"),
        "alpha tags still stale:\n{tags_a}"
    );
    assert!(
        !tags_b.contains("STALE"),
        "beta tags still stale:\n{tags_b}"
    );
}

#[test]
fn write_all_ctags_skips_standalone_non_stick_roots() {
    // A real stick project plus a standalone scratch `.rock` opened in the same
    // session (no stick.toml -- its root falls back to the parent dir). The
    // all-roots save sweep must write ONLY the stick project's tags, never
    // scatter a `.rock.tags` into the scratch file's parent (`~`/`/tmp` in real
    // use).
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("stick.toml"), "[package]\nname = \"proj\"\n").unwrap();
    std::fs::write(proj.join("a.rock"), "fn alpha_fn() -> int { 1 }\n").unwrap();
    // Standalone scratch file directly under the tempdir (no stick.toml here).
    std::fs::write(
        dir.path().join("scratch.rock"),
        "fn scratch_fn() -> int { 9 }\n",
    )
    .unwrap();

    let a = proj.join("a.rock").to_string_lossy().to_string();
    let scratch = dir
        .path()
        .join("scratch.rock")
        .to_string_lossy()
        .to_string();
    let mut bone = BoneWorkspace::new();
    bone.update(&a, "fn alpha_fn() -> int { 1 }\n".to_string());
    bone.update(&scratch, "fn scratch_fn() -> int { 9 }\n".to_string());

    let results = bone.write_all_ctags();
    assert_eq!(
        results.len(),
        1,
        "only the stick root should be swept, got {results:?}"
    );
    assert!(
        results[0].0.ends_with("proj"),
        "the swept root should be the stick project, got {:?}",
        results[0].0
    );
    assert!(
        proj.join(".rock.tags").exists(),
        "stick project tags should be written"
    );
    assert!(
        !dir.path().join(".rock.tags").exists(),
        "no .rock.tags should be scattered into the scratch file's non-stick parent"
    );
}

#[test]
fn write_ctags_covers_whole_project_not_just_opened_file() {
    // A stick.toml project with two files. Opening ONE (a single `update`, which
    // batch-loads the project) must still tag symbols from BOTH -- the ctags file
    // is workspace-wide, not limited to the edited buffer.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("stick.toml"),
        "[package]\nname = \"proj\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("a.rock"), "fn alpha() -> int { 1 }\n").unwrap();
    std::fs::write(dir.path().join("b.rock"), "fn beta() -> int { 2 }\n").unwrap();

    let a = dir.path().join("a.rock").to_string_lossy().to_string();
    let mut bone = BoneWorkspace::new();
    // Open only a.rock.
    bone.update(&a, "fn alpha() -> int { 1 }\n".to_string());

    bone.write_ctags(&a).expect("write_ctags should succeed");
    let tags = std::fs::read_to_string(dir.path().join(".rock.tags")).expect(".rock.tags written");

    assert!(
        tags.lines().any(|l| l.starts_with("alpha\t")),
        "alpha (opened file) tag missing:\n{tags}"
    );
    assert!(
        tags.lines().any(|l| l.starts_with("beta\t")),
        "beta (unopened sibling) tag missing:\n{tags}"
    );
}

#[test]
fn write_ctags_includes_stdlib_symbols() {
    // The stdlib is part of the workspace, so its symbols must land in the
    // project's tags file (jump-to-stdlib-by-name), pointing at the real stdlib
    // file. `vec_push` is a stable stdlib function.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"ct\"\n").unwrap();
    let file = dir.path().join("main.rock");
    std::fs::write(
        &file,
        "fn main() -> int {\n  let xs = [1]\n  xs.push(2)\n  xs.len()\n}\n",
    )
    .unwrap();
    let path = file.to_string_lossy().to_string();

    let mut bone = BoneWorkspace::new();
    bone.update(&path, std::fs::read_to_string(&file).unwrap());
    bone.write_ctags(&path).expect("write_ctags should succeed");

    let tags = std::fs::read_to_string(dir.path().join(".rock.tags")).expect(".rock.tags written");
    let push_tag = tags.lines().find(|l| l.starts_with("vec_push\t"));
    assert!(
        push_tag.is_some(),
        "stdlib `vec_push` tag missing:\n(first 20 lines)\n{}",
        tags.lines().take(20).collect::<Vec<_>>().join("\n")
    );
    assert!(
        push_tag.unwrap().contains("unified.rock"),
        "vec_push tag must point at the stdlib file: {}",
        push_tag.unwrap()
    );
}
