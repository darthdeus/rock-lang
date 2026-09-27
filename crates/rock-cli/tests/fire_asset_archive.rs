//! Fire's build script packs the staged release tree into the embedded asset
//! archive; the packer is compiled here from the same source file.

use std::path::Path;
use std::process::Command;

#[path = "../../fire/asset_archive.rs"]
mod asset_archive;

const FD_HELPER_ENV: &str = "ROCK_FIRE_ARCHIVE_FD_HELPER";

fn write_file(root: &Path, key: &str, bytes: &[u8]) {
    let path = root.join(key);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Re-entered below under a lowered descriptor limit; a no-op in the normal
/// suite run.
#[test]
fn fire_archive_fd_helper_process() {
    let Ok(dir) = std::env::var(FD_HELPER_ENV) else { return };
    let limit = libc::rlimit { rlim_cur: 64, rlim_max: 64 };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    match asset_archive::relative_files(Path::new(&dir)) {
        Ok(files) => println!("walked {} files", files.len()),
        Err(e) => println!("walk failed: {e}"),
    }
}

/// The eat cache stages one file per content-hash directory, so a real tree
/// has more directories than a default descriptor limit. Every one must be
/// packed.
#[test]
fn walk_sees_every_directory_beyond_the_descriptor_limit() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..300 {
        write_file(tmp.path(), &format!("mag/assets/.eat/{i:04}/world/f.png"), b"x");
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["fire_asset_archive::fire_archive_fd_helper_process", "--exact", "--nocapture"])
        .env(FD_HELPER_ENV, tmp.path())
        .output()
        .expect("spawn fd helper");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "helper failed:\n{stdout}\n{stderr}");
    assert!(stdout.contains("walked 300 files"), "helper output:\n{stdout}\n{stderr}");
}

#[cfg(unix)]
#[test]
fn unreadable_directory_fails_the_walk() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    write_file(tmp.path(), "mag/assets/sfx/ui/tab_01.ogg", b"x");
    let locked = tmp.path().join("mag/assets/sfx");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = asset_archive::relative_files(tmp.path());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = result.expect_err("an unreadable directory must fail the walk");
    assert!(error.contains("mag/assets/sfx"), "{error}");
}

#[test]
fn staged_tree_must_match_declared_keys() {
    let keys = |list: &[&str]| list.iter().map(|k| k.to_string()).collect::<Vec<_>>();
    let declared = keys(&["mag/assets/a.png", "mag/assets/sfx/b.ogg"]);
    asset_archive::check_declared_keys(&declared, &declared).unwrap();

    let missing = asset_archive::check_declared_keys(&keys(&["mag/assets/a.png"]), &declared).unwrap_err();
    assert!(missing.contains("1 declared keys missing") && missing.contains("mag/assets/sfx/b.ogg"), "{missing}");

    let staged = keys(&["mag/assets/a.png", "mag/assets/sfx/b.ogg", "mag/assets/stray.txt"]);
    let extra = asset_archive::check_declared_keys(&staged, &declared).unwrap_err();
    assert!(extra.contains("1 undeclared files") && extra.contains("mag/assets/stray.txt"), "{extra}");
}

#[test]
fn archive_packs_declared_keys_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    write_file(tmp.path(), "mag/a.png", b"AA");
    write_file(tmp.path(), "mag/sfx/b.ogg", b"BBB");
    let keys = asset_archive::relative_files(tmp.path()).unwrap();
    assert_eq!(keys, ["mag/a.png", "mag/sfx/b.ogg"]);

    let (archive, count, raw_total) = asset_archive::pack_assets(tmp.path(), &keys).unwrap();
    assert_eq!((count, raw_total), (2, 5));
    let mut expected = asset_archive::ARCHIVE_MAGIC.to_vec();
    expected.push(asset_archive::ARCHIVE_VERSION);
    expected.extend_from_slice(&2u32.to_le_bytes());
    for (key, data) in [("mag/a.png", &b"AA"[..]), ("mag/sfx/b.ogg", &b"BBB"[..])] {
        expected.extend_from_slice(&(key.len() as u32).to_le_bytes());
        expected.extend_from_slice(key.as_bytes());
        expected.extend_from_slice(&(data.len() as u32).to_le_bytes());
        expected.extend_from_slice(data);
    }
    assert_eq!(archive, expected);

    let missing = asset_archive::pack_assets(tmp.path(), &["mag/gone.png".to_string()]).unwrap_err();
    assert!(missing.contains("mag/gone.png"), "{missing}");
}
