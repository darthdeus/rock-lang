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
fn mag_has_no_aggregate_projection_bridge_names() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let mut sources = Vec::new();
    rock_sources(&repo.join("mag"), &mut sources);
    let forbidden = [
        ["run_projection", "_bind"].concat(),
        ["run_projection", "_capture"].concat(),
        ["MAG_BUILDING", "_RUN"].concat(),
    ];
    let mut findings = Vec::new();
    for source in sources {
        let text = fs::read_to_string(&source).expect("read MAG source");
        for name in &forbidden {
            if text.contains(name) {
                findings.push(format!("{}: {name}", source.display()));
            }
        }
    }
    assert!(findings.is_empty(), "projection bridge census is nonzero:\n{}", findings.join("\n"));
}

#[test]
fn mag_uses_direct_session_state_without_a_run_root_or_accessor() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let mut sources = Vec::new();
    rock_sources(&repo.join("mag"), &mut sources);
    let aggregate_type = ["Run", "State"].concat();
    let run_accessor = ["mag", "_run", "_state"].concat();
    let forbidden = [
        format!("struct {aggregate_type}"),
        format!("Scene<{aggregate_type}>"),
        ["MAG", "_ACTIVE", "_RUN"].concat(),
        ["mag", "_run", "_scene", "_for", "_access"].concat(),
        format!("fn {run_accessor}"),
        format!("{run_accessor}()"),
        ["mag", "_world", "_scene"].concat(),
        ["fn current", "_run"].concat(),
    ];
    let required = [
        "global player: Player",
        "global bank_gold: int",
        "global journal: [JournalLine]",
        "global history: [SnapshotHistoryNode]",
        "global spell_panel_open: bool",
        "global MAG_ACTIVE_WORLD: Scene<WorldState>?",
    ];
    let mut findings = Vec::new();
    let mut definitions = required.map(|_| Vec::new());
    for source in sources {
        let text = fs::read_to_string(&source).expect("read MAG source");
        for (line_number, line) in text.lines().enumerate() {
            for name in &forbidden {
                if line.contains(name) {
                    findings.push(format!("{}:{}: {name}", source.display(), line_number + 1));
                }
            }
            for (index, declaration) in required.iter().enumerate() {
                if line.trim_start().starts_with(declaration) {
                    definitions[index].push(format!("{}:{}", source.display(), line_number + 1));
                }
            }
        }
    }
    assert!(findings.is_empty(), "superseded run-root/accessor census is nonzero:\n{}", findings.join("\n"));
    for (declaration, found) in required.iter().zip(definitions) {
        assert_eq!(found.len(), 1, "`{declaration}` must have one production definition: {found:?}");
    }
}

#[test]
fn mag_player_queries_are_receiver_methods() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let source = fs::read_to_string(repo.join("mag/player.rock")).expect("read MAG player source");
    let required = [
        "fn Player.is_paralyzed(player)",
        "fn Player.effective_armor(player)",
        "fn Player.shaped_progress(player)",
        "fn Player.sprite(player)",
        "fn Player.stamina_regen_per_sec(player)",
        "fn Player.mana_per_sec(player)",
        "fn Player.tint(player)",
    ];
    for declaration in required {
        assert!(source.contains(declaration), "missing Player receiver declaration `{declaration}`");
    }
    let forbidden = [
        "fn player_is_paralyzed()",
        "fn player_effective_armor()",
        "fn player_shaped_progress()",
        "fn player_sprite()",
        "fn player_stamina_regen_per_sec()",
        "fn player_mana_per_sec()",
        "fn player_tint()",
    ];
    for declaration in forbidden {
        assert!(!source.contains(declaration), "prefix-only Player query remains `{declaration}`");
    }
}
