//! Watch coverage for embed_string inputs (docs/comptime-plan.md tier 1): an
//! edit to an embedded file is a source edit, even though no `.rock` file
//! changed -- the watcher's extension filter can't know these paths, so the
//! compiled program's read set feeds a shared watch set refreshed per build.

use crate::stone_harness::StoneHarness;
use std::time::Duration;

const QUIESCENT: Duration = Duration::from_millis(400);
// The event must arrive before the quiet-period clock starts. Allow a loaded
// host's compiler and watcher processes to run, with nextest as the outer cap.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(180);

#[test]
fn embedded_file_edit_triggers_reload_with_new_bytes() {
    let h = StoneHarness::spawn(&[
        ("stick.toml", "[package]\nname = \"embedwatch\"\n"),
        ("main.rock", "fn main() { print(embed_string(\"data.txt\")) }\n"),
        ("data.txt", "embed-watch-v1"),
    ]);
    let baseline = h.event_count();
    assert!(
        h.compiled_lua().contains("embed-watch-v1"),
        "control: the initial build embeds v1"
    );

    h.write_file("data.txt", "embed-watch-v2");
    let log = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
    assert!(!log.is_empty(), "an embedded-file edit must trigger a reload, got none");
    assert!(
        h.compiled_lua().contains("embed-watch-v2") && !h.compiled_lua().contains("embed-watch-v1"),
        "the rebuilt program must carry the edited file's bytes"
    );
}

// A stick.toml edit mid-session must re-resolve declared inputs: the session
// rebuild is the only cadence at which [build.vars] values move, so dropping
// the event leaves stale vars until an unrelated structural edit.
#[test]
fn stick_toml_edit_reresolves_declared_vars() {
    let h = StoneHarness::spawn(&[
        (
            "stick.toml",
            "[package]\nname = \"varwatch\"\n[build]\nchannels = [\"dev\"]\n[build.vars]\nGREETING = \"vars-v1\"\n",
        ),
        ("main.rock", "fn main() { print(build_var(\"GREETING\")) }\n"),
    ]);
    let baseline = h.event_count();
    assert!(h.compiled_lua().contains("vars-v1"), "control: the initial build carries v1");

    h.write_file(
        "stick.toml",
        "[package]\nname = \"varwatch\"\n[build]\nchannels = [\"dev\"]\n[build.vars]\nGREETING = \"vars-v2\"\n",
    );
    let log = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
    assert!(!log.is_empty(), "a stick.toml edit must trigger a rebuild, got none");
    assert!(
        h.compiled_lua().contains("vars-v2") && !h.compiled_lua().contains("vars-v1"),
        "the rebuilt program must carry the re-resolved var"
    );
}

// The production shape: every in-tree embed lives in a GLOBAL initializer
// (the cimgui prelude, mag's stamps). Global-init reads have no function
// Chunk, so a watch set fed only from chunks was green on the fixture above
// while `stone/prebuilt/cimgui/*.lua` edits produced no rebuild at all.
#[test]
fn global_initializer_embed_edit_triggers_reload_with_new_bytes() {
    let h = StoneHarness::spawn(&[
        ("stick.toml", "[package]\nname = \"globalembedwatch\"\n"),
        (
            "main.rock",
            "const BANNER: string = embed_string(\"banner.txt\")\nfn main() { print(BANNER) }\n",
        ),
        ("banner.txt", "global-embed-v1"),
    ]);
    let baseline = h.event_count();
    assert!(
        h.compiled_lua().contains("global-embed-v1"),
        "control: the initial build embeds v1"
    );

    h.write_file("banner.txt", "global-embed-v2");
    let log = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
    assert!(!log.is_empty(), "a global-init embed edit must trigger a reload, got none");
    assert!(
        h.compiled_lua().contains("global-embed-v2")
            && !h.compiled_lua().contains("global-embed-v1"),
        "the rebuilt program must carry the edited file's bytes"
    );
}
