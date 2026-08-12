//! Watch coverage for embed_string inputs (docs/comptime-plan.md tier 1): an
//! edit to an embedded file is a source edit, even though no `.rock` file
//! changed -- the watcher's extension filter can't know these paths, so the
//! compiled program's read set feeds a shared watch set refreshed per build.

use crate::stone_harness::StoneHarness;
use std::time::Duration;

const QUIESCENT: Duration = Duration::from_millis(400);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

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
