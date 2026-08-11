//! Every rich-text rejection, one bounded run per subject.
//!
//! The rich path raises rather than dropping a glyph or a call: an unsupported
//! byte, a line control or a deferred world-overlay destination is a program bug, and the plain
//! path's silent no-op on a stale font handle is exactly what the rich boundary
//! is not allowed to inherit.
//!
//! A raise aborts the rest of that frame, so the first subject would mask every
//! later one. Each gets its own process (`RICH_REJECT=<subject>`), driven
//! through `games/testbed/rich_text.rock`'s reject arm. The frame loop's xpcall
//! catches the raise and the bounded runner exits non-zero on the count
//! (frame_error_gate.rs), so the assertion is: the message reached the log AND
//! the run failed.
//!
//! `an_uninjected_run_still_passes` is the control -- without it every arm below
//! would pass on a testbed that could not start at all.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// `rock frame games/testbed` for 3 frames, optionally naming a reject subject.
/// Returns (combined output, exit-status-success).
fn run_frame(subject: Option<&str>) -> (String, bool) {
    let out_dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(rock_bin());
    cmd.current_dir(repo_root())
        .args(["frame", "games/testbed", "-n", "3"])
        .arg("--output")
        .arg(out_dir.path().join("main.lua"))
        .env("ROCK_FRAME_TIMEOUT_SECS", "120");
    if let Some(s) = subject {
        cmd.env("RICH_REJECT", s);
    }
    let out = cmd.output().expect("spawn rock frame");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

fn assert_rejects(subject: &str, needle: &str) {
    let (text, ok) = run_frame(Some(subject));
    assert!(
        text.contains(needle),
        "{subject}: the rejection did not name its cause (wanted {needle:?}):\n{text}"
    );
    assert!(!ok, "{subject}: a rejected rich-text call must fail the run:\n{text}");
}

#[test]
fn a_newline_rejects() {
    assert_rejects("newline", "rich text is one line");
}

#[test]
fn a_carriage_return_rejects() {
    assert_rejects("carriage_return", "rich text is one line");
}

#[test]
fn a_tab_rejects() {
    assert_rejects("tab", "rich text is one line");
}

#[test]
fn an_unsupported_byte_rejects() {
    assert_rejects("unsupported_byte", "byte 127");
}

#[test]
fn a_stale_font_handle_rejects() {
    assert_rejects("stale_font", "stale font handle");
}

/// The family is validated for the CALL, not per span. An empty sequence names
/// a family just as a full one does, and per-span validation would wave it
/// through on the grounds that there is nothing to walk.
#[test]
fn a_stale_font_handle_rejects_for_an_empty_sequence() {
    assert_rejects("empty_stale_font", "stale font handle");
}

/// Synthesis is DECLARED per face. Serving `Regular` for an undeclared bold
/// would draw a line that measures right and reads wrong, and the family that
/// asked for bold would never find out -- so an undeclared face rejects.
#[test]
fn an_undeclared_bold_face_rejects() {
    assert_rejects("undeclared_bold", "declares no bold face");
}

#[test]
fn an_undeclared_italic_face_rejects() {
    assert_rejects("undeclared_italic", "declares no italic face");
}

/// Declaring one axis must not admit the combination.
#[test]
fn a_half_declared_bold_italic_face_rejects() {
    assert_rejects("undeclared_bold_italic", "declares no bold-italic face");
}

/// Resolution happens for the WHOLE sequence before anything is measured or
/// emitted, so an undeclared face in the last span cannot leave the earlier
/// spans drawn.
#[test]
fn an_undeclared_face_in_a_later_span_rejects() {
    assert_rejects("undeclared_late_span", "declares no italic face");
}

#[test]
fn a_deferred_world_overlay_draw_list_rejects() {
    assert_rejects("dl_world_overlay", "unsupported list World/Output");
}

/// The control. Without the injection the same command must pass, so the arms
/// above cannot be passing because `rock frame games/testbed` is broken.
#[test]
fn an_uninjected_run_still_passes() {
    let (text, ok) = run_frame(None);
    assert!(ok, "the uninjected control run must pass:\n{text}");
    assert!(
        !text.contains("frame(s) raised during run"),
        "the control raised, so the injected arms prove nothing:\n{text}"
    );
}
