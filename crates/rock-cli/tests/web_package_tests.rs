//! The `rock web` DELIVERY shapes: `--single-file` (one self-contained .html)
//! and `--itch` (a zip with index.html at the archive root).
//!
//! These are structural gates and run in the default profile -- they build but
//! never open a browser. The behavioural half (does the embedded page actually
//! boot, decode its fonts and images, and render from file://) is
//! `web_smoke_tests::web_chrome_single_file_boots_from_file_url`, which needs
//! real Chrome and stays in `-P full`.
//!
//! `games/match3` is the fixture for shape and `games/chesslings` for assets:
//! match3 stages only the engine fallback font, so a test that only used match3
//! would pass with asset embedding deleted entirely.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn rock_web(args: &[&str]) -> std::process::Output {
    let out = Command::new(rock_bin())
        .current_dir(repo_root())
        .arg("web")
        .args(args)
        .output()
        .expect("spawn rock web");
    assert!(
        out.status.success(),
        "rock web {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

// ---------------------------------------------------------------------------
// --single-file

#[test]
fn single_file_emits_one_html_with_no_external_references() {
    let dir = tempfile::tempdir().expect("tempdir");
    rock_web(&["games/match3", "--single-file", "--out", dir.path().to_str().unwrap()]);

    let html = dir.path().join("match3.html");
    assert!(html.is_file(), "match3.html not emitted");

    // The whole point: ONE file. A stray game.js or asset tree beside it means
    // the page is still reaching for something at runtime.
    let siblings: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read out dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(siblings, vec!["match3.html".to_string()], "expected exactly one artifact");

    let text = std::fs::read_to_string(&html).expect("read html");
    // No `src=`/`href=` at all: every one would be a network request, and under
    // file:// each is a hard failure rather than a slow load.
    let refs: Vec<&str> = text
        .match_indices("src=")
        .chain(text.match_indices("href="))
        .map(|(i, _)| &text[i..(i + 40).min(text.len())])
        .collect();
    assert!(refs.is_empty(), "single-file page still references external resources: {refs:?}");
    assert!(text.contains("__ROCK_WEB_ASSETS"), "no embedded asset table");
}

#[test]
fn single_file_embeds_every_staged_asset() {
    let dir = tempfile::tempdir().expect("tempdir");
    // chesslings stages real assets; match3 stages only the fallback font, so
    // this assertion needs the game that can actually lose something.
    let out = rock_web(&[
        "games/chesslings",
        "--single-file",
        "--out",
        dir.path().to_str().unwrap(),
    ]);
    let log = String::from_utf8_lossy(&out.stderr).to_string()
        + &String::from_utf8_lossy(&out.stdout);

    let embedded: usize = log
        .split("assets embedded")
        .next()
        .and_then(|s| s.rsplit(", ").next())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    assert!(embedded > 1, "expected chesslings to embed several assets, got {embedded}");

    // Cross-view: the count the build REPORTED must equal the number of keys in
    // the emitted table. A report alone cannot show the payload made it in.
    let text = std::fs::read_to_string(dir.path().join("chesslings.html")).expect("read");
    let table = text
        .split("var b64 = {")
        .nth(1)
        .and_then(|s| s.split("};").next())
        .expect("asset table");
    let keys = table.matches("\": \"").count();
    assert_eq!(keys, embedded, "reported {embedded} assets, table holds {keys}");
}

#[test]
fn directory_build_stages_external_asset_overlays() {
    let overlay = tempfile::tempdir().expect("overlay tempdir");
    let source = overlay.path().join("assets/overlay-web-sentinel.txt");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "from-overlay").unwrap();
    let out_dir = tempfile::tempdir().expect("web output");
    let overlays = std::env::join_paths([overlay.path()]).unwrap();

    let out = Command::new(rock_bin())
        .current_dir(repo_root())
        .env("ROCK_ASSET_OVERLAYS", overlays)
        .args([
            "web",
            "games/match3",
            "--out",
            out_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn rock web");
    assert!(
        out.status.success(),
        "rock web failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(
            out_dir.path().join("games/match3/assets/overlay-web-sentinel.txt")
        )
        .unwrap(),
        "from-overlay"
    );
}

#[test]
fn the_page_opens_exactly_the_two_scripts_it_means_to() {
    // NOT an escaping test: no game in the tree emits `</script`, so this
    // fixture cannot distinguish working escaping from none at all. Escaping is
    // gated where it can fail, on the pure function
    // (`web::tests::escape_script_close_*`).
    //
    // What this DOES gate is the page's own shape -- the asset table and the
    // program, and nothing else. A third tag would mean something started
    // emitting markup into the bundle.
    let dir = tempfile::tempdir().expect("tempdir");
    rock_web(&["games/match3", "--single-file", "--out", dir.path().to_str().unwrap()]);
    let text = std::fs::read_to_string(dir.path().join("match3.html")).expect("read");

    let body = text.split("<canvas").nth(1).expect("body");
    let lower = body.to_lowercase();
    assert_eq!(lower.matches("<script>").count(), 2, "unexpected <script> count");
    assert_eq!(lower.matches("</script>").count(), 2, "unexpected </script> count");
}

// ---------------------------------------------------------------------------
// --itch

/// Read the archive with Python's `zipfile`, an INDEPENDENT implementation of
/// the format -- a hand-rolled reader that mirrored the writer's assumptions
/// would agree with a malformed archive.
fn unzip_names(zip: &Path) -> Vec<String> {
    let out = Command::new("python3")
        .arg("-c")
        .arg(format!(
            "import zipfile,sys\n\
             z=zipfile.ZipFile({:?})\n\
             bad=z.testzip()\n\
             assert bad is None, 'corrupt entry: '+str(bad)\n\
             print('\\n'.join(z.namelist()))",
            zip.to_str().expect("utf8 path")
        ))
        .output()
        .expect("spawn python3");
    assert!(
        out.status.success(),
        "python zipfile rejected the archive:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).lines().map(|s| s.to_string()).collect()
}

#[test]
fn itch_zip_has_index_html_at_the_archive_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    rock_web(&["games/match3", "--itch", "--out", dir.path().to_str().unwrap()]);
    let zip = dir.path().join("match3.zip");
    assert!(zip.is_file(), "match3.zip not emitted");

    let names = unzip_names(&zip);
    // itch.io serves the archive root, so index.html must be AT the root -- not
    // nested under a directory, which is the usual way an HTML5 upload fails.
    assert!(
        names.iter().any(|n| n == "index.html"),
        "no root index.html; archive holds {names:?}"
    );
    assert!(names.iter().any(|n| n == "game.js"), "no game.js in {names:?}");
}

#[test]
fn itch_zip_round_trips_every_byte() {
    // CRC and sizes live in two places in a zip (local header and central
    // directory) and a reader may consult either, so "it opened" is not enough:
    // compare the extracted bytes against the source.
    let dir = tempfile::tempdir().expect("tempdir");
    let plain = dir.path().join("plain");
    rock_web(&["games/chesslings", "--out", plain.to_str().unwrap()]);
    rock_web(&["games/chesslings", "--itch", "--out", dir.path().to_str().unwrap()]);

    let zip = dir.path().join("chesslings.zip");
    let extract = dir.path().join("x");
    let out = Command::new("python3")
        .arg("-c")
        .arg(format!(
            "import zipfile\nzipfile.ZipFile({:?}).extractall({:?})",
            zip.to_str().unwrap(),
            extract.to_str().unwrap()
        ))
        .output()
        .expect("spawn python3");
    assert!(out.status.success(), "extract failed: {}", String::from_utf8_lossy(&out.stderr));

    let mut compared = 0usize;
    for entry in walkdir::WalkDir::new(&plain) {
        let entry = entry.expect("walk");
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(&plain).expect("under plain");
        let from_zip = extract.join(rel);
        assert!(from_zip.is_file(), "{} missing from the archive", rel.display());
        assert_eq!(
            std::fs::read(entry.path()).expect("src"),
            std::fs::read(&from_zip).expect("extracted"),
            "{} differs after a zip round trip",
            rel.display()
        );
        compared += 1;
    }
    assert!(compared > 5, "only {compared} files compared; the fixture staged nothing");
}

#[test]
fn itch_single_file_zip_is_one_root_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    rock_web(&[
        "games/match3",
        "--itch",
        "--single-file",
        "--out",
        dir.path().to_str().unwrap(),
    ]);
    let names = unzip_names(&dir.path().join("match3.zip"));
    assert_eq!(names, vec!["index.html".to_string()], "expected a one-entry archive");
}

// ---------------------------------------------------------------------------
// Rejections

#[test]
fn watch_rejects_a_packaging_flag() {
    // --watch serves a live directory; there is nothing to serve for a zip, and
    // rebuilding a base64 page per keystroke is not a dev loop. Reject rather
    // than silently ignoring one of the two.
    for flag in ["--single-file", "--itch"] {
        let out = Command::new(rock_bin())
            .current_dir(repo_root())
            .args(["web", "games/match3", "--watch", flag])
            .output()
            .expect("spawn");
        assert!(!out.status.success(), "web --watch {flag} should reject");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("--watch"),
            "rejection should name the conflict, got: {err}"
        );
    }
}
