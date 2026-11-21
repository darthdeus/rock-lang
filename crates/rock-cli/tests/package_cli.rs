use crate::support::{assert_mentions, failure, success, write_files};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
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

fn write_project(root: &Path, extra_release: &str) {
    write_files(
        root,
        &[
            ("main.rock", "fn main() {}\n"),
            ("assets/owned.txt", "owned asset"),
            (
                "stick.toml",
                &format!(
                    "[package]\nname = \"sample\"\n[release]\nassets = [\"assets/owned.txt\"]\n{extra_release}\n"
                ),
            ),
        ],
    );
}

fn project(extra_release: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    write_project(root.path(), extra_release);
    root
}

fn package(root: &Path, overlay: Option<&Path>) -> Output {
    run(root, &["package", "--output", "dist"], overlay)
}

fn asset_check(root: &Path, overlay: Option<&Path>) -> Output {
    run(root, &["asset-check"], overlay)
}

/// Releases the whole `assets` directory instead of the one owned file.
fn release_all_assets(root: &Path) {
    let path = root.join("stick.toml");
    let config = std::fs::read_to_string(&path)
        .unwrap()
        .replace("assets/owned.txt", "assets");
    std::fs::write(path, config).unwrap();
}

#[test]
fn asset_check_and_package_validate_the_same_declared_overlay_assets() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("project");
    let overlay = root.path().join("overlay");
    write_project(&source, "overlay_assets = [\"assets/selected.txt\"]");
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
    success(asset_check(&source, Some(&overlay)));
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

fn png(value: u8) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&[value; 16])
        .unwrap();
    bytes
}

fn write_sheet(root: &Path, files: &BTreeMap<&str, Vec<u8>>) -> String {
    let listing: String = files
        .iter()
        .map(|(name, bytes)| format!("{:x}  {name}\n", Sha256::digest(bytes)))
        .collect();
    let digest = format!("{:x}", Sha256::digest(listing.as_bytes()));
    let directory = root.join(format!("assets/.eat/{digest}"));
    std::fs::create_dir_all(&directory).unwrap();
    for (name, bytes) in files {
        std::fs::write(directory.join(name), bytes).unwrap();
    }
    digest
}

fn sheet(root: &Path, value: u8) -> String {
    write_sheet(root, &BTreeMap::from([("color.png", png(value))]))
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
    let check = asset_check(root, overlay);
    let diagnostic = diagnostics(&check);
    failure(check, expected);
    for existing in [false, true] {
        let output = root.join("dist");
        if existing {
            std::fs::create_dir_all(&output).unwrap();
            std::fs::write(output.join("sentinel"), "untouched").unwrap();
        }
        let package = package(root, overlay);
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
    let root = project(
        "visual_banks = [\"assets/extra.jsonl\", \"assets/extra.jsonl\", \"assets/other.jsonl\"]",
    );
    let overlay = root.path().join("overlay");
    let old = sheet(&overlay, 10);
    bank(root.path(), "assets/extra.jsonl", &old, 2);
    bank(root.path(), "assets/other.jsonl", &old, 2);
    // Unreferenced generations and loose overlay files must not be admitted.
    let stale = sheet(root.path(), 11);
    std::fs::write(overlay.join("assets/loose.txt"), "loose").unwrap();
    success(asset_check(root.path(), Some(&overlay)));
    success(package(root.path(), Some(&overlay)));
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
    success(package(root.path(), Some(&overlay)));
    assert!(!root.path().join("dist/assets").join(old_key).exists());
    assert!(
        root.path()
            .join(format!("dist/assets/sample/assets/.eat/{new}/color.png"))
            .is_file()
    );
}

#[test]
fn main_and_auxiliary_bank_closures_are_both_packaged() {
    let root = project("visual_banks = [\"assets/extra.jsonl\"]");
    let main = sheet(root.path(), 1);
    let extra = sheet(root.path(), 2);
    bank(root.path(), "assets/visual_bank.jsonl", &main, 2);
    bank(root.path(), "assets/extra.jsonl", &extra, 2);
    success(package(root.path(), None));
    for digest in [main, extra] {
        assert!(
            root.path()
                .join(format!("dist/assets/sample/assets/.eat/{digest}/color.png"))
                .is_file()
        );
    }
}

#[test]
fn every_bank_reference_is_verified_even_when_the_digest_is_shared() {
    let root = project("visual_banks = [\"assets/extra.jsonl\", \"assets/other.jsonl\"]");
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
        let root = project(&format!(
            "visual_banks = [\"{bank_path}\"]\noverlay_assets = [\"{overlay_path}\"]"
        ));
        let overlay = root.path().join("overlay");
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
        let root = project("visual_banks = []");
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
    let root = project("");
    release_all_assets(root.path());
    let digest = sheet(root.path(), 1);
    bank(root.path(), "assets/extra.jsonl", &digest, 2);
    let checked = asset_check(root.path(), None);
    assert_mentions(&checked, "not declared in [release].visual_banks");
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
        let root = project(&format!(
            "visual_banks = [{}]",
            serde_json::to_string(path).unwrap()
        ));
        reject_both(
            root.path(),
            None,
            "canonical project-relative assets/...jsonl",
        );
    }
    for path in ["assets/visual_bank.jsonl", "assets/extra.jsonl"] {
        let root = project(&format!("visual_banks = [\"{path}\"]"));
        let overlay = root.path().join("overlay");
        let digest = sheet(&overlay, 1);
        bank(&overlay, path, &digest, 2);
        reject_both(
            root.path(),
            Some(&overlay),
            "worktree visual bank is missing",
        );
    }
    let root = project("");
    let overlay = root.path().join("overlay");
    let digest = sheet(&overlay, 1);
    bank(&overlay, "assets/visual_bank.jsonl", &digest, 2);
    reject_both(root.path(), Some(&overlay), "refusing the overlay bank");
}

#[test]
fn redundant_sheet_entries_warn_and_preserve_unrelated_overlay_assets() {
    let root = tempfile::tempdir().unwrap();
    let overlay = root.path().join("overlay");
    let digest = sheet(&overlay, 1);
    write_project(
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
    let checked = asset_check(root.path(), Some(&overlay));
    assert_mentions(&checked, "already supplied by visual bank");
    success(checked);
    success(package(root.path(), Some(&overlay)));
    assert!(
        root.path()
            .join("dist/assets/sample/assets/loose.txt")
            .is_file()
    );
}

#[test]
fn split_generations_and_malformed_banks_reject_both_commands() {
    let root = project("visual_banks = [\"assets/extra.jsonl\"]");
    let overlay = root.path().join("overlay");
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
