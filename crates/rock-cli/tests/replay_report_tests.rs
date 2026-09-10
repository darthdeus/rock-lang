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

const REPLAY_V14_WITH_JOB_DECISIONS: &str = "STONE_REPLAY 14\n\
seed 42\n\
start 1700000000\n\
window 0x1p+10 0x1p+9\n\
game MAG\n\
source 6d6167\n\
revision 726576\n\
build 0123456789abcdef\n\
t 0x1p-6 0 0 0 0 - - - -\n\
j 0 flow 42 1\n\
j 0 flow 9007199254740991 0\n\
p 0 0 10000 3000 6000 1000";

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

#[test]
fn replay_report_accepts_v14_job_decision_rows() {
    let dir = tempfile::tempdir().unwrap();
    let replay = dir.path().join("mag-v14.replay");
    std::fs::write(&replay, REPLAY_V14_WITH_JOB_DECISIONS).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "replay-report",
            replay.to_str().unwrap(),
            "--top", "1",
            "--format", "json",
        ])
        .output()
        .expect("run v14 replay report");
    assert!(
        output.status.success(),
        "v14 job rows must parse: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn replay_report_rejects_malformed_v14_job_decision_rows() {
    for row in [
        "j 0 flow 0 1",
        "j 0 flow 9007199254740992 1",
        "j 0 flow -1 1",
        "j 0 flow 42 2",
        "j 1 flow 42 1",
        "j 0 flow 42 1 extra",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let replay = dir.path().join("malformed-v14.replay");
        let text = REPLAY_V14_WITH_JOB_DECISIONS.replace(
            "j 0 flow 42 1\n",
            &format!("{row}\n"),
        );
        std::fs::write(&replay, text).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_rock"))
            .args([
                "replay-report",
                replay.to_str().unwrap(),
                "--top", "1",
                "--format", "json",
            ])
            .output()
            .expect("run malformed v14 replay report");
        assert!(
            !output.status.success(),
            "malformed row `{row}` must fail"
        );
    }
}
