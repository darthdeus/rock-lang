use std::fs;
use std::path::{Path, PathBuf};

fn rock_sources(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).expect("read MAG source directory") {
        let entry = entry.expect("read MAG source entry");
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) != Some("tests") {
                rock_sources(&path, out);
            }
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rock") {
            out.push(path);
        }
    }
}

#[test]
fn mag_has_no_runstate_projection_bridge_names() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let mut sources = Vec::new();
    rock_sources(&repo.join("mag"), &mut sources);
    let forbidden = ["run_projection_bind", "run_projection_capture", "MAG_BUILDING_RUN"];
    let mut findings = Vec::new();
    for source in sources {
        let text = fs::read_to_string(&source).expect("read MAG source");
        for name in forbidden {
            if text.contains(name) {
                findings.push(format!("{}: {name}", source.display()));
            }
        }
    }
    assert!(findings.is_empty(), "projection bridge census is nonzero:\n{}", findings.join("\n"));
}

#[test]
fn mag_has_one_typed_runstate_root_and_no_global_runstate_mirror() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let mut sources = Vec::new();
    rock_sources(&repo.join("mag"), &mut sources);
    let active_run_source = repo.join("mag/world_levels.rock").display().to_string();
    let mut runstate_globals = Vec::new();
    let mut active_run_defs = Vec::new();
    for source in sources {
        let text = fs::read_to_string(&source).expect("read MAG source");
        for (line_number, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("global ") && trimmed.contains("RunState") {
                runstate_globals.push(format!("{}:{}", source.display(), line_number + 1));
            }
            if trimmed.starts_with("global MAG_ACTIVE_RUN:") {
                active_run_defs.push(format!("{}:{}", source.display(), line_number + 1));
            }
        }
    }
    assert_eq!(
        runstate_globals.len(),
        1,
        "MAG must have exactly one typed RunState global, owned by MAG_ACTIVE_RUN"
    );
    assert!(
        runstate_globals[0].starts_with(&format!("{active_run_source}:")),
        "typed RunState global must be owned by world_levels.rock: {runstate_globals:?}"
    );
    assert_eq!(
        active_run_defs.len(),
        1,
        "MAG_ACTIVE_RUN must have exactly one production definition"
    );
    assert!(
        active_run_defs[0].starts_with(&format!("{active_run_source}:")),
        "MAG_ACTIVE_RUN definition must live in world_levels.rock: {active_run_defs:?}"
    );
}
