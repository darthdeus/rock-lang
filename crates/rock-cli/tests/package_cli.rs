use crate::support::{failure, success};
use std::path::Path;
use std::process::Output;

fn run(cwd: &Path, args: &[&str], overlay: Option<&Path>) -> Output {
    let mut command = crate::support::rock(cwd);
    command.args(args);
    for key in [
        "ROCK_ASSET_OVERLAYS",
        "ROCK_CONTEXT_ROOT",
        "ROCK_PREBUILT_GAME",
    ] {
        command.env_remove(key);
    }
    if let Some(overlay) = overlay {
        command.env("ROCK_ASSET_OVERLAYS", overlay);
    }
    command.output().expect("run public packaging command")
}

fn project(root: &Path, extra_release: &str) {
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(root.join("main.rock"), "fn main() {}\n").unwrap();
    std::fs::write(root.join("assets/owned.txt"), "owned asset").unwrap();
    std::fs::write(
        root.join("stick.toml"),
        format!(
            "[package]\nname = \"sample\"\n[release]\nassets = [\"assets/owned.txt\"]\n{extra_release}\n"
        ),
    )
    .unwrap();
}

#[test]
fn package_requires_an_explicit_output_directory() {
    let root = tempfile::tempdir().unwrap();
    project(root.path(), "");
    failure(
        run(root.path(), &["package"], None),
        "package requires --output <directory>",
    );
}

#[test]
fn package_uses_the_current_project_and_selected_relative_output() {
    let root = tempfile::tempdir().unwrap();
    project(root.path(), "");
    success(run(root.path(), &["package", "--output", "dist"], None));
    let output = root.path().join("dist");
    assert!(!std::fs::read(output.join("main.bc")).unwrap().is_empty());
    assert!(output.join("main.lua").is_file());
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.join("package-manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["package"]["name"], "sample");
    assert_eq!(
        std::fs::read_to_string(output.join("assets/sample/assets/owned.txt")).unwrap(),
        "owned asset"
    );
}

#[test]
fn asset_check_and_package_validate_the_same_declared_overlay_assets() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("project");
    let overlay = root.path().join("overlay");
    project(&source, "overlay_assets = [\"assets/selected.txt\"]");
    std::fs::create_dir_all(overlay.join("assets")).unwrap();
    failure(
        run(root.path(), &["asset-check", "project"], Some(&overlay)),
        "declared overlay asset does not exist",
    );
    failure(
        run(
            root.path(),
            &["package", "project", "--output", "dist"],
            Some(&overlay),
        ),
        "declared overlay asset does not exist",
    );
    assert!(!root.path().join("dist").exists());
    std::fs::write(overlay.join("assets/selected.txt"), "selected overlay").unwrap();
    success(run(&source, &["asset-check"], Some(&overlay)));
    success(run(
        root.path(),
        &["package", "project", "--output", "dist"],
        Some(&overlay),
    ));
    let assets = root.path().join("dist/assets/sample/assets");
    assert_eq!(
        std::fs::read_to_string(assets.join("selected.txt")).unwrap(),
        "selected overlay"
    );
}

fn sheet(root: &Path, value: u8) -> String {
    use sha2::{Digest, Sha256};
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[value; 16])
            .unwrap();
    }
    let listing = format!("{:x}  color.png\n", Sha256::digest(&bytes));
    let digest = format!("{:x}", Sha256::digest(listing.as_bytes()));
    let directory = root.join(format!("assets/.eat/{digest}"));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("color.png"), bytes).unwrap();
    digest
}

fn bank(root: &Path, path: &str, digest: &str, size: u32) {
    std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
    std::fs::write(
        root.join(path),
        format!(
            "{{\"kind\":\"texture\",\"id\":\"floor\",\"dir\":\"{digest}\",\"size\":[{size},{size}],\"normal\":false,\"material\":false}}\n"
        ),
    )
    .unwrap();
}

fn manifest(root: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(root.join("dist/package-manifest.json")).unwrap())
        .unwrap()
}

fn diagnostics(output: &Output) -> Vec<String> {
    crate::support::text(output)
        .lines()
        .map(|line| {
            line.split_once("] ")
                .map_or(line, |(_, message)| message)
                .to_owned()
        })
        .collect()
}

fn reject_both(root: &Path, overlay: Option<&Path>, expected: &str) {
    let check = run(root, &["asset-check"], overlay);
    let diagnostic = diagnostics(&check);
    failure(check, expected);
    for existing in [false, true] {
        let output = root.join("dist");
        if existing {
            std::fs::create_dir_all(&output).unwrap();
            std::fs::write(output.join("sentinel"), "untouched").unwrap();
        }
        let package = run(root, &["package", "--output", "dist"], overlay);
        assert_eq!(diagnostics(&package), diagnostic);
        failure(package, expected);
        if existing {
            assert_eq!(
                std::fs::read_to_string(output.join("sentinel")).unwrap(),
                "untouched"
            );
            assert_eq!(std::fs::read_dir(&output).unwrap().count(), 1);
            std::fs::remove_dir_all(output).unwrap();
        } else {
            assert!(!output.exists());
        }
    }
}

#[test]
fn auxiliary_banks_select_shared_closures_without_a_main_bank_and_rebake() {
    let root = tempfile::tempdir().unwrap();
    let overlay = root.path().join("overlay");
    project(
        root.path(),
        "visual_banks = [\"assets/extra.jsonl\", \"assets/extra.jsonl\", \"assets/other.jsonl\"]",
    );
    let old = sheet(&overlay, 10);
    bank(root.path(), "assets/extra.jsonl", &old, 2);
    bank(root.path(), "assets/other.jsonl", &old, 2);
    // Unreferenced generations and loose overlay files must not be admitted.
    let stale = sheet(root.path(), 11);
    std::fs::write(overlay.join("assets/loose.txt"), "loose").unwrap();
    success(run(root.path(), &["asset-check"], Some(&overlay)));
    success(run(
        root.path(),
        &["package", "--output", "dist"],
        Some(&overlay),
    ));
    let old_key = format!("sample/assets/.eat/{old}/color.png");
    let staged = manifest(root.path());
    let keys = staged["artifacts"]["assets"]["keys"].as_array().unwrap();
    assert_eq!(
        keys.iter()
            .filter(|key| key.as_str() == Some(&old_key))
            .count(),
        1
    );
    assert!(
        !keys
            .iter()
            .any(|key| key.as_str().unwrap().contains(&stale))
    );
    assert!(
        !root
            .path()
            .join("dist/assets/sample/assets/loose.txt")
            .exists()
    );
    assert!(
        root.path()
            .join("dist/assets/sample/assets/extra.jsonl")
            .is_file()
    );
    let new = sheet(&overlay, 12);
    bank(root.path(), "assets/extra.jsonl", &new, 2);
    bank(root.path(), "assets/other.jsonl", &new, 2);
    success(run(
        root.path(),
        &["package", "--output", "dist"],
        Some(&overlay),
    ));
    assert!(!root.path().join("dist/assets").join(old_key).exists());
    assert!(
        root.path()
            .join(format!("dist/assets/sample/assets/.eat/{new}/color.png"))
            .is_file()
    );
}

#[test]
fn main_and_auxiliary_bank_closures_are_both_packaged() {
    let root = tempfile::tempdir().unwrap();
    project(root.path(), "visual_banks = [\"assets/extra.jsonl\"]");
    let main = sheet(root.path(), 1);
    let extra = sheet(root.path(), 2);
    bank(root.path(), "assets/visual_bank.jsonl", &main, 2);
    bank(root.path(), "assets/extra.jsonl", &extra, 2);
    success(run(root.path(), &["package", "--output", "dist"], None));
    for digest in [main, extra] {
        assert!(
            root.path()
                .join(format!("dist/assets/sample/assets/.eat/{digest}/color.png"))
                .is_file()
        );
    }
}

#[test]
fn missing_visual_dependency_rejects_both_commands_without_touching_output() {
    let root = tempfile::tempdir().unwrap();
    project(root.path(), "visual_banks = [\"assets/extra.jsonl\"]");
    bank(root.path(), "assets/extra.jsonl", &"a".repeat(64), 2);
    reject_both(root.path(), None, "color.png");
}

#[test]
fn every_bank_reference_is_verified_even_when_the_digest_is_shared() {
    let root = tempfile::tempdir().unwrap();
    project(
        root.path(),
        "visual_banks = [\"assets/extra.jsonl\", \"assets/other.jsonl\"]",
    );
    let digest = sheet(root.path(), 1);
    bank(root.path(), "assets/extra.jsonl", &digest, 2);
    bank(root.path(), "assets/other.jsonl", &digest, 4);
    reject_both(root.path(), None, "expected 4x4");
    bank(root.path(), "assets/other.jsonl", &digest, 2);
    let path = root.path().join("assets/extra.jsonl");
    let first = std::fs::read_to_string(&path).unwrap();
    let second = first.replace("floor", "wall").replace("[2,2]", "[4,4]");
    std::fs::write(path, first + &second).unwrap();
    reject_both(root.path(), None, "expected 4x4");
}

#[test]
fn overlay_entries_cannot_target_worktree_banks() {
    for (bank_path, overlay_path) in [
        ("assets/visual_bank.jsonl", "assets/visual_bank.jsonl"),
        ("assets/extra.jsonl", "assets/extra.jsonl"),
        ("assets/visual_bank.jsonl", "assets/./visual_bank.jsonl"),
        ("assets/extra.jsonl", "assets//extra.jsonl"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let overlay = root.path().join("overlay");
        project(
            root.path(),
            &format!("visual_banks = [\"{bank_path}\"]\noverlay_assets = [\"{overlay_path}\"]"),
        );
        let digest = sheet(root.path(), 1);
        bank(root.path(), bank_path, &digest, 2);
        bank(&overlay, bank_path, &digest, 4);
        reject_both(
            root.path(),
            Some(&overlay),
            "overlay_assets entry targets visual bank",
        );
    }
}

#[test]
fn asset_check_verifies_pngs_and_digests_like_package() {
    for corrupt_png in [false, true] {
        let root = tempfile::tempdir().unwrap();
        project(root.path(), "visual_banks = []");
        let digest = sheet(root.path(), 1);
        let wrong = "a".repeat(64);
        std::fs::rename(
            root.path().join(format!("assets/.eat/{digest}")),
            root.path().join(format!("assets/.eat/{wrong}")),
        )
        .unwrap();
        bank(root.path(), "assets/visual_bank.jsonl", &wrong, 2);
        if corrupt_png {
            std::fs::write(
                root.path().join(format!("assets/.eat/{wrong}/color.png")),
                "invalid",
            )
            .unwrap();
        }
        reject_both(
            root.path(),
            None,
            if corrupt_png { "decoding" } else { "hashes to" },
        );
    }
}

#[test]
fn undeclared_packaged_banks_warn_and_require_selected_dependencies() {
    let root = tempfile::tempdir().unwrap();
    project(root.path(), "");
    let path = root.path().join("stick.toml");
    let config = std::fs::read_to_string(&path)
        .unwrap()
        .replace("assets/owned.txt", "assets");
    std::fs::write(path, config).unwrap();
    let digest = sheet(root.path(), 1);
    bank(root.path(), "assets/extra.jsonl", &digest, 2);
    let checked = run(root.path(), &["asset-check"], None);
    assert!(crate::support::text(&checked).contains("not declared in [release].visual_banks"));
    success(checked);
    std::fs::remove_dir_all(root.path().join(format!("assets/.eat/{digest}"))).unwrap();
    reject_both(root.path(), None, "missing selected file");
}

#[test]
fn bank_roots_are_canonical_existing_worktree_jsonl_files() {
    for path in [
        "assets/../extra.jsonl",
        "assets/./extra.jsonl",
        "assets//extra.jsonl",
        "assets/*.jsonl",
        "assets/extra.txt",
        "../extra.jsonl",
        "/assets/extra.jsonl",
        "assets/extra.jsonl/",
        "assets/extra?.jsonl",
        "assets/extra[1].jsonl",
    ] {
        let root = tempfile::tempdir().unwrap();
        project(
            root.path(),
            &format!("visual_banks = [{}]", serde_json::to_string(path).unwrap()),
        );
        reject_both(
            root.path(),
            None,
            "canonical project-relative assets/...jsonl",
        );
    }
    for path in ["assets/visual_bank.jsonl", "assets/extra.jsonl"] {
        let root = tempfile::tempdir().unwrap();
        let overlay = root.path().join("overlay");
        project(root.path(), &format!("visual_banks = [\"{path}\"]"));
        let digest = sheet(&overlay, 1);
        bank(&overlay, path, &digest, 2);
        reject_both(
            root.path(),
            Some(&overlay),
            "worktree visual bank is missing",
        );
    }
    let root = tempfile::tempdir().unwrap();
    let overlay = root.path().join("overlay");
    project(root.path(), "");
    let digest = sheet(&overlay, 1);
    bank(&overlay, "assets/visual_bank.jsonl", &digest, 2);
    reject_both(root.path(), Some(&overlay), "refusing the overlay bank");
}

#[test]
fn redundant_sheet_entries_warn_and_preserve_unrelated_overlay_assets() {
    let root = tempfile::tempdir().unwrap();
    let overlay = root.path().join("overlay");
    let digest = sheet(&overlay, 1);
    project(
        root.path(),
        &format!(
            "visual_banks = [\"assets/extra.jsonl\"]\noverlay_assets = [\"assets/.eat/{digest}/color.png\", \"assets/loose.txt\"]"
        ),
    );
    bank(root.path(), "assets/extra.jsonl", &digest, 2);
    std::fs::write(overlay.join("assets/loose.txt"), "loose").unwrap();
    // A redundant declaration does not turn a closure dependency into overlay-only data.
    let directory = format!("assets/.eat/{digest}");
    std::fs::create_dir_all(root.path().join("assets/.eat")).unwrap();
    std::fs::rename(overlay.join(&directory), root.path().join(&directory)).unwrap();
    let checked = run(root.path(), &["asset-check"], Some(&overlay));
    assert!(crate::support::text(&checked).contains("already supplied by visual bank"));
    success(checked);
    success(run(
        root.path(),
        &["package", "--output", "dist"],
        Some(&overlay),
    ));
    assert!(
        root.path()
            .join("dist/assets/sample/assets/loose.txt")
            .is_file()
    );
}

#[test]
fn higher_priority_corruption_does_not_fall_back_to_worktree_sheets() {
    let root = tempfile::tempdir().unwrap();
    let overlay = root.path().join("overlay");
    project(root.path(), "visual_banks = [\"assets/extra.jsonl\"]");
    let digest = sheet(root.path(), 1);
    bank(root.path(), "assets/extra.jsonl", &digest, 2);
    let directory = overlay.join(format!("assets/.eat/{digest}"));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("color.png"), "invalid PNG").unwrap();
    reject_both(root.path(), Some(&overlay), "decoding");
}

#[test]
fn split_generations_and_malformed_banks_reject_both_commands() {
    let root = tempfile::tempdir().unwrap();
    let overlay = root.path().join("overlay");
    project(root.path(), "visual_banks = [\"assets/extra.jsonl\"]");
    let digest = sheet(root.path(), 1);
    bank(root.path(), "assets/extra.jsonl", &digest, 2);
    let path = root.path().join("assets/extra.jsonl");
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        original.replace("\"material\":false", "\"material\":true"),
    )
    .unwrap();
    let directory = overlay.join(format!("assets/.eat/{digest}"));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("material.png"), "material").unwrap();
    reject_both(root.path(), Some(&overlay), "split across asset roots");
    std::fs::write(&path, "not a bank\n").unwrap();
    reject_both(root.path(), Some(&overlay), "malformed or inconsistent");
    std::fs::write(&path, format!("{original}{original}")).unwrap();
    reject_both(root.path(), Some(&overlay), "malformed or inconsistent");
}

#[test]
fn declaring_a_bank_already_in_release_assets_deduplicates_it() {
    let root = tempfile::tempdir().unwrap();
    project(root.path(), "visual_banks = [\"assets/extra.jsonl\"]");
    let path = root.path().join("stick.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("assets/owned.txt", "assets");
    std::fs::write(path, text).unwrap();
    let digest = sheet(root.path(), 1);
    bank(root.path(), "assets/extra.jsonl", &digest, 2);
    success(run(root.path(), &["package", "--output", "dist"], None));
    let staged = manifest(root.path());
    let keys = staged["artifacts"]["assets"]["keys"].as_array().unwrap();
    assert_eq!(
        keys.iter()
            .filter(|key| key.as_str() == Some("sample/assets/extra.jsonl"))
            .count(),
        1
    );
}

#[test]
fn auxiliary_studio_surface_closure_includes_lighting_and_checks_geometry() {
    use sha2::{Digest, Sha256};
    let root = tempfile::tempdir().unwrap();
    let overlay = root.path().join("overlay");
    project(root.path(), "visual_banks = [\"assets/studio.jsonl\"]");
    let seed = sheet(&overlay, 1);
    let png = std::fs::read(overlay.join(format!("assets/.eat/{seed}/color.png"))).unwrap();
    let mut files = std::collections::BTreeMap::new();
    for name in [
        "albedo.png",
        "normal.png",
        "depth.png",
        "material.png",
        "painted.png",
        "radiance.png",
        "skin-albedo.png",
        "skin-diffuse.png",
        "skin-specular.png",
    ] {
        files.insert(name, png.clone());
    }
    files.insert("sheet.json", br#"{"rects":[[0,0,2,2,0,0,0]]}"#.to_vec());
    let listing: String = files
        .iter()
        .map(|(name, bytes)| format!("{:x}  {name}\n", Sha256::digest(bytes)))
        .collect();
    let digest = format!("{:x}", Sha256::digest(listing.as_bytes()));
    let directory = overlay.join(format!("assets/.eat/{digest}"));
    std::fs::create_dir_all(&directory).unwrap();
    for (name, bytes) in &files {
        std::fs::write(directory.join(name), bytes).unwrap();
    }
    let bank = serde_json::json!({
        "kind":"surface", "id":"relief", "dir":digest,
        "size":[2,2], "cell":[2,2], "pivot":[0.5,0.5], "skin_mask":true,
        "studio":{"skin":true,"radiance_range":16.0},
        "camera":{"ground_y":0.0,"bases":[[0.,0.,0.,1.,0.,0.,0.,1.,0.,0.,0.,-1.]]}
    });
    std::fs::write(
        root.path().join("assets/studio.jsonl"),
        bank.to_string() + "\n",
    )
    .unwrap();
    success(run(root.path(), &["asset-check"], Some(&overlay)));
    success(run(
        root.path(),
        &["package", "--output", "dist"],
        Some(&overlay),
    ));
    let staged = root
        .path()
        .join(format!("dist/assets/sample/assets/.eat/{digest}"));
    assert_eq!(std::fs::read_dir(&staged).unwrap().count(), files.len());
    for (name, bytes) in &files {
        assert_eq!(std::fs::read(staged.join(name)).unwrap(), *bytes);
    }
    std::fs::remove_dir_all(root.path().join("dist")).unwrap();
    std::fs::write(directory.join("sheet.json"), br#"{"rects":[]}"#).unwrap();
    reject_both(root.path(), Some(&overlay), "sheet.json has 0 rects");
}
