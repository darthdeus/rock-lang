use std::process::Command;

const REPLAY: &str = "STONE_REPLAY 12\n\
seed 42\n\
window 0x1p+10 0x1p+9\n\
game MAG\n\
source 6d6167\n\
build 0123456789abcdef\n\
t 0x1p-6 0 0 0 0 - - - -\n\
p 0 0 10000 3000 6000 1000\n\
t 0x1p-6 0 0 0 0 W - - -\n\
p 1 1 40000 28000 10000 2000\n\
t 0x1p-6 0 0 0 0 W 1 - -\n\
p 2 2 50000 12000 35000 3000\n\
t 0x1p-6 0 0 0 0 - - - -\n\
p 3 3 11000 4000 6000 1000";

#[test]
fn replay_report_cli_emits_ranked_machine_readable_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let replay = dir.path().join("mag.replay");
    std::fs::write(&replay, REPLAY).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "replay-report",
            replay.to_str().unwrap(),
            "--top", "2",
            "--context", "1",
            "--format", "json",
        ])
        .output()
        .expect("run replay report");
    assert!(
        output.status.success(),
        "report failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["format"], "rock.replay-report.v1");
    assert_eq!(report["replay"]["source"], "mag");
    assert_eq!(report["top_frames"][0]["ordinal"], 2);
    assert_eq!(report["top_frames"][0]["suggested_range"], "1-3");
    assert_eq!(report["slow_clusters"][0]["start"], 1);
    assert_eq!(report["slow_clusters"][0]["end"], 2);
}
