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

#[test]
fn replay_report_preserves_v15_taps_and_rejects_corrupt_input_details() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.replay");
    let row = "a 0 67=1 - 1=1 1=1 - 67=1 - - - - - 1";
    let valid = REPLAY_V14_WITH_JOB_DECISIONS.replace("STONE_REPLAY 14", "STONE_REPLAY 15")
        .replace("j 0 flow 42 1", &format!("{row}\nj 0 flow 42 1"));
    let report = |text: &str| {
        std::fs::write(&path, text).unwrap();
        Command::new(env!("CARGO_BIN_EXE_rock"))
            .args(["replay-report", path.to_str().unwrap(), "--format", "json"])
            .output().unwrap()
    };
    let output = report(&valid);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let input = &json["top_frames"][0]["input"];
    assert_eq!(input["pressed_keys"], serde_json::json!(["g"]));
    assert_eq!(input["released_keys"], serde_json::json!(["g"]));
    assert_eq!(input["pressed_buttons"], serde_json::json!([1]));
    assert_eq!(input["released_buttons"], serde_json::json!([1]));
    for bad in [
        String::new(),
        format!("{row}\n{row}"),
        row.replacen("a 0", "a 1", 1),
        row.replacen("67=1", "67=2", 1),
        row.replacen("67=1", "67=1,67=1", 1),
        row.replacen("67=1", "zz=1", 1),
        row.replacen("1=1", "0=1", 1),
    ] {
        let output = report(&valid.replace(row, &bad));
        assert!(!output.status.success(), "accepted corrupt details: {bad}");
    }
}

#[test]
fn replay_report_validates_v16_image_publications() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("images.replay");
    let row = "u 0 5b5b2270222c2270756d70222c302c5b5d5d5d";
    let valid = REPLAY_V14_WITH_JOB_DECISIONS.replace("STONE_REPLAY 14", "STONE_REPLAY 16")
        .replace("j 0 flow 42 1", &format!("a 0 - - - - - - - - - - - 0\n{row}\nj 0 flow 42 1"));
    for (accepted, replacement) in [
        (true, row.to_string()), (false, String::new()),
        (false, format!("{row}\n{row}")), (false, row.replacen("u 0", "u 1", 1)),
        (false, "u 0 7b7d".to_string()), (false, row.replacen("u 0", "u +0", 1)),
    ] {
        std::fs::write(&path, valid.replace(row, &replacement)).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_rock"))
            .args(["replay-report", path.to_str().unwrap(), "--format", "json"])
            .output().unwrap();
        assert_eq!(output.status.success(), accepted, "{}", String::from_utf8_lossy(&output.stderr));
    }
}
