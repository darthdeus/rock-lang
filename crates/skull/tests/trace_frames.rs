use std::process::Command;

#[test]
fn final_report_is_opt_in_and_observes_mir_inlining() {
    const CHILD: &str = "ROCK_TRACE_FRAME_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let lua = crate::common::emit_module(
            "nobox value Wide { a: float, b: float, c: float, d: float, e: float }\n\
             fn leaf(w: Wide) -> float { w.a + w.b + w.c + w.d + w.e }\n\
             @inline fn bridge(w: Wide) -> float { leaf(w) + 2.0 }\n\
             fn run(x: float) -> float {\n\
                 let w = Wide { a: x, b: x + 1.0, c: x + 2.0, d: x + 3.0, e: x + 4.0 }\n\
                 let y = bridge(w)\n\
                 y + x\n\
             }\nfn main() { print(\"{run(1.0)}\") }\n",
        );
        std::fs::write(std::env::var_os(CHILD).expect("output path"), lua).expect("write Lua");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let report_path = temp.path().join("frames.txt");
    let baseline = temp.path().join("baseline.lua");
    let diagnostic = temp.path().join("diagnostic.lua");
    for (enabled, path) in [(false, &baseline), (true, &diagnostic)] {
        let mut command = Command::new(std::env::current_exe().expect("test binary"));
        command
            .args([
                "--exact",
                "trace_frames::final_report_is_opt_in_and_observes_mir_inlining",
                "--nocapture",
            ])
            .env(CHILD, path)
            .env_remove("ROCK_LUA_TRACE_FRAMES")
            .env("ROCK_LUA_TRACE_FRAME_REPORT", &report_path);
        if enabled {
            command.env("ROCK_LUA_TRACE_FRAMES", "1");
        }
        let output = command.output().expect("child test");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !enabled {
            assert!(!report_path.exists(), "diagnostic must be opt-in");
        }
    }
    assert_eq!(
        std::fs::read(baseline).unwrap(),
        std::fs::read(diagnostic).unwrap(),
        "diagnostic changed emitted Lua"
    );
    let report = std::fs::read_to_string(report_path).expect("opt-in report");
    assert!(
        report.contains("type `Wide`"),
        "wide type missing from report: {report}"
    );
    let start = report.find("function run_").expect("emitted run frame");
    let end = report[start + 1..]
        .find("\nfunction ")
        .map_or(report.len(), |at| start + 1 + at);
    let run = &report[start..end];
    assert!(run.contains("-> leaf_"), "{run}");
    assert!(
        !run.contains("-> bridge_"),
        "MIR-inlined bridge remains in call chain: {run}"
    );
    assert!(
        run.contains("5 scalar lanes") && run.contains("nobox"),
        "{run}"
    );
}
